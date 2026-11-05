const std = @import("std");
const accounts = @import("accounts.zig");
const proxy = @import("proxy.zig");
const providers = @import("providers.zig");
const orb_quota = @import("orb_quota.zig");

// Re-export for server.zig
pub const initProxyPublic = proxy.init;
pub const getProxyHost = proxy.getHost;
pub const getProxyPort = proxy.getPort;
pub const buildZedPayload = providers.buildZedPayload;
pub const extractModelFromBody = providers.extractModelFromBody;

// ── Token management ──

pub fn getToken(allocator: std.mem.Allocator, acc: *accounts.Account) ![]const u8 {
    if (acc.jwt_token) |tok| {
        if (std.time.timestamp() < acc.jwt_exp - 60) return tok;
        allocator.free(tok);
        acc.jwt_token = null;
    }
    return fetchNewToken(allocator, acc);
}

fn fetchNewToken(allocator: std.mem.Allocator, acc: *accounts.Account) ![]const u8 {
    // Route the token request through the configured proxy when one is set,
    // matching the chat/model paths. Going direct here was the root cause of
    // intermittent "Token Refresh" failures in proxied environments.
    proxy.init(allocator);

    // The token endpoint rejects an empty body: it needs the organization id,
    // which comes from the account's own profile.
    const profile = fetchBillingUsage(allocator, acc) catch return error.TokenRefreshFailed;
    defer allocator.free(profile);
    const org_id = (try parseOrganizationId(allocator, profile)) orelse {
        std.debug.print("[zed] no organization id in users/me for uid {s}\n", .{acc.user_id});
        return error.TokenRefreshFailed;
    };
    defer allocator.free(org_id);
    const req_body = try std.fmt.allocPrint(allocator, "{{\"organization_id\":\"{s}\"}}", .{org_id});
    defer allocator.free(req_body);

    const body = if (proxy.getHost() != null)
        try fetchTokenViaProxy(allocator, acc, req_body)
    else
        try fetchTokenDirect(allocator, acc, req_body);
    defer allocator.free(body);

    const parsed = std.json.parseFromSlice(std.json.Value, allocator, body, .{}) catch return error.TokenRefreshFailed;
    defer parsed.deinit();

    if (parsed.value != .object) return error.TokenRefreshFailed;
    const token = switch (parsed.value.object.get("token") orelse return error.NoToken) {
        .string => |s| allocator.dupe(u8, s) catch return error.TokenRefreshFailed,
        else => return error.InvalidToken,
    };

    acc.jwt_exp = parseJwtExp(token) catch 0;
    acc.jwt_token = token;
    std.debug.print("[zed] token refreshed for uid {s}\n", .{acc.user_id});
    return token;
}

/// Pick the organization id from a `client/users/me` body: the default one,
/// else the personal one, else the first. Returns an owned string or null.
pub fn parseOrganizationId(allocator: std.mem.Allocator, body: []const u8) !?[]u8 {
    const parsed = std.json.parseFromSlice(std.json.Value, allocator, body, .{}) catch return null;
    defer parsed.deinit();
    if (parsed.value != .object) return null;
    const root = parsed.value.object;
    if (root.get("default_organization_id")) |v| {
        if (v == .string and v.string.len > 0) return try allocator.dupe(u8, v.string);
    }
    const orgs = root.get("organizations") orelse return null;
    if (orgs != .array) return null;
    var first: ?[]const u8 = null;
    for (orgs.array.items) |o| {
        if (o != .object) continue;
        const id = switch (o.object.get("id") orelse continue) {
            .string => |x| x,
            else => continue,
        };
        if (first == null) first = id;
        if (o.object.get("is_personal")) |p| {
            if (p == .bool and p.bool) return try allocator.dupe(u8, id);
        }
    }
    return if (first) |id| try allocator.dupe(u8, id) else null;
}

