const std = @import("std");

// HTTP-free Orb portal logic: link validation, token extraction, and parsing of
// the two portal responses into one credit summary. The link carries a bearer
// token in its query string, so nothing here formats or logs the URL.

pub const ORB_HOST = "portal.withorb.com";

pub const LinkError = error{InvalidOrbLink};

fn componentBytes(component: std.Uri.Component) []const u8 {
    return switch (component) {
        .raw, .percent_encoded => |bytes| bytes,
    };
}

/// The `token` value from a URL query string, still percent-encoded as pasted.
/// Control characters, quotes and backslashes are rejected because the value is
/// later embedded in a curl config file.
pub fn tokenFromQuery(query: []const u8) ?[]const u8 {
    var pairs = std.mem.splitScalar(u8, query, '&');
    while (pairs.next()) |pair| {
        const eq = std.mem.indexOfScalar(u8, pair, '=') orelse continue;
        if (!std.mem.eql(u8, pair[0..eq], "token")) continue;
        const value = pair[eq + 1 ..];
        if (value.len == 0) return null;
        for (value) |byte| {
            if (byte < 0x20 or byte == 0x7f) return null;
            if (byte == '"' or byte == '\\') return null;
        }
        return value;
    }
    return null;
}

pub fn tokenFromLink(url: []const u8) ?[]const u8 {
    const uri = std.Uri.parse(url) catch return null;
    if (!std.ascii.eqlIgnoreCase(uri.scheme, "https")) return null;
    const query = uri.query orelse return null;
    return tokenFromQuery(componentBytes(query));
}

/// Percent-encode a query value while keeping the URL-safe characters a portal
/// token already uses, so an already-encoded token round-trips unchanged.
pub fn encodeToken(allocator: std.mem.Allocator, token: []const u8) ![]const u8 {
    var out: std.io.Writer.Allocating = .init(allocator);
    errdefer out.deinit();
    const w = &out.writer;
    for (token) |byte| {
        if (std.ascii.isAlphanumeric(byte) or byte == '-' or byte == '_' or byte == '.' or byte == '~' or byte == '%') {
            try w.writeByte(byte);
        } else {
            try w.print("%{X:0>2}", .{byte});
        }
    }
    return try out.toOwnedSlice();
}

/// A link is accepted only when it is https, points at the portal itself, and
/// carries a non-empty token. Anything else is refused before it is stored.
/// Userinfo and a non-default port are refused too: `https://portal.withorb.com@evil.example`
/// has host `evil.example`, but `https://portal.withorb.com:8443` would pass the
/// host check while never reaching the portal, and a userinfo form invites
/// confusion about which part is the host.
pub fn validateLink(url: []const u8) LinkError!void {
    const uri = std.Uri.parse(url) catch return error.InvalidOrbLink;
    if (!std.ascii.eqlIgnoreCase(uri.scheme, "https")) return error.InvalidOrbLink;
    if (uri.user != null or uri.password != null) return error.InvalidOrbLink;
    if (uri.port != null and uri.port.? != 443) return error.InvalidOrbLink;
    const host = uri.host orelse return error.InvalidOrbLink;
    var host_buf: [std.Uri.host_name_max]u8 = undefined;
    const host_raw = host.toRaw(&host_buf) catch return error.InvalidOrbLink;
    // Host names are case-insensitive; subdomains and lookalikes are not the portal.
    if (!std.ascii.eqlIgnoreCase(host_raw, ORB_HOST)) return error.InvalidOrbLink;
    const query = uri.query orelse return error.InvalidOrbLink;
    if (tokenFromQuery(componentBytes(query)) == null) return error.InvalidOrbLink;
}

fn numberValue(value: std.json.Value) ?f64 {
    const number: f64 = switch (value) {
        .integer => |int| @floatFromInt(int),
        .float => |float| float,
        .number_string => |text| std.fmt.parseFloat(f64, text) catch return null,
        .string => |text| std.fmt.parseFloat(f64, text) catch return null,
        else => return null,
    };
    // "nan", "inf" and overflow all parse to a non-finite f64, which is not
    // valid JSON and would poison the sums; treat them as absent instead.
    if (!std.math.isFinite(number)) return null;
    return number;
}

