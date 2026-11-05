const std = @import("std");
const accounts = @import("accounts.zig");
const auth = @import("auth.zig");
const zed = @import("zed.zig");
const proxy = @import("proxy.zig");
const providers = @import("providers.zig");
const account_status = @import("account_status.zig");
const orb_quota = @import("orb_quota.zig");
const catalog = @import("catalog.zig");
const stream = @import("stream.zig");
const socket = @import("socket.zig");
const gateway_auth = @import("gateway_auth.zig");
const zed_login = @import("zed_login.zig");
const web_ui = @embedFile("web_index_html");

// Explicit health probes are intentionally tiny. Passive quota checks never
// invoke a model; when the user asks for a real inference check, use the
// selected model with the cheapest reasoning level its provider accepts, and
// keep a modest margin above the upstream's observed 16-token output minimum.
const HEALTH_PROBE_MODEL = "gpt-5.6-luna";
const HEALTH_PROBE_MAX_OUTPUT_TOKENS: i64 = 32;

// Probe reasoning per provider. GPT-5.6 accepts "none" and is cheapest that
// way; GPT-5.5 is pinned to xhigh by the conversion layer, so sending a field
// would be misleading; Anthropic models take no effort field at all; Gemini
// rejects MINIMAL, and its low level is the cheapest one this proxy will send.
const ProbeEffort = struct {
    /// Value for the body's `reasoning_effort`, null to omit the field.
    send: ?[]const u8,
    /// What the upstream will actually run, for the response object.
    used: []const u8,
};

pub fn probeEffortFor(model: []const u8) ProbeEffort {
    const normalized = providers.normalizeModelName(model);
    const provider = providers.getProvider(normalized);
    if (std.mem.eql(u8, provider, "open_ai")) {
        if (std.mem.startsWith(u8, normalized, "gpt-5.5")) return .{ .send = null, .used = "xhigh" };
        return .{ .send = "none", .used = "none" };
    }
    if (std.mem.eql(u8, provider, "google")) return .{ .send = "low", .used = "low" };
    return .{ .send = null, .used = "" };
}

/// The output budget the upstream will really enforce for this probe. GPT-5.5
/// is pinned to xhigh, so the conversion layer lifts any requested budget to
/// its large-reasoning floor; Gemini requests carry no output limit at all.
pub fn probeOutputBudget(model: []const u8) ?i64 {
    if (std.mem.eql(u8, providers.getProvider(providers.normalizeModelName(model)), "google")) return null;
    if (std.mem.startsWith(u8, providers.normalizeModelName(model), "gpt-5.5")) return providers.GPT_REASONING_OUTPUT_FLOOR;
    return HEALTH_PROBE_MAX_OUTPUT_TOKENS;
}

/// The probe is sent through the Chat Completions path, so the body uses
/// top-level `reasoning_effort`. Providers that would refuse or ignore the
/// field do not receive it. `max_tokens`/`max_completion_tokens` stay tiny;
/// `probeOutputBudget` reports what the conversion layer will really enforce.
pub fn buildProbeBody(allocator: std.mem.Allocator, model: []const u8) ![]const u8 {
    const effort = probeEffortFor(model);
    if (effort.send) |value| {
        return std.fmt.allocPrint(
            allocator,
            "{{\"model\":\"{s}\",\"messages\":[{{\"role\":\"user\",\"content\":\"Reply only OK\"}}],\"reasoning_effort\":\"{s}\",\"max_tokens\":{d},\"max_completion_tokens\":{d},\"stream\":false}}",
            .{ model, value, HEALTH_PROBE_MAX_OUTPUT_TOKENS, HEALTH_PROBE_MAX_OUTPUT_TOKENS },
        );
    }
    return std.fmt.allocPrint(
        allocator,
        "{{\"model\":\"{s}\",\"messages\":[{{\"role\":\"user\",\"content\":\"Reply only OK\"}}],\"max_tokens\":{d},\"max_completion_tokens\":{d},\"stream\":false}}",
        .{ model, HEALTH_PROBE_MAX_OUTPUT_TOKENS, HEALTH_PROBE_MAX_OUTPUT_TOKENS },
    );
}

/// A probe accepts any slug the upstream catalog lists (plus the local `gpt-5.6`
/// alias), which is the same list `/v1/models` and the WebUI show.
pub fn isKnownProbeModel(slug: []const u8) bool {
    return catalog.has(slug) or std.mem.eql(u8, slug, catalog.SOL_ALIAS);
}

/// Match `models[].slug` (Zed's own catalog shape) or `data[].id` (the OpenAI
/// envelope), so either embedded list can back the probe selector.
pub fn catalogHasSlug(root: std.json.Value, slug: []const u8) bool {
    const lists = [_]struct { key: []const u8, field: []const u8 }{
        .{ .key = "models", .field = "slug" },
        .{ .key = "data", .field = "id" },
    };
    for (lists) |list| {
        const value = root.object.get(list.key) orelse continue;
        if (value != .array) continue;
        for (value.array.items) |model| {
            if (model != .object) continue;
            const entry = model.object.get(list.field) orelse continue;
            if (entry == .string and std.mem.eql(u8, entry.string, slug)) return true;
        }
    }
    return false;
}

var account_mgr: accounts.AccountManager = undefined;
var global_allocator: std.mem.Allocator = undefined;
var api_keys: [][]const u8 = &.{};

// Dynamic models cache
var cached_models_openai: ?[]const u8 = null;
var cached_models_time: i64 = 0;
const MODELS_CACHE_TTL: i64 = 3600; // 1 hour

pub fn run(allocator: std.mem.Allocator, host: []const u8, port: u16) !void {
    global_allocator = allocator;
    account_mgr = accounts.AccountManager.init(allocator);
    defer account_mgr.deinit();
    account_mgr.loadFromFile() catch {};

    // The cwd is already ZED_DATA_DIR when main set it, so "." is the data dir
    // for `api_key` as well as accounts.json.
    const keys_owned = try gateway_auth.loadKeys(allocator, ".");
    defer gateway_auth.freeKeys(allocator, keys_owned);
    api_keys = keys_owned;
    if (gateway_auth.bindPolicyError(host, api_keys.len)) |reason| {
        std.debug.print("[zed-proxy] {s}\n", .{reason});
        return error.MissingApiKeyForPublicBind;
    }

    // std.net has no hostname resolver in this codebase's dependency-free
    // build; "localhost" is the documented alias for the loopback default.
    const bind_host = if (std.ascii.eqlIgnoreCase(host, "localhost")) "127.0.0.1" else host;
    const addr = std.net.Address.parseIp(bind_host, port) catch |err| {
        std.debug.print("[zed-proxy] invalid ZED_API_HOST '{s}': {}\n", .{ host, err });
        return err;
    };

    std.debug.print("[zed-proxy] http://{s}:{d}\n[zed-proxy] {d} account(s) loaded\n[zed-proxy] auth: {s}\n", .{
        bind_host, port, account_mgr.list.items.len, if (api_keys.len > 0) "on" else "off",
    });

    proxy.init(allocator);
    if (proxy.getHost()) |proxy_host| {
        std.debug.print("[zed-proxy] proxy: {s}:{d}\n", .{ proxy_host, proxy.getPort() });
    } else {
        std.debug.print("[zed-proxy] proxy: none (set HTTPS_PROXY to use)\n", .{});
    }
    // On Windows SO_REUSEADDR allows two zed-proxy processes to bind the same
    // loopback port, which makes requests and logs land in different instances.
    // Fail the second start instead so one port always identifies one process.
    var tcp_server = try addr.listen(.{ .reuse_address = false });
    defer tcp_server.deinit();

    while (true) {
        const conn = tcp_server.accept() catch continue;
        const thread = std.Thread.spawn(.{}, handleConnection, .{conn.stream}) catch {
            conn.stream.close();
            continue;
        };
        thread.detach();
    }
}

