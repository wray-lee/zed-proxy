//! Paste-callback login helpers. Pure functions only: no sockets, no files,
//! no clock reads (callers pass `now`), so every rule below is unit-testable.

const std = @import("std");

/// Callback URLs are tiny; anything larger is a client bug or an attack.
pub const MAX_CALLBACK_BYTES = 8192;

/// Decoded fields of a Zed login callback. Both slices are owned by the
/// allocator passed to `parseCallbackPayload`.
pub const CallbackPayload = struct {
    user_id: []u8,
    encrypted_token: []u8,

    pub fn deinit(self: CallbackPayload, allocator: std.mem.Allocator) void {
        allocator.free(self.user_id);
        allocator.free(self.encrypted_token);
    }
};

pub const ParseError = error{
    CallbackTooLong,
    InvalidCallback,
    MissingUserId,
    MissingToken,
    OutOfMemory,
};

/// Parse either a full callback URL or a bare query string. Zed always sends
/// `...?user_id=<id>&access_token=<base64url ciphertext>`; everything else is
/// ignored so extra query parameters (state, code, ...) stay harmless.
/// Duplicate keys follow URLSearchParams: the last value wins.
pub fn parseCallbackPayload(allocator: std.mem.Allocator, input: []const u8) ParseError!CallbackPayload {
    if (input.len > MAX_CALLBACK_BYTES) return error.CallbackTooLong;

    var text = std.mem.trim(u8, input, " \t\r\n");
    if (std.mem.indexOfScalar(u8, text, '#')) |fragment| text = text[0..fragment];

    const query = if (std.mem.indexOfScalar(u8, text, '?')) |question|
        text[question + 1 ..]
    else if (std.mem.indexOfScalar(u8, text, '=') != null)
        text
    else
        return error.InvalidCallback;

    var user_id: ?[]u8 = null;
    errdefer if (user_id) |value| allocator.free(value);
    var encrypted_token: ?[]u8 = null;
    errdefer if (encrypted_token) |value| allocator.free(value);

    var params = std.mem.splitScalar(u8, query, '&');
    while (params.next()) |param| {
        if (param.len == 0) continue;
        // A parameter without '=' carries no value; unknown keys are ignored.
        const equals = std.mem.indexOfScalar(u8, param, '=') orelse continue;
        const key = param[0..equals];
        const raw_value = param[equals + 1 ..];
        // Decode before freeing the old value: on OOM the errdefer must see a
        // live pointer, not the one it is about to free.
        if (std.mem.eql(u8, key, "user_id")) {
            const decoded = try urlDecode(allocator, raw_value);
            if (user_id) |old| allocator.free(old);
            user_id = decoded;
        } else if (std.mem.eql(u8, key, "access_token")) {
            const decoded = try urlDecode(allocator, raw_value);
            if (encrypted_token) |old| allocator.free(old);
            encrypted_token = decoded;
        }
    }

    const uid = user_id orelse return error.MissingUserId;
    if (uid.len == 0) return error.MissingUserId;
    const token = encrypted_token orelse return error.MissingToken;
    if (token.len == 0) return error.MissingToken;

    user_id = null;
    encrypted_token = null;
    return .{ .user_id = uid, .encrypted_token = token };
}

/// WHATWG-style percent decoding: `%XX` to a byte, `+` to space, and invalid
/// escapes pass through literally instead of failing the whole callback.
fn urlDecode(allocator: std.mem.Allocator, input: []const u8) ParseError![]u8 {
    var result: std.ArrayList(u8) = .empty;
    defer result.deinit(allocator);

    var i: usize = 0;
    while (i < input.len) {
        const c = input[i];
        if (c == '%' and i + 2 < input.len) {
            const hi = hexVal(input[i + 1]);
            const lo = hexVal(input[i + 2]);
            if (hi != null and lo != null) {
                try result.append(allocator, (hi.? << 4) | lo.?);
                i += 3;
                continue;
            }
        }
        try result.append(allocator, if (c == '+') ' ' else c);
        i += 1;
    }
    return result.toOwnedSlice(allocator);
}

fn hexVal(c: u8) ?u8 {
    if (c >= '0' and c <= '9') return c - '0';
    if (c >= 'a' and c <= 'f') return c - 'a' + 10;
    if (c >= 'A' and c <= 'F') return c - 'A' + 10;
    return null;
}

/// One login slot at a time. `generation` lets a late timer thread (or an
/// aborted flow) clear only the slot it created, never a newer one.
pub const SlotState = struct {
    active: bool = false,
    deadline: i64 = 0,
    generation: u64 = 0,

    /// Claim the slot. An expired slot counts as free; an unexpired one is
    /// busy. Returns the new generation for the caller's timer thread.
    pub fn begin(self: *SlotState, now: i64, timeout_s: i64) error{Busy}!u64 {
        if (self.isActive(now)) return error.Busy;
        self.active = true;
        self.deadline = now + timeout_s;
        self.generation +%= 1;
        return self.generation;
    }

    /// Drop the slot if its deadline has passed. Returns true when it did.
    pub fn expireIfDue(self: *SlotState, now: i64) bool {
        if (!self.active or now < self.deadline) return false;
        self.active = false;
        return true;
    }

    /// Drop the slot only if it still belongs to `generation`. The deadline is
    /// kept so a request can still tell "ran out of time" from "never started";
    /// callers that finish a login reset `deadline` themselves.
    pub fn clearIfCurrent(self: *SlotState, generation: u64) bool {
        if (!self.active or self.generation != generation) return false;
        self.active = false;
        return true;
    }

    pub fn isActive(self: *const SlotState, now: i64) bool {
        return self.active and now < self.deadline;
    }
};