fn stringValue(value: ?std.json.Value) ?[]const u8 {
    const present = value orelse return null;
    return switch (present) {
        .string => |text| if (text.len > 0) text else null,
        else => null,
    };
}

pub const Customer = struct {
    id: []const u8,
    pricing_unit_id: []const u8,
};

/// `{"customer":{"id":..,"pricing_unit":{"id":..}}}`. Slices borrow from `root`.
pub fn parseCustomer(root: std.json.Value) ?Customer {
    if (root != .object) return null;
    const customer_value = root.object.get("customer") orelse return null;
    if (customer_value != .object) return null;
    const id = stringValue(customer_value.object.get("id")) orelse return null;
    const unit_value = customer_value.object.get("pricing_unit") orelse return null;
    if (unit_value != .object) return null;
    const unit_id = stringValue(unit_value.object.get("id")) orelse return null;
    return .{ .id = id, .pricing_unit_id = unit_id };
}

/// ISO 8601 timestamps from the portal fit easily; anything longer is ignored
/// rather than truncated, so a malformed date cannot break the JSON output.
pub const MAX_DATE_BYTES = 40;

/// One account's credit summary. It owns its dates so it can be cached in
/// memory after the parsed response is released.
pub const Credit = struct {
    ok: bool = false,
    /// Short static code shown to the UI; only set when `ok` is false.
    error_code: ?[]const u8 = "parse_error",
    balance_usd: f64 = 0,
    cap_usd: f64 = 0,
    used_usd: f64 = 0,
    used_pct: f64 = 0,
    period_start_buf: [MAX_DATE_BYTES]u8 = [_]u8{0} ** MAX_DATE_BYTES,
    period_start_len: usize = 0,
    resets_at_buf: [MAX_DATE_BYTES]u8 = [_]u8{0} ** MAX_DATE_BYTES,
    resets_at_len: usize = 0,
    total_balance_usd: f64 = 0,

    /// Earliest effective date among the counted blocks.
    pub fn periodStart(self: *const Credit) []const u8 {
        return self.period_start_buf[0..self.period_start_len];
    }

    /// Earliest expiry date among the counted blocks.
    pub fn resetsAt(self: *const Credit) []const u8 {
        return self.resets_at_buf[0..self.resets_at_len];
    }
};

fn earlier(current: ?[]const u8, candidate: []const u8) []const u8 {
    // Both dates come from the portal in the same ISO shape, so byte order is
    // date order.
    if (current) |existing| {
        if (std.mem.lessThan(u8, existing, candidate)) return existing;
    }
    return candidate;
}

/// Aggregate `ledger_summary`: sum every block that has a balance, take the
/// earliest start and the earliest expiry, and derive used from cap - balance.
/// Strings in the result borrow from `root`.
pub fn parseLedger(root: std.json.Value) Credit {
    var result: Credit = .{};
    if (root != .object) return result;
    const blocks_value = root.object.get("credit_blocks") orelse return result;
    if (blocks_value != .array) return result;

    var counted = false;
    var balance: f64 = 0;
    var cap: f64 = 0;
    var period_start: ?[]const u8 = null;
    var resets_at: ?[]const u8 = null;
    for (blocks_value.array.items) |block| {
        if (block != .object) continue;
        const block_balance = numberValue(block.object.get("balance") orelse continue) orelse continue;
        counted = true;
        balance += block_balance;
        if (block.object.get("maximum_initial_balance")) |cap_value| {
            if (numberValue(cap_value)) |value| cap += value;
        }
        if (stringValue(block.object.get("effective_date"))) |date| period_start = earlier(period_start, date);
        if (stringValue(block.object.get("expiry_date"))) |date| resets_at = earlier(resets_at, date);
    }
    if (!counted) return result;

    result.ok = true;
    result.error_code = null;
    result.balance_usd = balance;
    result.cap_usd = cap;
    result.used_usd = @max(@as(f64, 0), cap - balance);
    result.used_pct = if (cap > 0) result.used_usd / cap * 100 else 0;
    if (period_start) |date| {
        if (date.len <= MAX_DATE_BYTES) {
            @memcpy(result.period_start_buf[0..date.len], date);
            result.period_start_len = date.len;
        }
    }
    if (resets_at) |date| {
        if (date.len <= MAX_DATE_BYTES) {
            @memcpy(result.resets_at_buf[0..date.len], date);
            result.resets_at_len = date.len;
        }
    }
    result.total_balance_usd = if (root.object.get("credits_balance")) |value|
        numberValue(value) orelse balance
    else
        balance;
    return result;
}