fn handleConnection(conn_stream: std.net.Stream) void {
    defer conn_stream.close();

    var hdr_buf: [8192]u8 = undefined;
    var hdr_total: usize = 0;

    while (hdr_total < hdr_buf.len) {
        const n = socket.recv(conn_stream, hdr_buf[hdr_total..]) catch return;
        if (n == 0) return;
        hdr_total += n;
        if (std.mem.indexOf(u8, hdr_buf[0..hdr_total], "\r\n\r\n") != null) break;
    }

    const header_end = std.mem.indexOf(u8, hdr_buf[0..hdr_total], "\r\n\r\n") orelse return;
    const headers = hdr_buf[0..header_end];
    const body_in_hdr = hdr_buf[header_end + 4 .. hdr_total];

    const first_line_end = std.mem.indexOf(u8, headers, "\r\n") orelse return;
    const first_line = headers[0..first_line_end];
    var parts = std.mem.splitScalar(u8, first_line, ' ');
    const method = parts.next() orelse return;
    const full_path = parts.next() orelse return;
    const path = if (std.mem.indexOf(u8, full_path, "?")) |i| full_path[0..i] else full_path;

    // Authenticate before reading the body or touching any route/upstream.
    // OPTIONS stays open for browser preflight, which cannot carry credentials;
    // it never returns data.
    if (api_keys.len > 0 and gateway_auth.isProtectedPath(path) and !std.mem.eql(u8, method, "OPTIONS")) {
        if (!gateway_auth.authorized(api_keys, gateway_auth.bearerFromHeaders(headers))) {
            socket.writeResponse(conn_stream, 401, "{\"error\":{\"message\":\"missing or invalid API key\",\"type\":\"auth_error\"}}");
            return;
        }
    }

    var content_length: usize = 0;
    var header_lines = std.mem.splitSequence(u8, headers, "\r\n");
    while (header_lines.next()) |line| {
        if (std.ascii.startsWithIgnoreCase(line, "content-length:")) {
            const val = std.mem.trim(u8, line["content-length:".len..], " ");
            content_length = std.fmt.parseInt(usize, val, 10) catch 0;
        }
    }

    // Read body (up to 16MB). Reject oversized requests explicitly instead of
    // silently truncating JSON and forwarding a malformed provider request.
    const max_body = 16 * 1024 * 1024;
    if (content_length > max_body) {
        socket.writeResponse(conn_stream, 413, "{\"error\":{\"message\":\"request body exceeds 16 MiB\",\"type\":\"invalid_request_error\"}}");
        return;
    }
    const actual_len = content_length;
    var body: []const u8 = "";
    var body_alloc: ?[]u8 = null;
    defer if (body_alloc) |b| global_allocator.free(b);

    if (actual_len > 0) {
        const body_buf = global_allocator.alloc(u8, actual_len) catch {
            socket.writeResponse(conn_stream, 500, "{\"error\":\"body too large\"}");
            return;
        };
        body_alloc = body_buf;
        const already = @min(body_in_hdr.len, actual_len);
        @memcpy(body_buf[0..already], body_in_hdr[0..already]);
        var filled: usize = already;
        while (filled < actual_len) {
            const n = socket.recv(conn_stream, body_buf[filled..actual_len]) catch break;
            if (n == 0) break;
            filled += n;
        }
        body = body_buf[0..filled];
    }

    // Streaming proxy check
    const is_messages = std.mem.eql(u8, path, "/v1/messages") and std.mem.eql(u8, method, "POST");
    const is_completions = std.mem.eql(u8, path, "/v1/chat/completions") and std.mem.eql(u8, method, "POST");
    const is_responses = std.mem.eql(u8, path, "/v1/responses") and std.mem.eql(u8, method, "POST");

    if (is_messages or is_completions or is_responses) {
        providers.validateClientReasoningEffort(global_allocator, body, is_messages) catch |err| {
            if (err == error.UnsupportedReasoningEffort) {
                socket.writeResponse(conn_stream, 400, "{\"error\":{\"message\":\"Zed-hosted GPT-5.6 supports none, low, medium, high, and xhigh; max/minimal are not available on this upstream route\",\"type\":\"invalid_request_error\"}}");
            } else {
                socket.writeResponse(conn_stream, 400, "{\"error\":{\"message\":\"invalid JSON request body\",\"type\":\"invalid_request_error\"}}");
            }
            return;
        };
    }
    const wants_stream = (is_messages or is_completions or is_responses) and requestWantsStream(body);

    if (wants_stream) {
        const req_model_owned = providers.extractModelFromBody(global_allocator, body) catch null;
        defer if (req_model_owned) |value| global_allocator.free(value);
        const req_model = req_model_owned orelse "unknown";
        const has_thinking = std.mem.indexOf(u8, body, "\"thinking\"") != null or std.mem.indexOf(u8, body, "\"reasoning\"") != null;
        std.debug.print("[req] {s} {s} model={s} thinking={} body={d}bytes (stream)\n", .{ method, path, req_model, has_thinking, body.len });
        stream.handleStreamProxy(conn_stream, body, is_messages, is_responses, &account_mgr, global_allocator);
        return;
    }

    // Non-streaming route
    const response = route(method, path, body) catch |err| {
        std.debug.print("[zed-proxy] route error: {} for {s} {s}\n", .{ err, method, path });
        socket.writeResponse(conn_stream, 500, "{\"error\":\"internal error\"}");
        return;
    };
    defer if (response.allocated) global_allocator.free(response.body);
    socket.writeResponseWithType(conn_stream, response.status, response.body, response.content_type);
}

/// JSON whitespace and formatting are insignificant. Parsing the boolean
/// avoids routing a valid request to the non-streaming handler merely because
/// a client emitted tabs, newlines, or multiple spaces around `stream`.
fn requestWantsStream(body: []const u8) bool {
    const parsed = std.json.parseFromSlice(std.json.Value, global_allocator, body, .{}) catch return false;
    defer parsed.deinit();
    if (parsed.value != .object) return false;
    const stream_value = parsed.value.object.get("stream") orelse return false;
    return stream_value == .bool and stream_value.bool;
}

const Response = struct {
    status: u16,
    body: []const u8,
    content_type: []const u8 = "application/json",
    allocated: bool = false,
};

const ProxyProtocol = enum { openai_chat, openai_responses, anthropic };

fn route(method: []const u8, path: []const u8, body: []const u8) !Response {
    std.debug.print("[req] {s} {s} body={d}bytes\n", .{ method, path, body.len });

    if (std.mem.eql(u8, path, "/")) return .{ .status = 200, .body = web_ui, .content_type = "text/html; charset=utf-8" };
    // Unauthenticated liveness probe: must not disclose accounts or versions.
    if (std.mem.eql(u8, path, "/health") and std.mem.eql(u8, method, "GET"))
        return .{ .status = 200, .body = "{\"status\":\"ok\"}" };
    if (std.mem.eql(u8, path, "/v1/models") and std.mem.eql(u8, method, "GET"))
        return try handleModels();
    if (std.mem.eql(u8, path, "/api/event_logging/batch"))
        return .{ .status = 200, .body = "{\"status\":\"ok\"}" };
    if (std.mem.startsWith(u8, path, "/v1/messages/count_tokens"))
        return .{ .status = 200, .body = "{\"input_tokens\":0}" };
    if (std.mem.eql(u8, path, "/zed/accounts") and std.mem.eql(u8, method, "GET"))
        return try handleListAccounts();
    if (std.mem.eql(u8, path, "/zed/accounts/status") and std.mem.eql(u8, method, "GET"))
        return try handleAccountStatuses();
    if (std.mem.eql(u8, path, "/zed/accounts/health") and std.mem.eql(u8, method, "POST"))
        return try handleAccountHealth(body);
    if (std.mem.eql(u8, path, "/zed/accounts/orb") and std.mem.eql(u8, method, "POST"))
        return try handleAccountOrb(body);
    if (std.mem.eql(u8, path, "/zed/accounts/switch") and std.mem.eql(u8, method, "POST"))
        return handleSwitchAccount(body);
    if (std.mem.eql(u8, path, "/zed/usage") and std.mem.eql(u8, method, "GET"))
        return try handleUsage();
    if (std.mem.eql(u8, path, "/zed/billing") and std.mem.eql(u8, method, "GET"))
        return try handleBilling();
    if (std.mem.eql(u8, path, "/v1/chat/completions") and std.mem.eql(u8, method, "POST"))
        return try handleProxy(body, .openai_chat);
    if (std.mem.eql(u8, path, "/v1/responses") and std.mem.eql(u8, method, "POST"))
        return try handleProxy(body, .openai_responses);
    if (std.mem.eql(u8, path, "/v1/messages") and std.mem.eql(u8, method, "POST"))
        return try handleProxy(body, .anthropic);
    if (std.mem.eql(u8, path, "/zed/login") and std.mem.eql(u8, method, "POST"))
        return try handleLogin(body);
    if (std.mem.eql(u8, path, "/zed/login/complete") and std.mem.eql(u8, method, "POST"))
        return try handleLoginComplete(body);
    if (std.mem.eql(u8, path, "/zed/login/status") and std.mem.eql(u8, method, "GET"))
        return handleLoginStatus();
    if (std.mem.eql(u8, method, "OPTIONS"))
        return .{ .status = 200, .body = "" };
    return .{ .status = 404, .body = "{\"error\":\"not found\"}" };
}

// ── Non-streaming proxy with failover ──