test "parseOrganizationId prefers default, then personal, then first" {
    const a = std.testing.allocator;
    const d = (try parseOrganizationId(a, "{\"default_organization_id\":\"o1\",\"organizations\":[{\"id\":\"o2\"}]}")).?;
    defer a.free(d);
    try std.testing.expectEqualStrings("o1", d);
    const p = (try parseOrganizationId(a, "{\"organizations\":[{\"id\":\"o2\"},{\"id\":\"o3\",\"is_personal\":true}]}")).?;
    defer a.free(p);
    try std.testing.expectEqualStrings("o3", p);
    const f = (try parseOrganizationId(a, "{\"organizations\":[{\"id\":\"o2\"}]}")).?;
    defer a.free(f);
    try std.testing.expectEqualStrings("o2", f);
    try std.testing.expect((try parseOrganizationId(a, "{}")) == null);
    try std.testing.expect((try parseOrganizationId(a, "nope")) == null);
}

/// Fetch an LLM token directly (no proxy). Returns an owned response body.
fn fetchTokenDirect(allocator: std.mem.Allocator, acc: *accounts.Account, req_body: []const u8) ![]const u8 {
    const auth_header = try std.fmt.allocPrint(allocator, "{s} {s}", .{ acc.user_id, acc.credential_json });
    defer allocator.free(auth_header);

    const system_id = try proxy.getSystemId(allocator);
    defer if (system_id) |id| allocator.free(id);

    var headers: [4]std.http.Header = undefined;
    var header_count: usize = 0;
    for ([_]std.http.Header{
        .{ .name = "authorization", .value = auth_header },
        .{ .name = "content-type", .value = "application/json" },
        .{ .name = "user-agent", .value = proxy.USER_AGENT },
    }) |h| {
        headers[header_count] = h;
        header_count += 1;
    }
    if (system_id) |id| {
        headers[header_count] = .{ .name = "x-zed-system-id", .value = id };
        header_count += 1;
    }

    var response_buf: std.io.Writer.Allocating = .init(allocator);
    errdefer response_buf.deinit();

    var client: std.http.Client = .{ .allocator = allocator };
    defer client.deinit();

    const result = client.fetch(.{
        .location = .{ .url = "https://cloud.zed.dev/client/llm_tokens" },
        .method = .POST,
        .payload = req_body,
        .response_writer = &response_buf.writer,
        .extra_headers = headers[0..header_count],
    }) catch |err| {
        std.debug.print("[zed] token fetch error: {}\n", .{err});
        return error.TokenRefreshFailed;
    };

    if (result.status != .ok) {
        std.debug.print("[zed] token fetch status: {d} body: {s}\n", .{ @intFromEnum(result.status), response_buf.written() });
        return error.TokenRefreshFailed;
    }
    return try response_buf.toOwnedSlice();
}