// ── Tests ──

fn expectPayload(input: []const u8, expected_user_id: []const u8, expected_token: []const u8) !void {
    const parsed = try parseCallbackPayload(std.testing.allocator, input);
    defer parsed.deinit(std.testing.allocator);
    try std.testing.expectEqualStrings(expected_user_id, parsed.user_id);
    try std.testing.expectEqualStrings(expected_token, parsed.encrypted_token);
}

test "parseCallbackPayload accepts a full callback URL" {
    try expectPayload("http://127.0.0.1:58443/?user_id=4242&access_token=abc-DEF_123", "4242", "abc-DEF_123");
}

test "parseCallbackPayload accepts bare queries with and without '?'" {
    try expectPayload("user_id=4242&access_token=token-1", "4242", "token-1");
    try expectPayload("?user_id=4242&access_token=token-1", "4242", "token-1");
}

test "parseCallbackPayload trims whitespace and drops the fragment" {
    try expectPayload("  http://127.0.0.1:58443/?user_id=1&access_token=t#user_id=9&access_token=bad  ", "1", "t");
    try expectPayload("user_id=1&access_token=t#frag=zzz", "1", "t");
}

test "parseCallbackPayload decodes %XX and '+' in values" {
    try expectPayload("http://x/?user_id=user%2Bone&access_token=a%2Bb%20c", "user+one", "a+b c");
    try expectPayload("user_id=a+b&access_token=t%7E", "a b", "t~");
}

test "parseCallbackPayload ignores unknown parameters" {
    try expectPayload("http://x/?state=xyz&user_id=7&code=abc&access_token=t", "7", "t");
    // A parameter without '=' is not a known key, so it is ignored too.
    try expectPayload("http://x/?garbage&user_id=7&access_token=t", "7", "t");
}

test "parseCallbackPayload keeps the last value of a duplicate key" {
    try expectPayload("user_id=1&access_token=first&user_id=2&access_token=second", "2", "second");
}

test "parseCallbackPayload reports missing user_id" {
    try std.testing.expectError(error.MissingUserId, parseCallbackPayload(std.testing.allocator, "?access_token=t"));
    try std.testing.expectError(error.MissingUserId, parseCallbackPayload(std.testing.allocator, "?user_id=&access_token=t"));
}

test "parseCallbackPayload reports missing access_token" {
    try std.testing.expectError(error.MissingToken, parseCallbackPayload(std.testing.allocator, "?user_id=1"));
    try std.testing.expectError(error.MissingToken, parseCallbackPayload(std.testing.allocator, "?user_id=1&access_token="));
}

test "parseCallbackPayload rejects input that is not a callback" {
    try std.testing.expectError(error.InvalidCallback, parseCallbackPayload(std.testing.allocator, ""));
    try std.testing.expectError(error.InvalidCallback, parseCallbackPayload(std.testing.allocator, "not a callback"));
    try std.testing.expectError(error.InvalidCallback, parseCallbackPayload(std.testing.allocator, "http://127.0.0.1:58443/"));
}

test "parseCallbackPayload rejects oversized input" {
    var oversized: [MAX_CALLBACK_BYTES + 1]u8 = @splat('a');
    try std.testing.expectError(error.CallbackTooLong, parseCallbackPayload(std.testing.allocator, &oversized));
}

test "SlotState only admits one login at a time" {
    var slot: SlotState = .{};
    const first = try slot.begin(1000, 600);
    try std.testing.expect(slot.isActive(1000));
    try std.testing.expectError(error.Busy, slot.begin(1000, 600));
    try std.testing.expectError(error.Busy, slot.begin(1599, 600));

    // At the deadline the slot is stale and can be reclaimed.
    try std.testing.expect(!slot.isActive(1600));
    const second = try slot.begin(1600, 600);
    try std.testing.expect(second != first);
    try std.testing.expect(slot.isActive(1600));
}

test "SlotState expireIfDue only fires once past the deadline" {
    var slot: SlotState = .{};
    _ = try slot.begin(0, 600);
    try std.testing.expect(!slot.expireIfDue(599));
    try std.testing.expect(slot.isActive(599));
    try std.testing.expect(slot.expireIfDue(600));
    try std.testing.expect(!slot.isActive(600));
    try std.testing.expect(!slot.expireIfDue(601));
}

test "SlotState clearIfCurrent never clears a newer generation" {
    var slot: SlotState = .{};
    const first = try slot.begin(0, 600);
    try std.testing.expect(slot.clearIfCurrent(first));
    try std.testing.expect(!slot.clearIfCurrent(first));

    const second = try slot.begin(10, 600);
    try std.testing.expect(!slot.clearIfCurrent(first));
    try std.testing.expect(slot.isActive(10));
    try std.testing.expect(slot.clearIfCurrent(second));
    try std.testing.expect(!slot.isActive(10));
}