fn handleProxy(body: []const u8, protocol: ProxyProtocol) !Response {
    if (account_mgr.list.items.len == 0) return .{ .status = 400, .body = "{\"error\":\"no account configured\"}" };

    var try_order: [64]usize = undefined;
    const count = account_mgr.buildTryOrder(&try_order);

    var last_err: anyerror = error.UpstreamError;
    for (try_order[0..count]) |acc_idx| {
        const acc = &account_mgr.list.items[acc_idx];
        const available = accounts.AccountManager.isAvailable(acc);
        if (!available)
            std.debug.print("[zed-proxy] account '{s}' is benched (retry in {d}s), trying anyway as last resort\n", .{ acc.name, acc.disabled_until - std.time.timestamp() });

        const result = switch (protocol) {
            .anthropic => zed.proxyMessages(global_allocator, acc, body),
            .openai_chat => zed.proxyChatCompletions(global_allocator, acc, body),
            .openai_responses => zed.proxyResponses(global_allocator, acc, body),
        };

        if (result) |data| {
            accounts.AccountManager.markSuccess(acc, 200);
            if (acc_idx != account_mgr.current) {
                std.debug.print("[zed-proxy] failover success: switched to '{s}'\n", .{acc.name});
                account_mgr.setCurrent(acc_idx);
            }
            return .{ .status = 200, .body = data, .allocated = true };
        } else |err| {
            last_err = err;
            const kind = failureKind(err);
            accounts.AccountManager.markFailure(acc, kind, statusForError(err));
            std.debug.print("[zed-proxy] account '{s}' failed: {} (kind={s}, benched {d}s)\n", .{ acc.name, err, @tagName(kind), @max(acc.disabled_until - std.time.timestamp(), 0) });
            // Auth/rate/upstream errors are worth failing over; anything else
            // (e.g. malformed request) will fail identically on every account.
            const should_failover = (err == error.TokenRefreshFailed or err == error.TokenExpired or err == error.UpstreamError or err == error.RateLimited);
            if (!should_failover) break;
        }
    }

    const status: u16 = switch (last_err) {
        error.TokenRefreshFailed => 401,
        error.TokenExpired => 401,
        error.RateLimited => 429,
        error.UpstreamError => 502,
        else => 500,
    };
    const msg = switch (last_err) {
        error.TokenRefreshFailed => "{\"error\":{\"message\":\"All accounts failed: token refresh failed (bad/expired credential or banned account)\",\"type\":\"auth_error\"}}",
        error.TokenExpired => "{\"error\":{\"message\":\"All accounts failed: token expired\",\"type\":\"auth_error\"}}",
        error.RateLimited => "{\"error\":{\"message\":\"All accounts failed: rate limited\",\"type\":\"rate_limit_error\"}}",
        error.UpstreamError => "{\"error\":{\"message\":\"All accounts failed: upstream error\",\"type\":\"upstream_error\"}}",
        else => "{\"error\":{\"message\":\"All accounts failed: internal error\",\"type\":\"server_error\"}}",
    };
    return .{ .status = status, .body = msg };
}

/// Classify a proxy error into a health FailureKind so the scheduler can pick a
/// sensible cooldown.
fn failureKind(err: anyerror) accounts.FailureKind {
    return switch (err) {
        error.TokenRefreshFailed, error.TokenExpired => .auth,
        error.RateLimited => .rate_limit,
        else => .transient,
    };
}

fn statusForError(err: anyerror) u16 {
    return switch (err) {
        error.TokenRefreshFailed, error.TokenExpired => 401,
        error.RateLimited => 429,
        error.UpstreamError => 502,
        else => 0,
    };
}

// ── Account handlers ──

fn handleListAccounts() !Response {
    const now = std.time.timestamp();
    var buf: std.io.Writer.Allocating = .init(global_allocator);
    errdefer buf.deinit();
    const w = &buf.writer;
    try w.writeAll("{\"accounts\":[");
    for (account_mgr.list.items, 0..) |acc, i| {
        if (i > 0) try w.writeAll(",");
        const healthy = now >= acc.disabled_until;
        const cooldown = if (healthy) @as(i64, 0) else acc.disabled_until - now;
        try w.writeAll("{\"name\":");
        try std.json.Stringify.encodeJsonString(acc.name, .{}, w);
        try w.writeAll(",\"user_id\":");
        try std.json.Stringify.encodeJsonString(acc.user_id, .{}, w);
        try w.print(",\"current\":{s},\"healthy\":{s},\"cooldown_s\":{d},\"consecutive_failures\":{d},\"last_status\":{d}}}", .{
            if (i == account_mgr.current) "true" else "false",
            if (healthy) "true" else "false",
            cooldown,
            acc.consecutive_failures,
            acc.last_status,
        });
    }
    try w.writeAll("],\"current\":");
    try std.json.Stringify.encodeJsonString(if (account_mgr.getCurrent()) |c| c.name else "", .{}, w);
    try w.writeAll("}");
    return .{ .status = 200, .body = try buf.toOwnedSlice(), .allocated = true };
}

fn writeOptionalInt(w: *std.io.Writer, value: ?i64) !void {
    if (value) |number| {
        try w.print("{d}", .{number});
    } else {
        try w.writeAll("null");
    }
}

fn writeOptionalString(w: *std.io.Writer, value: ?[]const u8) !void {
    if (value) |text| {
        try std.json.Stringify.encodeJsonString(text, .{}, w);
    } else {
        try w.writeAll("null");
    }
}

// ── Orb credit cache ──

const ORB_CACHE_TTL_S: i64 = 300;
const ORB_FAILURE_TTL_S: i64 = 60;

const OrbCacheEntry = struct {
    credit: orb_quota.Credit,
    fetched_at: i64,
};

// Guarded by orb_cache_mutex; the status route may run on many threads.
var orb_cache_mutex: std.Thread.Mutex = .{};
var orb_cache: std.StringHashMap(OrbCacheEntry) = undefined;
var orb_cache_ready = false;

fn orbCacheInit() void {
    if (orb_cache_ready) return;
    orb_cache = std.StringHashMap(OrbCacheEntry).init(global_allocator);
    orb_cache_ready = true;
}

/// The cached credit for an account, fetching when the entry is missing or
/// past its TTL. Failures are cached for a shorter time so a dead link is not
/// hammered by a status poll. The account name is duplicated into the cache.
fn orbCreditFor(acc: *const accounts.Account) ?orb_quota.Credit {
    const link = acc.orb_url orelse return null;
    const now = std.time.timestamp();

    orb_cache_mutex.lock();
    orbCacheInit();
    if (orb_cache.get(acc.name)) |entry| {
        const ttl = if (entry.credit.ok) ORB_CACHE_TTL_S else ORB_FAILURE_TTL_S;
        if (now - entry.fetched_at < ttl) {
            const cached = entry.credit;
            orb_cache_mutex.unlock();
            return cached;
        }
    }
    // The fetch is not under the lock: one slow portal response must not stall
    // status polls for other accounts. A duplicate fetch on a race only costs
    // one extra request.
    orb_cache_mutex.unlock();

    const credit = zed.fetchOrbCredit(global_allocator, link) catch |err| orb_quota.Credit{
        .ok = false,
        .error_code = switch (err) {
            error.InvalidOrbLink => "invalid_link",
            error.OrbTimeout => "timeout",
            error.OrbParse => "parse_error",
            else => "fetch_failed",
        },
    };

    orb_cache_mutex.lock();
    defer orb_cache_mutex.unlock();
    if (orb_cache.getPtr(acc.name)) |existing| {
        existing.* = .{ .credit = credit, .fetched_at = now };
    } else {
        const key = global_allocator.dupe(u8, acc.name) catch return credit;
        orb_cache.put(key, .{ .credit = credit, .fetched_at = now }) catch {
            global_allocator.free(key);
            return credit;
        };
    }
    return credit;
}

/// `nan`/`inf` are not valid JSON: a single one would break every client that
/// parses the status response, so the writer never emits them.
fn finite(value: f64) f64 {
    return if (std.math.isFinite(value)) value else 0;
}

fn writeOrbCredit(w: *std.io.Writer, credit: orb_quota.Credit) !void {
    if (!credit.ok) {
        try w.writeAll("{\"ok\":false,\"error\":");
        try std.json.Stringify.encodeJsonString(credit.error_code orelse "parse_error", .{}, w);
        try w.writeAll("}");
        return;
    }
    try w.print(
        "{{\"ok\":true,\"balance_usd\":{d:.4},\"cap_usd\":{d:.4},\"used_usd\":{d:.4},\"used_pct\":{d:.2},\"period_start\":",
        .{ finite(credit.balance_usd), finite(credit.cap_usd), finite(credit.used_usd), finite(credit.used_pct) },
    );
    try std.json.Stringify.encodeJsonString(credit.periodStart(), .{}, w);
    try w.writeAll(",\"resets_at\":");
    try std.json.Stringify.encodeJsonString(credit.resetsAt(), .{}, w);
    try w.print(",\"total_balance_usd\":{d:.4}}}", .{finite(credit.total_balance_usd)});
}