/// Fetch an LLM token via the curl proxy. Returns an owned response body.
fn fetchTokenViaProxy(allocator: std.mem.Allocator, acc: *accounts.Account, req_body: []const u8) ![]const u8 {
    const p_url = try std.fmt.allocPrint(allocator, "http://{s}:{d}", .{ proxy.getHost().?, proxy.getPort() });
    defer allocator.free(p_url);

    const auth_header = try std.fmt.allocPrint(allocator, "authorization: {s} {s}", .{ acc.user_id, acc.credential_json });
    defer allocator.free(auth_header);

    const system_id = try proxy.getSystemId(allocator);
    defer if (system_id) |id| allocator.free(id);
    const system_id_header: ?[]u8 = if (system_id) |id| try std.fmt.allocPrint(allocator, "x-zed-system-id: {s}", .{id}) else null;
    defer if (system_id_header) |h| allocator.free(h);

    var argv: std.ArrayList([]const u8) = .empty;
    defer argv.deinit(allocator);
    try argv.appendSlice(allocator, &.{
        "curl",                                    "-s",
        "-x",                                      p_url,
        "-X",                                      "POST",
        "https://cloud.zed.dev/client/llm_tokens", "-H",
        auth_header,                               "-H",
        "content-type: application/json",          "-H",
        proxy.USER_AGENT_HEADER,
    });
    if (system_id_header) |h| try argv.appendSlice(allocator, &.{ "-H", h });
    try argv.appendSlice(allocator, &.{ "-d", req_body, "--max-time", "15", "-w", "\n__HTTP_STATUS__%{http_code}" });

    const result = std.process.Child.run(.{
        .allocator = allocator,
        .argv = argv.items,
        .max_output_bytes = 1024 * 1024,
    }) catch return error.TokenRefreshFailed;
    defer allocator.free(result.stderr);

    if (result.term != .Exited or result.term.Exited != 0) {
        std.debug.print("[zed] token fetch (proxy) curl failed: {s}\n", .{result.stderr});
        allocator.free(result.stdout);
        return error.TokenRefreshFailed;
    }

    var response_body = result.stdout;
    var http_status: []const u8 = "unknown";
    if (std.mem.lastIndexOf(u8, result.stdout, "\n__HTTP_STATUS__")) |pos| {
        response_body = result.stdout[0..pos];
        http_status = result.stdout[pos + "\n__HTTP_STATUS__".len ..];
    }

    if (!std.mem.startsWith(u8, http_status, "2")) {
        std.debug.print("[zed] token fetch (proxy) status: {s} body: {s}\n", .{ http_status, response_body[0..@min(response_body.len, 300)] });
        allocator.free(result.stdout);
        return error.TokenRefreshFailed;
    }

    const owned = allocator.dupe(u8, response_body) catch {
        allocator.free(result.stdout);
        return error.TokenRefreshFailed;
    };
    allocator.free(result.stdout);
    return owned;
}

fn parseJwtExp(jwt: []const u8) !i64 {
    var parts = std.mem.splitScalar(u8, jwt, '.');
    _ = parts.next();
    const payload_b64 = parts.next() orelse return error.InvalidJwt;
    const decoder = std.base64.url_safe_no_pad.Decoder;
    const decoded_len = decoder.calcSizeForSlice(payload_b64) catch return error.InvalidJwt;
    const decoded = try std.heap.page_allocator.alloc(u8, decoded_len);
    defer std.heap.page_allocator.free(decoded);
    decoder.decode(decoded, payload_b64) catch return error.InvalidJwt;
    const parsed = try std.json.parseFromSlice(std.json.Value, std.heap.page_allocator, decoded, .{});
    defer parsed.deinit();
    return switch (parsed.value.object.get("exp") orelse return error.NoExp) {
        .integer => |i| i,
        else => error.InvalidExp,
    };
}

pub fn parseJwtClaims(allocator: std.mem.Allocator, jwt: []const u8) ![]const u8 {
    var parts = std.mem.splitScalar(u8, jwt, '.');
    _ = parts.next();
    const payload_b64 = parts.next() orelse return error.InvalidJwt;
    const decoder = std.base64.url_safe_no_pad.Decoder;
    const decoded_len = decoder.calcSizeForSlice(payload_b64) catch return error.InvalidJwt;
    const decoded = try allocator.alloc(u8, decoded_len);
    decoder.decode(decoded, payload_b64) catch {
        allocator.free(decoded);
        return error.InvalidJwt;
    };
    return decoded;
}

// ── Proxy endpoints (non-streaming) ──

pub fn proxyChatCompletions(allocator: std.mem.Allocator, acc: *accounts.Account, body: []const u8) ![]const u8 {
    return proxyChatCompletionsInner(allocator, acc, body) catch |err| {
        if (err == error.TokenExpired) {
            if (acc.jwt_token) |tok| {
                allocator.free(tok);
                acc.jwt_token = null;
            }
            std.debug.print("[zed] token expired, refreshing and retrying...\n", .{});
            return proxyChatCompletionsInner(allocator, acc, body);
        }
        return err;
    };
}