// ── Tests ──

test "validateLink accepts only https portal links with a token" {
    try validateLink("https://portal.withorb.com/?token=abc123");
    try validateLink("https://portal.withorb.com/customer?foo=1&token=abc123");
    try validateLink("https://Portal.WithOrb.com/?token=abc123");

    try std.testing.expectError(error.InvalidOrbLink, validateLink("http://portal.withorb.com/?token=abc123"));
    try std.testing.expectError(error.InvalidOrbLink, validateLink("https://portal.withorb.com.evil.test/?token=abc123"));
    try std.testing.expectError(error.InvalidOrbLink, validateLink("https://evil.portal.withorb.com/?token=abc123"));
    try std.testing.expectError(error.InvalidOrbLink, validateLink("https://portal.withorb.com/"));
    try std.testing.expectError(error.InvalidOrbLink, validateLink("https://portal.withorb.com/?token="));
    try std.testing.expectError(error.InvalidOrbLink, validateLink("https://portal.withorb.com/?tok=abc123"));
    try std.testing.expectError(error.InvalidOrbLink, validateLink("not a url"));
    try std.testing.expectError(error.InvalidOrbLink, validateLink(""));

    // Userinfo and non-default ports must not pass even though the parsed host
    // still reads portal.withorb.com.
    try std.testing.expectError(error.InvalidOrbLink, validateLink("https://portal.withorb.com@evil.example/?token=abc123"));
    try std.testing.expectError(error.InvalidOrbLink, validateLink("https://user:pw@portal.withorb.com/?token=abc123"));
    try std.testing.expectError(error.InvalidOrbLink, validateLink("https://portal.withorb.com:8443/?token=abc123"));
    try validateLink("https://portal.withorb.com:443/?token=abc123");
}

test "token extraction keeps the value percent-encoded as pasted" {
    try std.testing.expectEqualStrings("abc123", tokenFromLink("https://portal.withorb.com/?token=abc123&x=1").?);
    try std.testing.expectEqualStrings("abc123", tokenFromLink("https://portal.withorb.com/?x=1&token=abc123").?);
    try std.testing.expectEqualStrings("a%2Bb-c_d", tokenFromLink("https://portal.withorb.com/?token=a%2Bb-c_d").?);
    try std.testing.expect(tokenFromLink("https://portal.withorb.com/?token=") == null);
    try std.testing.expect(tokenFromLink("https://portal.withorb.com/?token=a%22b") != null);
    try std.testing.expect(tokenFromLink("https://portal.withorb.com/?token=a\"b") == null);
}

test "token encoding keeps URL-safe characters and escapes separators" {
    const allocator = std.testing.allocator;
    const plain_enc = try encodeToken(allocator, "abc-123_4.5~6");
    defer allocator.free(plain_enc);
    try std.testing.expectEqualStrings("abc-123_4.5~6", plain_enc);

    const escaped = try encodeToken(allocator, "a&b#c=d e");
    defer allocator.free(escaped);
    try std.testing.expectEqualStrings("a%26b%23c%3Dd%20e", escaped);

    // Already percent-encoded input must not be double encoded.
    const already = try encodeToken(allocator, "a%2Bb");
    defer allocator.free(already);
    try std.testing.expectEqualStrings("a%2Bb", already);
}

test "parseCustomer reads the id and pricing unit" {
    const allocator = std.testing.allocator;
    const parsed = try std.json.parseFromSlice(std.json.Value, allocator,
        \\{"customer":{"id":"cust_1","pricing_unit":{"id":"pu_9"},"name":"x"}}
    , .{});
    defer parsed.deinit();
    const customer = parseCustomer(parsed.value).?;
    try std.testing.expectEqualStrings("cust_1", customer.id);
    try std.testing.expectEqualStrings("pu_9", customer.pricing_unit_id);

    const missing = try std.json.parseFromSlice(std.json.Value, allocator, "{\"customer\":{}}", .{});
    defer missing.deinit();
    try std.testing.expect(parseCustomer(missing.value) == null);
}