/// POST /zed/accounts/orb: `{"account":"name","url":"<link>"}`, where an empty
/// url clears the stored link. The link is never echoed back.
fn handleAccountOrb(body: []const u8) !Response {
    const parsed = std.json.parseFromSlice(std.json.Value, global_allocator, body, .{}) catch
        return .{ .status = 400, .body = "{\"error\":\"invalid json\"}" };
    defer parsed.deinit();
    if (parsed.value != .object)
        return .{ .status = 400, .body = "{\"error\":\"request body must be an object\"}" };

    const name = switch (parsed.value.object.get("account") orelse return .{ .status = 400, .body = "{\"error\":\"missing account\"}" }) {
        .string => |s| s,
        else => return .{ .status = 400, .body = "{\"error\":\"account must be a string\"}" },
    };
    const url = switch (parsed.value.object.get("url") orelse std.json.Value{ .string = "" }) {
        .string => |s| s,
        else => return .{ .status = 400, .body = "{\"error\":\"url must be a string\"}" },
    };

    if (url.len > 0) {
        orb_quota.validateLink(url) catch
            return .{ .status = 400, .body = "{\"error\":\"invalid orb link\"}" };
    }

    // Login rewrites accounts.json too; share its lock so the two writes
    // cannot interleave and drop each other's fields. The account pointer must
    // be resolved under the same lock: a concurrent login deinits and reloads
    // the manager, which frees the list this pointer would come from.
    finalize_mutex.lock();
    defer finalize_mutex.unlock();

    var target: ?*accounts.Account = null;
    for (account_mgr.list.items) |*acc| {
        if (std.mem.eql(u8, acc.name, name)) target = acc;
    }
    if (target == null) return .{ .status = 404, .body = "{\"error\":\"account not found\"}" };

    accounts.setOrbUrl(global_allocator, name, url) catch |err| switch (err) {
        error.AccountNotFound => return .{ .status = 404, .body = "{\"error\":\"account not found\"}" },
        else => return .{ .status = 500, .body = "{\"error\":\"failed to save orb link\"}" },
    };
    // Update the live entry in place; a full manager reload here would race
    // with request threads holding pointers into the old list.
    const acc = target.?;
    if (url.len == 0) {
        acc.orb_url = null;
    } else {
        acc.orb_url = account_mgr.arena.allocator().dupe(u8, url) catch acc.orb_url;
    }

    // Drop any cached credit for this account so the next status poll reflects
    // the new link instead of the previous one.
    orb_cache_mutex.lock();
    orbCacheInit();
    if (orb_cache.fetchRemove(name)) |removed| global_allocator.free(removed.key);
    orb_cache_mutex.unlock();

    return .{ .status = 200, .body = "{\"success\":true}" };
}

/// Check every configured account without exposing credentials or JWTs. A
/// successful LLM-token refresh proves that the account can authenticate;
/// billing data then provides the best non-consuming quota signal available.
fn handleAccountStatuses() !Response {
    const now = std.time.timestamp();
    var output: std.io.Writer.Allocating = .init(global_allocator);
    errdefer output.deinit();
    const w = &output.writer;

    try w.print("{{\"checked_at\":{d},\"accounts\":[", .{now});
    for (account_mgr.list.items, 0..) |*acc, index| {
        if (index > 0) try w.writeAll(",");
        const scheduler_healthy = accounts.AccountManager.isAvailable(acc);
        const cooldown = if (scheduler_healthy) @as(i64, 0) else @max(@as(i64, 0), acc.disabled_until - now);

        try w.writeAll("{\"name\":");
        try std.json.Stringify.encodeJsonString(acc.name, .{}, w);
        try w.writeAll(",\"user_id\":");
        try std.json.Stringify.encodeJsonString(acc.user_id, .{}, w);
        try w.print(",\"current\":{s},\"scheduler_healthy\":{s},\"cooldown_s\":{d},\"last_status\":{d}", .{
            if (index == account_mgr.current) "true" else "false",
            if (scheduler_healthy) "true" else "false",
            cooldown,
            acc.last_status,
        });
        try w.print(",\"model_state\":\"{s}\",\"model_ok\":{s},\"model_checked_at\":{d},\"model_latency_ms\":{d}", .{
            @tagName(acc.last_probe_state),
            if (acc.last_probe_state == .healthy) "true" else "false",
            acc.last_probe_at,
            acc.last_probe_latency_ms,
        });

        // Orb credit is fetched sequentially, one configured account at a
        // time, and served from the per-account cache for 5 minutes.
        try w.print(",\"orb_configured\":{s}", .{if (acc.orb_url != null) "true" else "false"});
        if (orbCreditFor(acc)) |credit| {
            try w.writeAll(",\"credit\":");
            try writeOrbCredit(w, credit);
        }

        const token: ?[]const u8 = zed.getToken(global_allocator, acc) catch null;
        if (token == null) {
            try w.writeAll(",\"check_ok\":false,\"token_ok\":false,\"billing_ok\":false,\"usable\":false,\"quota_state\":\"unavailable\",\"error\":\"llm_token_refresh_failed\"}");
            continue;
        }

        const billing_result = zed.fetchBillingUsage(global_allocator, acc);
        if (billing_result) |billing| {
            defer global_allocator.free(billing);
            const parsed = std.json.parseFromSlice(std.json.Value, global_allocator, billing, .{}) catch {
                try w.writeAll(",\"check_ok\":false,\"token_ok\":true,\"billing_ok\":false,\"usable\":true,\"quota_state\":\"unknown\",\"error\":\"invalid_billing_response\"}");
                continue;
            };
            defer parsed.deinit();

            const summary = account_status.summarize(parsed.value);
            const state = account_status.quotaState(summary);
            try w.print(",\"check_ok\":true,\"token_ok\":true,\"billing_ok\":true,\"usable\":{s},\"quota_state\":\"{s}\",\"plan\":", .{
                if (account_status.isUsable(summary)) "true" else "false",
                state,
            });
            try std.json.Stringify.encodeJsonString(summary.plan, .{}, w);
            try w.writeAll(",\"used\":");
            try writeOptionalInt(w, summary.used);
            try w.writeAll(",\"limit\":");
            try writeOptionalInt(w, summary.limit);
            try w.writeAll(",\"remaining\":");
            try writeOptionalInt(w, account_status.remaining(summary));
            try w.writeAll(",\"subscription_ends_at\":");
            try writeOptionalString(w, summary.subscription_ends_at);
            try w.print(",\"usage_based_billing\":{s},\"overdue\":{s},\"account_too_young\":{s}}}", .{
                if (summary.usage_based_billing) "true" else "false",
                if (summary.overdue) "true" else "false",
                if (summary.account_too_young) "true" else "false",
            });
        } else |_| {
            // The LLM token is valid, so model access may still work even when
            // the optional billing endpoint is temporarily unavailable.
            try w.writeAll(",\"check_ok\":false,\"token_ok\":true,\"billing_ok\":false,\"usable\":true,\"quota_state\":\"unknown\",\"error\":\"billing_check_failed\"}");
        }
    }
    try w.writeAll("]}");
    return .{ .status = 200, .body = try output.toOwnedSlice(), .allocated = true };
}

/// Run a real, account-specific inference probe. This bypasses failover on
/// purpose: a healthy second account must not hide a broken selected account.
fn runAccountHealthProbe(acc: *accounts.Account, model: []const u8) void {
    const probe_body = buildProbeBody(global_allocator, model) catch {
        accounts.AccountManager.markProbeFailure(acc, .upstream_error, .transient, 500, 0);
        return;
    };
    defer global_allocator.free(probe_body);

    const started_ms = std.time.milliTimestamp();
    const result: anyerror![]const u8 = zed.proxyChatCompletions(global_allocator, acc, probe_body);
    const latency_ms = @max(@as(i64, 0), std.time.milliTimestamp() - started_ms);

    if (result) |response| {
        const has_text = probeResponseHasText(response);
        global_allocator.free(response);
        if (!has_text) {
            accounts.AccountManager.markProbeFailure(acc, .upstream_error, .transient, 502, latency_ms);
            std.debug.print("[health] account '{s}' probe returned no visible text ({d}ms)\n", .{ acc.name, latency_ms });
            return;
        }
        accounts.AccountManager.markProbeSuccess(acc, latency_ms);
        return;
    } else |err| {
        const state: accounts.ProbeState = switch (err) {
            error.TokenRefreshFailed, error.TokenExpired => .auth_error,
            error.RateLimited => .rate_limited,
            else => .upstream_error,
        };
        accounts.AccountManager.markProbeFailure(acc, state, failureKind(err), statusForError(err), latency_ms);
        std.debug.print("[health] account '{s}' probe failed: {} ({d}ms)\n", .{ acc.name, err, latency_ms });
    }
}

/// A 2xx envelope with an empty choice is not a useful health signal. Require
/// visible assistant text so the probe verifies the complete inference path.
fn probeResponseHasText(response: []const u8) bool {
    const parsed = std.json.parseFromSlice(std.json.Value, global_allocator, response, .{}) catch return false;
    defer parsed.deinit();
    if (parsed.value != .object) return false;
    const choices = parsed.value.object.get("choices") orelse return false;
    if (choices != .array or choices.array.items.len == 0) return false;
    const choice = choices.array.items[0];
    if (choice != .object) return false;
    const message = choice.object.get("message") orelse return false;
    if (message != .object) return false;
    const content = message.object.get("content") orelse return false;
    return content == .string and std.mem.trim(u8, content.string, " \t\r\n").len > 0;
}

fn writeAccountHealthResult(w: *std.io.Writer, acc: *const accounts.Account, now: i64) !void {
    const scheduler_healthy = accounts.AccountManager.isAvailable(acc);
    const cooldown = if (scheduler_healthy) @as(i64, 0) else @max(@as(i64, 0), acc.disabled_until - now);

    try w.writeAll("{\"name\":");
    try std.json.Stringify.encodeJsonString(acc.name, .{}, w);
    try w.print(",\"model_ok\":{s},\"model_state\":\"{s}\",\"model_checked_at\":{d},\"model_latency_ms\":{d},\"scheduler_healthy\":{s},\"cooldown_s\":{d},\"last_status\":{d}}}", .{
        if (acc.last_probe_state == .healthy) "true" else "false",
        @tagName(acc.last_probe_state),
        acc.last_probe_at,
        acc.last_probe_latency_ms,
        if (scheduler_healthy) "true" else "false",
        cooldown,
        acc.last_status,
    });
}