fn proxyChatCompletionsInner(allocator: std.mem.Allocator, acc: *accounts.Account, body: []const u8) ![]const u8 {
    const jwt = try getToken(allocator, acc);
    const model_name = try providers.extractModelFromBody(allocator, body);
    defer allocator.free(model_name);
    const model = providers.normalizeModelName(model_name);

    const payload = try providers.buildZedPayload(allocator, body, false);
    defer allocator.free(payload);

    const response = try proxy.sendToZed(allocator, jwt, payload);
    defer allocator.free(response);
    return try providers.convertToOpenAI(allocator, response, model);
}

pub fn proxyResponses(allocator: std.mem.Allocator, acc: *accounts.Account, body: []const u8) ![]const u8 {
    return proxyResponsesInner(allocator, acc, body) catch |err| {
        if (err == error.TokenExpired) {
            if (acc.jwt_token) |tok| {
                allocator.free(tok);
                acc.jwt_token = null;
            }
            std.debug.print("[zed] token expired, refreshing and retrying Responses request...\n", .{});
            return proxyResponsesInner(allocator, acc, body);
        }
        return err;
    };
}

fn proxyResponsesInner(allocator: std.mem.Allocator, acc: *accounts.Account, body: []const u8) ![]const u8 {
    const jwt = try getToken(allocator, acc);
    const model_name = try providers.extractModelFromBody(allocator, body);
    defer allocator.free(model_name);
    const model = providers.normalizeModelName(model_name);

    const payload = try providers.buildZedPayload(allocator, body, false);
    defer allocator.free(payload);

    const response = try proxy.sendToZed(allocator, jwt, payload);
    defer allocator.free(response);
    return try providers.convertToResponses(allocator, response, model);
}

pub fn proxyMessages(allocator: std.mem.Allocator, acc: *accounts.Account, body: []const u8) ![]const u8 {
    return proxyMessagesInner(allocator, acc, body) catch |err| {
        if (err == error.TokenExpired) {
            if (acc.jwt_token) |tok| {
                allocator.free(tok);
                acc.jwt_token = null;
            }
            std.debug.print("[zed] token expired, refreshing and retrying...\n", .{});
            return proxyMessagesInner(allocator, acc, body);
        }
        return err;
    };
}

fn proxyMessagesInner(allocator: std.mem.Allocator, acc: *accounts.Account, body: []const u8) ![]const u8 {
    const jwt = try getToken(allocator, acc);
    const model_name = try providers.extractModelFromBody(allocator, body);
    defer allocator.free(model_name);
    const model = providers.normalizeModelName(model_name);

    const payload = try providers.buildZedPayload(allocator, body, true);
    defer allocator.free(payload);

    const response = try proxy.sendToZed(allocator, jwt, payload);
    defer allocator.free(response);
    return try providers.convertToAnthropic(allocator, response, model);
}

// ── Models ──

pub fn fetchModels(allocator: std.mem.Allocator, acc: *accounts.Account) ![]const u8 {
    const jwt = try getToken(allocator, acc);
    const bearer = try std.fmt.allocPrint(allocator, "Bearer {s}", .{jwt});
    defer allocator.free(bearer);

    proxy.init(allocator);

    if (proxy.getHost()) |_| {
        return fetchModelsViaProxy(allocator, bearer);
    } else {
        return fetchModelsDirect(allocator, bearer);
    }
}

fn fetchModelsViaProxy(allocator: std.mem.Allocator, bearer: []const u8) ![]const u8 {
    const p_url = try std.fmt.allocPrint(allocator, "http://{s}:{d}", .{ proxy.getHost().?, proxy.getPort() });
    defer allocator.free(p_url);

    const auth_header = try std.fmt.allocPrint(allocator, "authorization: {s}", .{bearer});
    defer allocator.free(auth_header);

    const result = std.process.Child.run(.{
        .allocator = allocator,
        .argv = &.{
            "curl",                         "-s",
            "-x",                           p_url,
            "https://cloud.zed.dev/models", "-H",
            auth_header,                    "-H",
            proxy.ZED_VERSION_HEADER,       "-H",
            proxy.USER_AGENT_HEADER,        "--max-time",
            "15",
        },
        .max_output_bytes = 1024 * 1024,
    }) catch return error.UpstreamError;
    defer allocator.free(result.stderr);

    if (result.term != .Exited or result.term.Exited != 0) {
        allocator.free(result.stdout);
        return error.UpstreamError;
    }
    if (result.stdout.len == 0 or !std.mem.startsWith(u8, result.stdout, "{")) {
        allocator.free(result.stdout);
        return error.UpstreamError;
    }
    return result.stdout;
}

