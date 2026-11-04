const std = @import("std");
const accounts = @import("accounts.zig");
const proxy = @import("proxy.zig");
const providers = @import("providers.zig");

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