/// POST body may be `{}` for all accounts or `{"account":"name"}` for one,
/// each with an optional `"model":"<slug>"`. The response contains only
/// diagnostics and never returns credentials/JWTs.
fn handleAccountHealth(body: []const u8) !Response {
    var target_owned: ?[]const u8 = null;
    defer if (target_owned) |target| global_allocator.free(target);
    var model_owned: ?[]const u8 = null;
    defer if (model_owned) |owned| global_allocator.free(owned);
    var model: []const u8 = HEALTH_PROBE_MODEL;

    if (body.len > 0) {
        const parsed = std.json.parseFromSlice(std.json.Value, global_allocator, body, .{}) catch
            return .{ .status = 400, .body = "{\"error\":\"invalid json\"}" };
        defer parsed.deinit();
        if (parsed.value != .object)
            return .{ .status = 400, .body = "{\"error\":\"request body must be an object\"}" };
        if (parsed.value.object.get("account")) |account_value| {
            if (account_value != .string)
                return .{ .status = 400, .body = "{\"error\":\"account must be a string\"}" };
            target_owned = try global_allocator.dupe(u8, account_value.string);
        }
        if (parsed.value.object.get("model")) |model_value| {
            if (model_value != .string)
                return .{ .status = 400, .body = "{\"error\":\"model must be a string\"}" };
            if (!isKnownProbeModel(model_value.string))
                return .{ .status = 400, .body = "{\"error\":\"unknown model\"}" };
            // Keep the choice alive past `parsed`.
            model_owned = try global_allocator.dupe(u8, model_value.string);
            model = model_owned.?;
        }
    }

    if (target_owned) |target| {
        var found = false;
        for (account_mgr.list.items) |*acc| {
            if (!std.mem.eql(u8, acc.name, target)) continue;
            found = true;
            runAccountHealthProbe(acc, model);
            break;
        }
        if (!found) return .{ .status = 404, .body = "{\"error\":\"account not found\"}" };
    } else {
        // Sequential checks keep resource use predictable and avoid sending a
        // burst of paid model requests when many accounts are configured.
        for (account_mgr.list.items) |*acc| runAccountHealthProbe(acc, model);
    }

    const now = std.time.timestamp();
    var output: std.io.Writer.Allocating = .init(global_allocator);
    errdefer output.deinit();
    const w = &output.writer;
    try w.print("{{\"checked_at\":{d},\"probe\":{{\"model\":\"{s}\",\"reasoning_effort\":", .{ now, model });
    // Null means "the provider takes no effort knob", which is different from
    // a provider default the client should guess at.
    const effort_used = probeEffortFor(model).used;
    if (effort_used.len > 0) try std.json.Stringify.encodeJsonString(effort_used, .{}, w) else try w.writeAll("null");
    try w.writeAll(",\"max_output_tokens\":");
    if (probeOutputBudget(model)) |budget| try w.print("{d}", .{budget}) else try w.writeAll("null");
    try w.writeAll("},\"accounts\":[");
    var written: usize = 0;
    for (account_mgr.list.items) |*acc| {
        if (target_owned) |target| {
            if (!std.mem.eql(u8, acc.name, target)) continue;
        }
        if (written > 0) try w.writeAll(",");
        try writeAccountHealthResult(w, acc, now);
        written += 1;
    }
    try w.writeAll("]}");
    return .{ .status = 200, .body = try output.toOwnedSlice(), .allocated = true };
}

fn handleSwitchAccount(body: []const u8) Response {
    const parsed = std.json.parseFromSlice(std.json.Value, global_allocator, body, .{}) catch
        return .{ .status = 400, .body = "{\"error\":\"invalid json\"}" };
    defer parsed.deinit();
    const name = switch (parsed.value.object.get("account") orelse return .{ .status = 400, .body = "{\"error\":\"missing account\"}" }) {
        .string => |s| s,
        else => return .{ .status = 400, .body = "{\"error\":\"bad type\"}" },
    };
    if (account_mgr.switchTo(name))
        return .{ .status = 200, .body = "{\"success\":true}" }
    else
        return .{ .status = 404, .body = "{\"error\":\"not found\"}" };
}

fn handleUsage() !Response {
    const acc = account_mgr.getCurrent() orelse return .{ .status = 400, .body = "{\"error\":\"no account\"}" };
    const jwt = try zed.getToken(global_allocator, acc);
    const claims = try zed.parseJwtClaims(global_allocator, jwt);
    return .{ .status = 200, .body = claims, .allocated = true };
}

fn handleBilling() !Response {
    const acc = account_mgr.getCurrent() orelse return .{ .status = 400, .body = "{\"error\":\"no account\"}" };
    const user_info = zed.fetchBillingUsage(global_allocator, acc) catch {
        return .{ .status = 502, .body = "{\"error\":\"failed to fetch user info\"}" };
    };
    return .{ .status = 200, .body = user_info, .allocated = true };
}

fn handleModels() !Response {
    const now = std.time.timestamp();
    if (cached_models_openai) |cached| {
        if (now - cached_models_time < MODELS_CACHE_TTL) {
            return .{ .status = 200, .body = cached };
        }
    }

    // Fetch from Zed
    const acc = account_mgr.getCurrent() orelse {
        // Fallback to static
        return .{ .status = 200, .body = @embedFile("models.json") };
    };

    const raw = zed.fetchModels(global_allocator, acc) catch {
        // Fallback to cache or static
        if (cached_models_openai) |cached| return .{ .status = 200, .body = cached };
        return .{ .status = 200, .body = @embedFile("models.json") };
    };
    defer global_allocator.free(raw);
    catalog.refreshFromZed(raw);

    // Convert Zed format to OpenAI format
    const openai = convertZedModelsToOpenAI(global_allocator, raw) catch {
        if (cached_models_openai) |cached| return .{ .status = 200, .body = cached };
        return .{ .status = 200, .body = @embedFile("models.json") };
    };

    // A successful upstream response can still contain none of the aliases
    // this proxy intentionally exposes. Return the stable local catalog in
    // that case instead of caching an empty model list.
    if (std.mem.indexOf(u8, openai, "\"data\":[]") != null) {
        global_allocator.free(openai);
        return .{ .status = 200, .body = @embedFile("models.json") };
    }

    // Update cache
    if (cached_models_openai) |old| global_allocator.free(old);
    cached_models_openai = openai;
    cached_models_time = now;

    std.debug.print("[zed-proxy] models refreshed ({d} bytes)\n", .{openai.len});
    return .{ .status = 200, .body = openai };
}

const EffortLevel = struct { value: []const u8, name: []const u8, is_default: bool };

fn writeJsonString(w: *std.io.Writer, text: []const u8) !void {
    try std.json.Stringify.encodeJsonString(text, .{}, w);
}

/// The `supported_effort_levels` of one Zed model. Entries without a string
/// value are skipped.
fn collectEffortLevels(allocator: std.mem.Allocator, model: std.json.ObjectMap) ![]EffortLevel {
    var out: std.ArrayList(EffortLevel) = .empty;
    errdefer out.deinit(allocator);
    const list = model.get("supported_effort_levels") orelse return out.toOwnedSlice(allocator);
    if (list != .array) return out.toOwnedSlice(allocator);
    for (list.array.items) |item| {
        if (item != .object) continue;
        const value = switch (item.object.get("value") orelse continue) {
            .string => |s| s,
            else => continue,
        };
        const name = if (item.object.get("name")) |n| (if (n == .string) n.string else value) else value;
        const is_default = if (item.object.get("is_default")) |d| (d == .bool and d.bool) else false;
        try out.append(allocator, .{ .value = value, .name = name, .is_default = is_default });
    }
    return out.toOwnedSlice(allocator);
}

/// Gemini reports `LOW`, the others `low`; the catalog uses one spelling.
fn writeLowerJsonString(w: *std.io.Writer, allocator: std.mem.Allocator, text: []const u8) !void {
    const lower = try std.ascii.allocLowerString(allocator, text);
    defer allocator.free(lower);
    try writeJsonString(w, lower);
}