fn fetchModelsDirect(allocator: std.mem.Allocator, bearer: []const u8) ![]const u8 {
    var response_buf: std.io.Writer.Allocating = .init(allocator);
    errdefer response_buf.deinit();

    var client: std.http.Client = .{ .allocator = allocator };
    defer client.deinit();

    const result = client.fetch(.{
        .location = .{ .url = "https://cloud.zed.dev/models" },
        .method = .GET,
        .response_writer = &response_buf.writer,
        .extra_headers = &.{
            .{ .name = "authorization", .value = bearer },
            .{ .name = "x-zed-version", .value = proxy.ZED_VERSION },
            .{ .name = "user-agent", .value = proxy.USER_AGENT },
        },
    }) catch return error.UpstreamError;

    if (result.status != .ok) {
        return error.UpstreamError;
    }
    return try response_buf.toOwnedSlice();
}

// ── Orb credit balance ──

pub const OrbFetchError = error{ InvalidOrbLink, OrbTimeout, OrbRequestFailed, OrbParse };

const ORB_ORIGIN = "https://portal.withorb.com";

/// Chrome-like headers: the portal's WAF rejects obviously non-browser clients.
const ORB_HEADERS =
    "header = \"User-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36\"\n" ++
    "header = \"Accept: application/json, text/plain, */*\"\n" ++
    "header = \"Accept-Language: en-US,en;q=0.9\"\n" ++
    "header = \"Origin: https://portal.withorb.com\"\n" ++
    "header = \"Sec-Fetch-Site: same-origin\"\n" ++
    "header = \"Sec-Fetch-Mode: cors\"\n" ++
    "header = \"Sec-Fetch-Dest: empty\"\n" ++
    "header = \"X-Requested-With: XMLHttpRequest\"\n";

/// A value that can go into a quoted curl config line or header unchanged.
fn curlSafeValue(value: []const u8) bool {
    for (value) |byte| {
        if (byte < 0x20 or byte == 0x7f) return false;
        if (byte == '"' or byte == '\\') return false;
    }
    return true;
}

/// Customer and pricing-unit ids are interpolated into the ledger URL, so keep
/// them to an obviously safe character set.
fn orbIdSafe(id: []const u8) bool {
    if (id.len == 0 or id.len > 128) return false;
    for (id) |byte| {
        if (std.ascii.isAlphanumeric(byte)) continue;
        if (byte == '-' or byte == '_') continue;
        return false;
    }
    return true;
}