test "parseLedger sums valid blocks and takes the earliest dates" {
    const allocator = std.testing.allocator;
    const parsed = try std.json.parseFromSlice(std.json.Value, allocator,
        \\{"credit_blocks":[
        \\  {"balance":"9.9597","maximum_initial_balance":"10.00","effective_date":"2026-11-04T00:00:00+00:00","expiry_date":"2026-12-04T00:00:00+00:00"},
        \\  {"balance":null,"maximum_initial_balance":"5.00","effective_date":"2026-10-01T00:00:00+00:00","expiry_date":"2026-11-01T00:00:00+00:00"},
        \\  {"balance":2.5,"maximum_initial_balance":5,"effective_date":"2026-10-20T00:00:00+00:00","expiry_date":"2026-11-20T00:00:00+00:00"}
        \\],"credits_balance":"12.4597"}
    , .{});
    defer parsed.deinit();

    const credit = parseLedger(parsed.value);
    try std.testing.expect(credit.ok);
    try std.testing.expectEqual(@as(?[]const u8, null), credit.error_code);
    try std.testing.expectApproxEqAbs(@as(f64, 12.4597), credit.balance_usd, 0.00001);
    try std.testing.expectApproxEqAbs(@as(f64, 15.0), credit.cap_usd, 0.00001);
    try std.testing.expectApproxEqAbs(@as(f64, 2.5403), credit.used_usd, 0.00001);
    try std.testing.expectApproxEqAbs(@as(f64, 2.5403 / 15.0 * 100), credit.used_pct, 0.00001);
    try std.testing.expectEqualStrings("2026-10-20T00:00:00+00:00", credit.periodStart());
    try std.testing.expectEqualStrings("2026-11-20T00:00:00+00:00", credit.resetsAt());
    try std.testing.expectApproxEqAbs(@as(f64, 12.4597), credit.total_balance_usd, 0.00001);
}

test "parseLedger falls back to the summed balance and rejects empty data" {
    const allocator = std.testing.allocator;
    const parsed = try std.json.parseFromSlice(std.json.Value, allocator,
        \\{"credit_blocks":[{"balance":"3.5","maximum_initial_balance":"10.00"}]}
    , .{});
    defer parsed.deinit();
    const credit = parseLedger(parsed.value);
    try std.testing.expect(credit.ok);
    try std.testing.expectApproxEqAbs(@as(f64, 3.5), credit.total_balance_usd, 0.00001);
    try std.testing.expectApproxEqAbs(@as(f64, 6.5), credit.used_usd, 0.00001);
    try std.testing.expectEqual(@as(f64, 65), credit.used_pct);

    const empty = try std.json.parseFromSlice(std.json.Value, allocator, "{\"credit_blocks\":[]}", .{});
    defer empty.deinit();
    const empty_credit = parseLedger(empty.value);
    try std.testing.expect(!empty_credit.ok);
    try std.testing.expectEqualStrings("parse_error", empty_credit.error_code.?);

    const garbage = try std.json.parseFromSlice(std.json.Value, allocator, "[]", .{});
    defer garbage.deinit();
    try std.testing.expect(!parseLedger(garbage.value).ok);
}

test "parseLedger ignores non-finite amounts so the JSON stays valid" {
    const allocator = std.testing.allocator;
    const parsed = try std.json.parseFromSlice(std.json.Value, allocator,
        \\{"credit_blocks":[{"balance":"nan","maximum_initial_balance":"10"},{"balance":"inf","maximum_initial_balance":"10"},{"balance":"2.5","maximum_initial_balance":"10"}]}
    , .{});
    defer parsed.deinit();
    const credit = parseLedger(parsed.value);
    try std.testing.expect(credit.ok);
    try std.testing.expect(std.math.isFinite(credit.balance_usd));
    try std.testing.expect(std.math.isFinite(credit.used_pct));
    try std.testing.expectApproxEqAbs(@as(f64, 2.5), credit.balance_usd, 0.00001);
}