/// One entry of the Codex-style `models` catalog, built from a Zed model.
fn writeCodexModel(w: *std.io.Writer, allocator: std.mem.Allocator, slug: []const u8, model: std.json.ObjectMap, priority: usize) !void {
    const display = if (model.get("display_name")) |d| (if (d == .string) d.string else slug) else slug;
    const levels = try collectEffortLevels(allocator, model);
    defer allocator.free(levels);

    try w.writeAll("{\"slug\":");
    try writeJsonString(w, slug);
    try w.writeAll(",\"display_name\":");
    try writeJsonString(w, display);
    try w.writeAll(",\"description\":");
    const description = try std.fmt.allocPrint(allocator, "Zed - {s}", .{display});
    defer allocator.free(description);
    try writeJsonString(w, description);
    for (levels) |level| if (level.is_default) {
        try w.writeAll(",\"default_reasoning_level\":");
        try writeLowerJsonString(w, allocator, level.value);
        break;
    };
    try w.writeAll(",\"supported_reasoning_levels\":[");
    for (levels, 0..) |level, i| {
        if (i > 0) try w.writeAll(",");
        try w.writeAll("{\"effort\":");
        try writeLowerJsonString(w, allocator, level.value);
        try w.writeAll(",\"description\":");
        try writeJsonString(w, level.name);
        try w.writeAll("}");
    }
    try w.print("],\"shell_type\":\"shell_command\",\"visibility\":\"list\",\"supported_in_api\":true,\"priority\":{d}}}", .{priority});
}

fn isDisabledModel(model: std.json.ObjectMap) bool {
    const flag = model.get("is_disabled") orelse return false;
    return flag == .bool and flag.bool;
}

/// Zed's catalog as the OpenAI list plus the Codex `models` catalog, with
/// nothing filtered out except models Zed marks disabled. Zed lists only the
/// concrete GPT-5.6 variants, so the bare `gpt-5.6` alias for Sol is added.
fn convertZedModelsToOpenAI(allocator: std.mem.Allocator, raw: []const u8) ![]const u8 {
    const parsed = try std.json.parseFromSlice(std.json.Value, allocator, raw, .{});
    defer parsed.deinit();
    if (parsed.value != .object) return error.InvalidFormat;

    const models = switch (parsed.value.object.get("models") orelse return error.InvalidFormat) {
        .array => |a| a,
        else => return error.InvalidFormat,
    };

    var buf: std.io.Writer.Allocating = .init(allocator);
    errdefer buf.deinit();
    const w = &buf.writer;

    var sol: ?std.json.ObjectMap = null;
    var has_alias = false;
    for (models.items) |model| {
        if (model != .object) continue;
        const id = switch (model.object.get("id") orelse continue) {
            .string => |s| s,
            else => continue,
        };
        if (std.mem.eql(u8, id, catalog.SOL_ALIAS)) has_alias = true;
        if (std.mem.eql(u8, id, "gpt-5.6-sol")) sol = model.object;
    }
    const add_alias = sol != null and !has_alias;

    try w.writeAll("{\"object\":\"list\",\"data\":[");
    var first = true;
    if (add_alias) {
        try w.writeAll("{\"id\":\"gpt-5.6\",\"object\":\"model\",\"owned_by\":\"open_ai\"}");
        first = false;
    }
    for (models.items) |model| {
        if (model != .object) continue;
        const id = switch (model.object.get("id") orelse continue) {
            .string => |s| s,
            else => continue,
        };
        const provider = switch (model.object.get("provider") orelse continue) {
            .string => |s| s,
            else => continue,
        };
        if (isDisabledModel(model.object)) continue;

        if (!first) try w.writeAll(",");
        first = false;
        try w.writeAll("{\"id\":");
        try writeJsonString(w, id);
        try w.writeAll(",\"object\":\"model\",\"owned_by\":");
        try writeJsonString(w, provider);
        try w.writeAll("}");
    }

    // Codex Desktop's models manager requires a top-level "models" catalog
    // (it fails the whole turn with `missing field \`models\`` otherwise).
    // OpenAI-style clients ignore the extra field, so both formats coexist.
    try w.writeAll("],\"models\":[");
    var priority: usize = 1;
    first = true;
    if (add_alias) {
        try writeCodexModel(w, allocator, catalog.SOL_ALIAS, sol.?, priority);
        priority += 1;
        first = false;
    }
    for (models.items) |model| {
        if (model != .object) continue;
        const id = switch (model.object.get("id") orelse continue) {
            .string => |s| s,
            else => continue,
        };
        if (isDisabledModel(model.object)) continue;
        if (!first) try w.writeAll(",");
        first = false;
        try writeCodexModel(w, allocator, id, model.object, priority);
        priority += 1;
    }
    try w.writeAll("]}");
    return try buf.toOwnedSlice();
}

// ── Login ──
var login_status: enum { idle, waiting, success, failed } = .idle;
var login_error_msg: []const u8 = "";
var login_result_name: []const u8 = "";

// Paste mode: the user finishes the login in their own browser and posts the
// callback URL back. The callback port never has to be reachable from the
// container, which is the whole point.
const PASTE_LOGIN_TIMEOUT_S: i64 = 600;
// The local-callback flow used to wait forever, which let one abandoned login
// block every later paste login. Match the paste deadline.
const LEGACY_LOGIN_TIMEOUT_S: i64 = 600;
const DEFAULT_LOGIN_PORT: u16 = 58443;

var paste_mutex: std.Thread.Mutex = .{};
var paste_slot: zed_login.SlotState = .{};
var paste_keypair: ?*auth.RsaKeyPair = null;
var paste_name: ?[]u8 = null;
// The local-callback flow shares the single-login rule with paste. Guarded by
// paste_mutex so legacy+legacy and legacy+paste claims are atomic.
var legacy_slot: zed_login.SlotState = .{};

/// Both login flavours mutate `account_mgr` by deinit + reload; serialize them
/// so a callback landing at the same moment as a paste completion cannot leave
/// the manager half rebuilt.
var finalize_mutex: std.Thread.Mutex = .{};

/// Free the slot's keypair (zeroing its PEM) and name. Caller holds the mutex.
fn releasePasteSlotLocked() void {
    if (paste_keypair) |keypair| {
        keypair.deinit();
        global_allocator.destroy(keypair);
        paste_keypair = null;
    }
    if (paste_name) |name| {
        global_allocator.free(name);
        paste_name = null;
    }
}

/// Drop a login whose deadline has passed. Caller holds the mutex.
fn expirePasteSlotLocked(now: i64) void {
    if (paste_slot.expireIfDue(now)) releasePasteSlotLocked();
}

/// Drop the local-callback claim once its deadline passes. The worker thread
/// notices the same deadline through its accept poll and stays quiet.
fn expireLegacyLoginLocked(now: i64) void {
    if (legacy_slot.expireIfDue(now)) login_status = .failed;
}

/// Consume the slot after a finished login. Clearing the deadline keeps a
/// later request on the "no pending login" (409) path instead of mistaking a
/// consumed login for an expired one.
fn clearPasteSlotLocked() void {
    paste_slot.active = false;
    paste_slot.deadline = 0;
    releasePasteSlotLocked();
}

fn pasteExpiryWorker(generation: u64, deadline: i64) void {
    const now = std.time.timestamp();
    if (deadline > now) std.Thread.sleep(@as(u64, @intCast(deadline - now)) * std.time.ns_per_s);

    paste_mutex.lock();
    defer paste_mutex.unlock();
    // A newer login owns the slot now; leave its key material alone.
    if (!paste_slot.clearIfCurrent(generation)) return;
    releasePasteSlotLocked();
}

/// `.write_probe` in the data dir (cwd) proves accounts.json can be written
/// before we hand out a login URL; otherwise the failure only shows up after
/// the user has already signed in.
fn dataDirWritable() bool {
    const probe = std.fs.cwd().createFile(".write_probe", .{ .mode = 0o600 }) catch return false;
    probe.close();
    std.fs.cwd().deleteFile(".write_probe") catch {};
    return true;
}

fn parseLoginPort() error{ InvalidLoginPort, OutOfMemory }!u16 {
    // Unset keeps the documented default; anything unreadable is a bad value.
    const raw = std.process.getEnvVarOwned(global_allocator, "ZED_LOGIN_PORT") catch |err| switch (err) {
        error.EnvironmentVariableNotFound => return DEFAULT_LOGIN_PORT,
        else => return error.InvalidLoginPort,
    };
    defer global_allocator.free(raw);
    if (raw.len == 0) return DEFAULT_LOGIN_PORT;
    const port = std.fmt.parseInt(u16, raw, 10) catch return error.InvalidLoginPort;
    if (port == 0) return error.InvalidLoginPort;
    return port;
}

/// Persist the credential, reload the account manager, and make the new
/// account active. Shared by the callback-listener and paste flows.
fn finalizeLogin(name: []const u8, user_id: []const u8, credential_json: []const u8) !void {
    // Hold the lock across the read-modify-write of accounts.json as well, or
    // two simultaneous logins each re-emit a file missing the other's account.
    finalize_mutex.lock();
    defer finalize_mutex.unlock();
    try accounts.addAccount(global_allocator, name, user_id, credential_json);
    account_mgr.deinit();
    account_mgr = accounts.AccountManager.init(global_allocator);
    account_mgr.loadFromFile() catch {};
    // A freshly logged-in account is the one the user wants to use, so make it
    // active (and persist it) instead of keeping whatever was restored on load.
    _ = account_mgr.switchTo(name);
}