/// GET an Orb endpoint through curl with the request written to `--config -` on
/// stdin, so the bearer token in the URL never reaches the process argument
/// list. `status_error` selects which error a non-2xx response becomes.
fn orbCurlGet(
    allocator: std.mem.Allocator,
    url: []const u8,
    referer: []const u8,
    status_error: OrbFetchError,
) OrbFetchError![]const u8 {
    proxy.init(allocator);

    var config: std.io.Writer.Allocating = .init(allocator);
    defer config.deinit();
    const w = &config.writer;
    w.writeAll("silent\nshow-error\n") catch return error.OrbRequestFailed;
    w.print("url = \"{s}\"\n", .{url}) catch return error.OrbRequestFailed;
    w.writeAll("max-time = 10\nconnect-timeout = 10\n") catch return error.OrbRequestFailed;
    w.writeAll(ORB_HEADERS) catch return error.OrbRequestFailed;
    // A pasted link can contain characters that break the quoted config line;
    // labelling the same origin is enough of a referer in that case.
    const safe_referer = if (curlSafeValue(referer)) referer else ORB_ORIGIN ++ "/";
    w.print("header = \"Referer: {s}\"\n", .{safe_referer}) catch return error.OrbRequestFailed;
    if (proxy.getHost()) |host| {
        // The host comes from an env var; a quote or newline there would inject
        // another curl config line, so refuse instead of building one.
        if (!curlSafeValue(host)) return error.OrbRequestFailed;
        w.print("proxy = \"http://{s}:{d}\"\n", .{ host, proxy.getPort() }) catch return error.OrbRequestFailed;
    }
    w.writeAll("write-out = \"\\n__HTTP_STATUS__%{http_code}\"\n") catch return error.OrbRequestFailed;

    var child = std.process.Child.init(&.{ "curl", "--config", "-" }, allocator);
    child.stdin_behavior = .Pipe;
    child.stdout_behavior = .Pipe;
    child.stderr_behavior = .Pipe;
    child.spawn() catch return error.OrbRequestFailed;

    // Close stdin before reading: the request is small, and curl must see EOF
    // before it can finish and be collected.
    if (child.stdin) |stdin_file| {
        stdin_file.writeAll(config.written()) catch {
            _ = child.kill() catch {};
            return error.OrbRequestFailed;
        };
        stdin_file.close();
        child.stdin = null;
    }

    var stdout: std.ArrayList(u8) = .empty;
    defer stdout.deinit(allocator);
    var stderr: std.ArrayList(u8) = .empty;
    defer stderr.deinit(allocator);
    child.collectOutput(allocator, &stdout, &stderr, 1024 * 1024) catch {
        _ = child.kill() catch {};
        return error.OrbRequestFailed;
    };
    const term = child.wait() catch return error.OrbRequestFailed;
    switch (term) {
        .Exited => |code| {
            if (code == 28) return error.OrbTimeout;
            if (code != 0) return error.OrbRequestFailed;
        },
        else => return error.OrbRequestFailed,
    }

    var body: []const u8 = stdout.items;
    var status: []const u8 = "";
    if (std.mem.lastIndexOf(u8, body, "\n__HTTP_STATUS__")) |pos| {
        status = body[pos + "\n__HTTP_STATUS__".len ..];
        body = body[0..pos];
    }
    if (!std.mem.startsWith(u8, status, "2")) return status_error;

    const owned = allocator.dupe(u8, body) catch return error.OrbRequestFailed;
    return owned;
}

/// Fetch the current-period credit summary for an Orb portal link. Two
/// sequential GETs run inside a temporary arena so the returned Credit owns
/// copies of the two dates.
pub fn fetchOrbCredit(allocator: std.mem.Allocator, link: []const u8) OrbFetchError!orb_quota.Credit {
    const token = orb_quota.tokenFromLink(link) orelse return error.InvalidOrbLink;
    if (!curlSafeValue(token)) return error.InvalidOrbLink;

    var arena_state = std.heap.ArenaAllocator.init(allocator);
    defer arena_state.deinit();
    const arena = arena_state.allocator();

    // The token is inserted into two query strings; escape the characters that
    // would otherwise split the query or end it.
    const token_enc = orb_quota.encodeToken(arena, token) catch return error.OrbRequestFailed;

    const customer_url = std.fmt.allocPrint(arena, "{s}/api/v1/customer_from_link?token={s}", .{ ORB_ORIGIN, token_enc }) catch
        return error.OrbRequestFailed;
    const customer_body = try orbCurlGet(arena, customer_url, link, error.InvalidOrbLink);
    const customer_parsed = std.json.parseFromSlice(std.json.Value, arena, customer_body, .{}) catch return error.OrbParse;
    const customer = orb_quota.parseCustomer(customer_parsed.value) orelse return error.OrbParse;
    if (!orbIdSafe(customer.id) or !orbIdSafe(customer.pricing_unit_id)) return error.OrbParse;

    const ledger_url = std.fmt.allocPrint(
        arena,
        "{s}/api/v1/customers/{s}/ledger_summary?pricing_unit_id={s}&token={s}",
        .{ ORB_ORIGIN, customer.id, customer.pricing_unit_id, token_enc },
    ) catch return error.OrbRequestFailed;
    const ledger_body = try orbCurlGet(arena, ledger_url, link, error.OrbRequestFailed);
    const ledger_parsed = std.json.parseFromSlice(std.json.Value, arena, ledger_body, .{}) catch return error.OrbParse;
    const credit = orb_quota.parseLedger(ledger_parsed.value);
    if (!credit.ok) return error.OrbParse;
    return credit;
}