fn handleLogin(body: []const u8) !Response {
    var wants_paste = false;
    var name_owned: ?[]u8 = null;
    if (body.len > 0) {
        const parsed = std.json.parseFromSlice(std.json.Value, global_allocator, body, .{}) catch null;
        if (parsed) |p| {
            defer p.deinit();
            if (p.value == .object) {
                if (p.value.object.get("mode")) |mode| {
                    if (mode == .string and std.mem.eql(u8, mode.string, "paste")) wants_paste = true;
                }
                if (p.value.object.get("name")) |n| {
                    if (n == .string and n.string.len > 0) name_owned = global_allocator.dupe(u8, n.string) catch null;
                }
            }
        }
    }
    if (wants_paste) return handlePasteLogin(name_owned);

    // The worker owns the duplicated name once the thread is spawned.
    var worker_owns_name = false;
    defer if (!worker_owns_name) {
        if (name_owned) |n| global_allocator.free(n);
    };

    if (!dataDirWritable()) {
        return .{ .status = 500, .body = "{\"error\":\"data directory is not writable\",\"detail\":\"accounts.json cannot be saved; on Linux run chown -R 10001:10001 ./data or set ZED_DATA_DIR\"}" };
    }
    const account_name: []const u8 = name_owned orelse "";

    const keypair = try global_allocator.create(auth.RsaKeyPair);
    keypair.* = auth.RsaKeyPair.generate(global_allocator) catch |err| {
        global_allocator.destroy(keypair);
        return err;
    };
    const pub_key = keypair.exportPublicKeyB64(global_allocator) catch |err| {
        keypair.deinit();
        global_allocator.destroy(keypair);
        return err;
    };

    const addr = std.net.Address.initIp4(.{ 127, 0, 0, 1 }, 0);
    const tcp = try global_allocator.create(std.net.Server);
    tcp.* = addr.listen(.{}) catch |err| {
        global_allocator.free(pub_key);
        keypair.deinit();
        global_allocator.destroy(keypair);
        global_allocator.destroy(tcp);
        return err;
    };
    const port = tcp.listen_address.getPort();
    const url = try std.fmt.allocPrint(global_allocator, "https://zed.dev/native_app_signin?native_app_port={d}&native_app_public_key={s}", .{ port, pub_key });

    // Claim the single-login slot under the same lock the paste flow uses, so
    // concurrent legacy+legacy and legacy+paste requests see one winner.
    const now = std.time.timestamp();
    paste_mutex.lock();
    expirePasteSlotLocked(now);
    expireLegacyLoginLocked(now);
    if (paste_slot.active) {
        paste_mutex.unlock();
        tcp.deinit();
        global_allocator.destroy(tcp);
        keypair.deinit();
        global_allocator.destroy(keypair);
        global_allocator.free(pub_key);
        global_allocator.free(url);
        return .{ .status = 409, .body = "{\"error\":\"login already in progress\"}" };
    }
    const generation = legacy_slot.begin(now, LEGACY_LOGIN_TIMEOUT_S) catch {
        paste_mutex.unlock();
        tcp.deinit();
        global_allocator.destroy(tcp);
        keypair.deinit();
        global_allocator.destroy(keypair);
        global_allocator.free(pub_key);
        global_allocator.free(url);
        return .{ .status = 409, .body = "{\"error\":\"login already in progress\"}" };
    };
    login_status = .waiting;
    const deadline = legacy_slot.deadline;
    paste_mutex.unlock();

    const thread = std.Thread.spawn(.{}, loginWorker, .{ keypair, tcp, pub_key, account_name, generation, deadline }) catch {
        paste_mutex.lock();
        _ = legacy_slot.clearIfCurrent(generation);
        login_status = .failed;
        paste_mutex.unlock();
        tcp.deinit();
        global_allocator.destroy(tcp);
        keypair.deinit();
        global_allocator.destroy(keypair);
        global_allocator.free(pub_key);
        global_allocator.free(url);
        return .{ .status = 500, .body = "{\"error\":\"thread spawn failed\"}" };
    };
    worker_owns_name = true;
    thread.detach();
    auth.openBrowserPublic(url);

    var resp_buf: [4096]u8 = undefined;
    const resp = try std.fmt.bufPrint(&resp_buf, "{{\"login_url\":\"{s}\",\"port\":{d}}}", .{ url, port });
    const result = try global_allocator.dupe(u8, resp);
    global_allocator.free(url);
    return .{ .status = 200, .body = result, .allocated = true };
}

fn loginWorker(keypair: *auth.RsaKeyPair, tcp: *std.net.Server, pub_key: []const u8, account_name: []const u8, generation: u64, deadline: i64) void {
    defer {
        tcp.deinit();
        global_allocator.destroy(tcp);
        keypair.deinit();
        global_allocator.destroy(keypair);
        global_allocator.free(pub_key);
        if (account_name.len > 0) global_allocator.free(account_name);
    }
    const creds = auth.loginWithServer(global_allocator, keypair, tcp, deadline) catch |err| {
        paste_mutex.lock();
        // Only report a failure we still own; a newer login may hold the slot.
        if (legacy_slot.clearIfCurrent(generation)) {
            login_status = .failed;
            login_error_msg = @errorName(err);
        }
        paste_mutex.unlock();
        return;
    };
    // A newer login may already own the slot; drop this stale result instead of
    // finalizing it on top of the newer flow.
    paste_mutex.lock();
    const owned = legacy_slot.active and legacy_slot.generation == generation;
    if (owned) _ = legacy_slot.clearIfCurrent(generation);
    paste_mutex.unlock();
    if (!owned) {
        global_allocator.free(creds.user_id);
        global_allocator.free(creds.access_token);
        return;
    }
    defer global_allocator.free(creds.user_id);
    defer global_allocator.free(creds.access_token);

    const name = if (account_name.len > 0) account_name else creds.user_id;
    finalizeLogin(name, creds.user_id, creds.access_token) catch |err| {
        paste_mutex.lock();
        login_status = .failed;
        login_error_msg = @errorName(err);
        paste_mutex.unlock();
        return;
    };
    login_result_name = global_allocator.dupe(u8, name) catch "";
    paste_mutex.lock();
    // Also releases the single-login claim now that this login is finished.
    _ = legacy_slot.clearIfCurrent(generation);
    login_status = .success;
    paste_mutex.unlock();
    std.debug.print("[login] success: {s}\n", .{name});
}

fn handlePasteLogin(name_owned: ?[]u8) !Response {
    // The slot takes over the name only once it has been claimed.
    var name_transferred = false;
    defer if (!name_transferred) {
        if (name_owned) |name| global_allocator.free(name);
    };

    const now = std.time.timestamp();
    paste_mutex.lock();
    expirePasteSlotLocked(now);
    expireLegacyLoginLocked(now);
    const legacy_busy = legacy_slot.active;
    paste_mutex.unlock();
    if (legacy_busy) return .{ .status = 409, .body = "{\"error\":\"login already in progress\"}" };

    if (!dataDirWritable()) {
        return .{ .status = 500, .body = "{\"error\":\"data directory is not writable\",\"detail\":\"accounts.json cannot be saved; on Linux run chown -R 10001:10001 ./data or set ZED_DATA_DIR\"}" };
    }
    const port = parseLoginPort() catch {
        return .{ .status = 400, .body = "{\"error\":\"ZED_LOGIN_PORT must be 1-65535\"}" };
    };

    var keypair = try global_allocator.create(auth.RsaKeyPair);
    keypair.* = auth.RsaKeyPair.generate(global_allocator) catch |err| {
        global_allocator.destroy(keypair);
        return err;
    };
    const pub_key = keypair.exportPublicKeyB64(global_allocator) catch |err| {
        keypair.deinit();
        global_allocator.destroy(keypair);
        return err;
    };
    defer global_allocator.free(pub_key);

    const url = try std.fmt.allocPrint(global_allocator, "https://zed.dev/native_app_signin?native_app_port={d}&native_app_public_key={s}", .{ port, pub_key });
    defer global_allocator.free(url);

    paste_mutex.lock();
    expirePasteSlotLocked(now);
    expireLegacyLoginLocked(now);
    if (legacy_slot.active) {
        paste_mutex.unlock();
        keypair.deinit();
        global_allocator.destroy(keypair);
        return .{ .status = 409, .body = "{\"error\":\"login already in progress\"}" };
    }
    const generation = paste_slot.begin(now, PASTE_LOGIN_TIMEOUT_S) catch {
        paste_mutex.unlock();
        keypair.deinit();
        global_allocator.destroy(keypair);
        return .{ .status = 409, .body = "{\"error\":\"login already in progress\"}" };
    };
    paste_keypair = keypair;
    paste_name = name_owned;
    name_transferred = true;
    const deadline = paste_slot.deadline;
    paste_mutex.unlock();

    const timer = std.Thread.spawn(.{}, pasteExpiryWorker, .{ generation, deadline }) catch {
        paste_mutex.lock();
        _ = paste_slot.clearIfCurrent(generation);
        releasePasteSlotLocked();
        paste_mutex.unlock();
        return .{ .status = 500, .body = "{\"error\":\"thread spawn failed\"}" };
    };
    timer.detach();
    auth.openBrowserPublic(url);

    return .{
        .status = 200,
        .body = try std.fmt.allocPrint(global_allocator, "{{\"login_url\":\"{s}\",\"port\":{d},\"mode\":\"paste\",\"expires_in\":{d}}}", .{ url, port, PASTE_LOGIN_TIMEOUT_S }),
        .allocated = true,
    };
}

/// `POST /zed/login/complete` with the URL the browser landed on. The
/// ciphertext is decrypted with the slot's private key, which never leaves
/// this process.
fn handleLoginComplete(body: []const u8) !Response {
    const parsed = std.json.parseFromSlice(std.json.Value, global_allocator, body, .{}) catch
        return .{ .status = 400, .body = "{\"error\":\"invalid json\"}" };
    defer parsed.deinit();
    if (parsed.value != .object) return .{ .status = 400, .body = "{\"error\":\"missing url\"}" };

    const input = switch (parsed.value.object.get("url") orelse parsed.value.object.get("query") orelse return .{ .status = 400, .body = "{\"error\":\"missing url\"}" }) {
        .string => |text| text,
        else => return .{ .status = 400, .body = "{\"error\":\"missing url\"}" },
    };

    paste_mutex.lock();
    defer paste_mutex.unlock();

    const now = std.time.timestamp();
    if (paste_slot.expireIfDue(now)) releasePasteSlotLocked();
    if (!paste_slot.isActive(now)) {
        // A deadline still in the past means a started login ran out of time,
        // whether this request or the timer thread noticed first. A consumed
        // login (deadline 0) and a virgin slot both read as "none".
        if (paste_slot.deadline != 0 and now >= paste_slot.deadline)
            return .{ .status = 410, .body = "{\"error\":\"pending login expired; start again\"}" };
        return .{ .status = 409, .body = "{\"error\":\"no pending login\"}" };
    }
    const keypair = paste_keypair orelse return .{ .status = 409, .body = "{\"error\":\"no pending login\"}" };

    const payload = zed_login.parseCallbackPayload(global_allocator, input) catch |err| {
        return switch (err) {
            error.CallbackTooLong => .{ .status = 400, .body = "{\"error\":\"invalid callback url: too long\"}" },
            error.MissingUserId => .{ .status = 400, .body = "{\"error\":\"invalid callback url: missing user_id\"}" },
            error.MissingToken => .{ .status = 400, .body = "{\"error\":\"invalid callback url: missing access_token\"}" },
            error.InvalidCallback => .{ .status = 400, .body = "{\"error\":\"invalid callback url: no user_id/access_token query\"}" },
            error.OutOfMemory => .{ .status = 500, .body = "{\"error\":\"out of memory\"}" },
        };
    };
    defer payload.deinit(global_allocator);

    // Keep the slot on every failure below so the user can paste a corrected
    // URL; only a successful save consumes it.
    const ciphertext = auth.decodeCiphertext(global_allocator, payload.encrypted_token) catch {
        return .{ .status = 400, .body = "{\"error\":\"failed to decrypt access token (wrong key or corrupted link)\"}" };
    };
    defer global_allocator.free(ciphertext);

    const credential_json = keypair.decrypt(global_allocator, ciphertext) catch {
        return .{ .status = 400, .body = "{\"error\":\"failed to decrypt access token (wrong key or corrupted link)\"}" };
    };
    defer global_allocator.free(credential_json);

    // The slot owns `paste_name`; copy the chosen name before clearing frees it.
    const name = try global_allocator.dupe(u8, paste_name orelse payload.user_id);
    defer global_allocator.free(name);

    finalizeLogin(name, payload.user_id, credential_json) catch |err| {
        if (err == error.AccountFieldTooLong) {
            return .{ .status = 400, .body = "{\"error\":\"account name or user_id is too long (max 256 bytes)\"}" };
        }
        if (err == error.InvalidCredentialJson) {
            return .{ .status = 400, .body = "{\"error\":\"decrypted credential is not valid JSON\"}" };
        }
        return .{
            .status = 500,
            .body = try std.fmt.allocPrint(global_allocator, "{{\"error\":\"failed to save account: {s}\"}}", .{@errorName(err)}),
            .allocated = true,
        };
    };
    // Only a finished login consumes the slot; a failed save keeps it so the
    // user can fix the data directory and retry without a new OAuth round.
    clearPasteSlotLocked();

    std.debug.print("[login] success: {s}\n", .{name});
    var resp: std.io.Writer.Allocating = .init(global_allocator);
    errdefer resp.deinit();
    const w = &resp.writer;
    try w.writeAll("{\"status\":\"success\",\"name\":");
    try std.json.Stringify.encodeJsonString(name, .{}, w);
    try w.writeAll("}");
    return .{ .status = 200, .body = try resp.toOwnedSlice(), .allocated = true };
}

fn handleLoginStatus() Response {
    return switch (login_status) {
        .idle => .{ .status = 200, .body = "{\"status\":\"idle\"}" },
        .waiting => .{ .status = 200, .body = "{\"status\":\"waiting\"}" },
        .success => blk: {
            login_status = .idle;
            break :blk .{ .status = 200, .body = "{\"status\":\"success\"}" };
        },
        .failed => blk: {
            login_status = .idle;
            break :blk .{ .status = 200, .body = "{\"status\":\"failed\"}" };
        },
    };
}

test "health probe payload matches its advertised safety budget" {
    try std.testing.expect(HEALTH_PROBE_MAX_OUTPUT_TOKENS > 16);
    const body_text = try buildProbeBody(std.testing.allocator, HEALTH_PROBE_MODEL);
    defer std.testing.allocator.free(body_text);
    const parsed = try std.json.parseFromSlice(std.json.Value, std.testing.allocator, body_text, .{});
    defer parsed.deinit();
    const body = parsed.value.object;
    try std.testing.expectEqualStrings(HEALTH_PROBE_MODEL, body.get("model").?.string);
    try std.testing.expectEqualStrings("none", body.get("reasoning_effort").?.string);
    try std.testing.expectEqual(HEALTH_PROBE_MAX_OUTPUT_TOKENS, body.get("max_completion_tokens").?.integer);
    // Anthropic reads `max_tokens`; the same tiny budget is sent under both names.
    try std.testing.expectEqual(HEALTH_PROBE_MAX_OUTPUT_TOKENS, body.get("max_tokens").?.integer);
}

test "probe body uses an effort each provider accepts" {
    const allocator = std.testing.allocator;
    const cases = [_]struct { model: []const u8, effort: ?[]const u8 }{
        .{ .model = "gpt-5.6-luna", .effort = "none" },
        .{ .model = "gpt-5.6-sol", .effort = "none" },
        .{ .model = "gpt-5.5", .effort = null },
        .{ .model = "claude-sonnet-5", .effort = null },
        .{ .model = "gemini-4-argon", .effort = "low" },
    };
    // The reported effort is what the upstream will actually use; an empty
    // string means "no effort knob" and is serialized as null.
    try std.testing.expectEqualStrings("xhigh", probeEffortFor("gpt-5.5").used);
    try std.testing.expectEqualStrings("", probeEffortFor("claude-sonnet-5").used);
    for (cases) |case| {
        const body = try buildProbeBody(allocator, case.model);
        defer allocator.free(body);
        const parsed = try std.json.parseFromSlice(std.json.Value, allocator, body, .{});
        defer parsed.deinit();
        try std.testing.expectEqualStrings(case.model, parsed.value.object.get("model").?.string);
        const effort = parsed.value.object.get("reasoning_effort");
        if (case.effort) |expected| {
            try std.testing.expectEqualStrings(expected, effort.?.string);
        } else {
            try std.testing.expect(effort == null);
        }
        try std.testing.expectEqual(HEALTH_PROBE_MAX_OUTPUT_TOKENS, parsed.value.object.get("max_completion_tokens").?.integer);
        try std.testing.expectEqual(HEALTH_PROBE_MAX_OUTPUT_TOKENS, parsed.value.object.get("max_tokens").?.integer);
    }
    // Gemini never gets MINIMAL, and never gets an explicit minimal mapping.
    const gemini = try buildProbeBody(allocator, "gemini-4-argon");
    defer allocator.free(gemini);
    try std.testing.expect(std.mem.indexOf(u8, gemini, "MINIMAL") == null);

    // The advertised budget must match what the conversion layer really
    // enforces: GPT-5.5 is pinned to xhigh and floored, Gemini gets no cap.
    try std.testing.expectEqual(@as(?i64, 32), probeOutputBudget("gpt-5.6-luna"));
    try std.testing.expectEqual(@as(?i64, null), probeOutputBudget("gemini-4-argon"));
    try std.testing.expectEqual(@as(?i64, providers.GPT_REASONING_OUTPUT_FLOOR), probeOutputBudget("gpt-5.5"));
}

test "probe model must come from the embedded catalog" {
    try std.testing.expect(isKnownProbeModel("gpt-5.6-luna"));
    try std.testing.expect(isKnownProbeModel("claude-sonnet-5"));
    try std.testing.expect(isKnownProbeModel("gemini-4-argon"));
    try std.testing.expect(isKnownProbeModel("gpt-5.6"));
    try std.testing.expect(!isKnownProbeModel("gpt-9"));
    try std.testing.expect(!isKnownProbeModel(""));
    try std.testing.expect(!isKnownProbeModel("GPT-5.6-luna"));
}