// ── Billing ──

pub fn fetchBillingUsage(allocator: std.mem.Allocator, acc: *accounts.Account) ![]const u8 {
    proxy.init(allocator);
    if (proxy.getHost() != null) return fetchBillingViaProxy(allocator, acc);
    return fetchBillingDirect(allocator, acc);
}

fn fetchBillingViaProxy(allocator: std.mem.Allocator, acc: *accounts.Account) ![]const u8 {
    const p_url = try std.fmt.allocPrint(allocator, "http://{s}:{d}", .{ proxy.getHost().?, proxy.getPort() });
    defer allocator.free(p_url);

    const auth_header = try std.fmt.allocPrint(allocator, "authorization: {s} {s}", .{ acc.user_id, acc.credential_json });
    defer allocator.free(auth_header);

    const result = std.process.Child.run(.{
        .allocator = allocator,
        .argv = &.{
            "curl",                                  "-s",
            "-x",                                    p_url,
            "https://cloud.zed.dev/client/users/me", "-H",
            auth_header,                             "-H",
            "accept: application/json",              "-H",
            "content-type: application/json",        "-H",
            proxy.USER_AGENT_HEADER,                 "--max-time",
            "15",                                    "-w",
            "\n__HTTP_STATUS__%{http_code}",
        },
        .max_output_bytes = 1024 * 1024,
    }) catch return error.BillingFetchFailed;
    defer allocator.free(result.stderr);

    if (result.term != .Exited or result.term.Exited != 0) {
        allocator.free(result.stdout);
        return error.BillingFetchFailed;
    }

    var response_body = result.stdout;
    var http_status: []const u8 = "unknown";
    if (std.mem.lastIndexOf(u8, result.stdout, "\n__HTTP_STATUS__")) |pos| {
        response_body = result.stdout[0..pos];
        http_status = result.stdout[pos + "\n__HTTP_STATUS__".len ..];
    }

    if (!std.mem.startsWith(u8, http_status, "2")) {
        std.debug.print("[zed] users/me (proxy) failed {s}: {s}\n", .{ http_status, response_body[0..@min(response_body.len, 300)] });
        allocator.free(result.stdout);
        return error.BillingFetchFailed;
    }

    const owned = allocator.dupe(u8, response_body) catch {
        allocator.free(result.stdout);
        return error.BillingFetchFailed;
    };
    allocator.free(result.stdout);
    return owned;
}

fn fetchBillingDirect(allocator: std.mem.Allocator, acc: *accounts.Account) ![]const u8 {
    const auth_header = try std.fmt.allocPrint(allocator, "{s} {s}", .{ acc.user_id, acc.credential_json });
    defer allocator.free(auth_header);

    var response_buf: std.io.Writer.Allocating = .init(allocator);
    errdefer response_buf.deinit();

    var client: std.http.Client = .{ .allocator = allocator };
    defer client.deinit();

    const result = try client.fetch(.{
        .location = .{ .url = "https://cloud.zed.dev/client/users/me" },
        .method = .GET,
        .response_writer = &response_buf.writer,
        .extra_headers = &.{
            .{ .name = "authorization", .value = auth_header },
            .{ .name = "accept", .value = "application/json" },
            .{ .name = "content-type", .value = "application/json" },
            .{ .name = "user-agent", .value = proxy.USER_AGENT },
        },
    });

    if (result.status != .ok) {
        const err_body = response_buf.written();
        std.debug.print("[zed] users/me failed {}: {s}\n", .{ result.status, err_body });
        return error.BillingFetchFailed;
    }
    return try response_buf.toOwnedSlice();
}
