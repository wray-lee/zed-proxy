//! Gateway API-key authentication helpers. Pure functions only: no sockets,
//! so every rule below is unit-testable.

const std = @import("std");
const builtin = @import("builtin");

const MAX_KEY_FILE_BYTES = 1024 * 1024;

extern "c" fn setenv(name: [*:0]const u8, value: [*:0]const u8, overwrite: c_int) c_int;
extern "c" fn unsetenv(name: [*:0]const u8) c_int;

/// Load gateway keys: `ZED_API_KEY`, else `ZED_API_KEY_FILE`, else
/// `<data_dir>/api_key`. Values are comma- or newline-separated; surrounding
/// whitespace and empty items are dropped. Missing sources yield an empty
/// slice. Caller owns the result (see `freeKeys`).
pub fn loadKeys(allocator: std.mem.Allocator, data_dir: []const u8) ![][]const u8 {
    var raw_owned: ?[]u8 = null;
    defer if (raw_owned) |value| allocator.free(value);
    var raw: []const u8 = "";

    if (std.process.getEnvVarOwned(allocator, "ZED_API_KEY")) |value| {
        if (value.len > 0) {
            raw_owned = value;
            raw = value;
        } else {
            allocator.free(value);
        }
    } else |_| {}

    if (raw.len == 0) {
        var file_env_owned: ?[]u8 = null;
        defer if (file_env_owned) |value| allocator.free(value);
        var joined_owned: ?[]u8 = null;
        defer if (joined_owned) |value| allocator.free(value);

        var file_path: []const u8 = undefined;
        if (std.process.getEnvVarOwned(allocator, "ZED_API_KEY_FILE")) |value| {
            if (value.len > 0) {
                file_env_owned = value;
                file_path = value;
            } else {
                allocator.free(value);
            }
        } else |_| {}
        if (file_env_owned == null) {
            joined_owned = try std.fs.path.join(allocator, &.{ data_dir, "api_key" });
            file_path = joined_owned.?;
        }

        raw_owned = std.fs.cwd().readFileAlloc(allocator, file_path, MAX_KEY_FILE_BYTES) catch null;
        if (raw_owned) |value| raw = value;
    }

    var keys: std.ArrayListUnmanaged([]const u8) = .empty;
    errdefer {
        for (keys.items) |key| allocator.free(key);
        keys.deinit(allocator);
    }
    var tokens = std.mem.tokenizeAny(u8, raw, ",\r\n");
    while (tokens.next()) |token| {
        const key = std.mem.trim(u8, token, " \t");
        if (key.len == 0) continue;
        try keys.append(allocator, try allocator.dupe(u8, key));
    }
    return try keys.toOwnedSlice(allocator);
}

pub fn freeKeys(allocator: std.mem.Allocator, keys: [][]const u8) void {
    for (keys) |key| allocator.free(key);
    allocator.free(keys);
}

/// Constant-time-by-shape comparison: every configured key is hashed and
/// compared with timing-safe equality, with no early return on a hit, so the
/// response time does not reveal which key (or how many) matched.
pub fn authorized(keys: []const []const u8, presented: ?[]const u8) bool {
    const value = presented orelse return false;
    if (value.len == 0) return false;

    var presented_hash: [32]u8 = undefined;
    std.crypto.hash.sha2.Sha256.hash(value, &presented_hash, .{});

    var matched = false;
    for (keys) |key| {
        var key_hash: [32]u8 = undefined;
        std.crypto.hash.sha2.Sha256.hash(key, &key_hash, .{});
        if (std.crypto.timing_safe.eql([32]u8, key_hash, presented_hash)) matched = true;
    }
    return matched;
}

/// Extract a credential from a raw header block (CRLF separated). Prefers
/// `Authorization: Bearer <k>` (scheme case-insensitive, extra spaces
/// tolerated), falling back to `x-api-key: <k>`.
pub fn bearerFromHeaders(headers: []const u8) ?[]const u8 {
    var api_key: ?[]const u8 = null;
    var lines = std.mem.splitSequence(u8, headers, "\r\n");
    while (lines.next()) |line| {
        const colon = std.mem.indexOfScalar(u8, line, ':') orelse continue;
        const name = std.mem.trim(u8, line[0..colon], " \t");
        const value = std.mem.trim(u8, line[colon + 1 ..], " \t");
        if (std.ascii.eqlIgnoreCase(name, "authorization")) {
            if (value.len > 6 and std.ascii.eqlIgnoreCase(value[0..6], "bearer") and (value[6] == ' ' or value[6] == '\t')) {
                const token = std.mem.trim(u8, value[6..], " \t");
                if (token.len > 0) return token;
            }
        } else if (std.ascii.eqlIgnoreCase(name, "x-api-key")) {
            if (value.len > 0) api_key = value;
        }
    }
    return api_key;
}

pub fn isProtectedPath(path: []const u8) bool {
    return std.mem.startsWith(u8, path, "/v1/") or std.mem.startsWith(u8, path, "/zed/");
}

pub fn isLoopbackHost(host: []const u8) bool {
    if (std.ascii.eqlIgnoreCase(host, "localhost")) return true;
    if (std.mem.eql(u8, host, "::1") or std.mem.eql(u8, host, "[::1]") or std.mem.eql(u8, host, "0:0:0:0:0:0:0:1")) return true;
    const addr = std.net.Address.parseIp4(host, 0) catch return false;
    const bytes: *const [4]u8 = @ptrCast(&addr.in.sa.addr);
    return bytes[0] == 127;
}

/// Bind policy is fail-closed: a non-loopback address without any configured
/// key is refused instead of silently exposing an unauthenticated proxy.
pub fn bindPolicyError(host: []const u8, key_count: usize) ?[]const u8 {
    if (isLoopbackHost(host) or key_count > 0) return null;
    return "refusing to bind: 监听地址非回环 (non-loopback) 且未配置 API key；必须配置 ZED_API_KEY 或 <data>/api_key，或改回 127.0.0.1";
}

fn clearKeyEnv() void {
    if (comptime builtin.os.tag == .windows) {
        return;
    } else {
        _ = unsetenv("ZED_API_KEY");
        _ = unsetenv("ZED_API_KEY_FILE");
    }
}

fn setKeyEnv(name: [:0]const u8, value: [:0]const u8) void {
    if (comptime builtin.os.tag == .windows) {
        return;
    } else {
        _ = setenv(name, value, 1);
    }
}

fn unsetKeyEnv(name: [:0]const u8) void {
    if (comptime builtin.os.tag == .windows) {
        return;
    } else {
        _ = unsetenv(name);
    }
}

test "loadKeys splits comma and newline separated file values" {
    if (comptime builtin.os.tag == .windows) return error.SkipZigTest;
    clearKeyEnv();
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "api_key", .data = " key1 , key2\n\n key3 \r\n" });
    const dir_path = try tmp.dir.realpathAlloc(std.testing.allocator, ".");
    defer std.testing.allocator.free(dir_path);

    const keys = try loadKeys(std.testing.allocator, dir_path);
    defer freeKeys(std.testing.allocator, keys);
    try std.testing.expectEqual(@as(usize, 3), keys.len);
    try std.testing.expectEqualStrings("key1", keys[0]);
    try std.testing.expectEqualStrings("key2", keys[1]);
    try std.testing.expectEqualStrings("key3", keys[2]);
}

test "loadKeys returns empty when no env and no file" {
    if (comptime builtin.os.tag == .windows) return error.SkipZigTest;
    clearKeyEnv();
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const dir_path = try tmp.dir.realpathAlloc(std.testing.allocator, ".");
    defer std.testing.allocator.free(dir_path);

    const keys = try loadKeys(std.testing.allocator, dir_path);
    defer freeKeys(std.testing.allocator, keys);
    try std.testing.expectEqual(@as(usize, 0), keys.len);
}

test "loadKeys reads ZED_API_KEY and trims entries" {
    if (comptime builtin.os.tag == .windows) return error.SkipZigTest;
    clearKeyEnv();
    setKeyEnv("ZED_API_KEY", "  env-a ,env-b  \n env-c ");
    defer unsetKeyEnv("ZED_API_KEY");

    const keys = try loadKeys(std.testing.allocator, "/nonexistent-data-dir");
    defer freeKeys(std.testing.allocator, keys);
    try std.testing.expectEqual(@as(usize, 3), keys.len);
    try std.testing.expectEqualStrings("env-a", keys[0]);
    try std.testing.expectEqualStrings("env-b", keys[1]);
    try std.testing.expectEqualStrings("env-c", keys[2]);
}

test "loadKeys prefers ZED_API_KEY over ZED_API_KEY_FILE" {
    if (comptime builtin.os.tag == .windows) return error.SkipZigTest;
    clearKeyEnv();
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "other_key", .data = "file-key" });
    const file_path = try tmp.dir.realpathAlloc(std.testing.allocator, "other_key");
    defer std.testing.allocator.free(file_path);
    const file_path_z = try std.testing.allocator.dupeZ(u8, file_path);
    defer std.testing.allocator.free(file_path_z);

    setKeyEnv("ZED_API_KEY", "env-key");
    setKeyEnv("ZED_API_KEY_FILE", file_path_z);
    defer unsetKeyEnv("ZED_API_KEY");
    defer unsetKeyEnv("ZED_API_KEY_FILE");

    const keys = try loadKeys(std.testing.allocator, "/nonexistent-data-dir");
    defer freeKeys(std.testing.allocator, keys);
    try std.testing.expectEqual(@as(usize, 1), keys.len);
    try std.testing.expectEqualStrings("env-key", keys[0]);
}

test "loadKeys reads ZED_API_KEY_FILE when env key is unset" {
    if (comptime builtin.os.tag == .windows) return error.SkipZigTest;
    clearKeyEnv();
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "other_key", .data = "file-key-1,file-key-2" });
    const file_path = try tmp.dir.realpathAlloc(std.testing.allocator, "other_key");
    defer std.testing.allocator.free(file_path);
    const file_path_z = try std.testing.allocator.dupeZ(u8, file_path);
    defer std.testing.allocator.free(file_path_z);

    setKeyEnv("ZED_API_KEY_FILE", file_path_z);
    defer unsetKeyEnv("ZED_API_KEY_FILE");

    const keys = try loadKeys(std.testing.allocator, "/nonexistent-data-dir");
    defer freeKeys(std.testing.allocator, keys);
    try std.testing.expectEqual(@as(usize, 2), keys.len);
    try std.testing.expectEqualStrings("file-key-1", keys[0]);
    try std.testing.expectEqualStrings("file-key-2", keys[1]);
}

test "authorized accepts any configured key and rejects everything else" {
    const keys = [_][]const u8{ "key-a", "key-b" };
    try std.testing.expect(authorized(&keys, "key-a"));
    try std.testing.expect(authorized(&keys, "key-b"));
    try std.testing.expect(!authorized(&keys, "key-c"));
    try std.testing.expect(!authorized(&keys, "key-a-longer"));
    try std.testing.expect(!authorized(&keys, "key-"));
    try std.testing.expect(!authorized(&keys, ""));
    try std.testing.expect(!authorized(&keys, null));
    const empty = [_][]const u8{};
    try std.testing.expect(!authorized(&empty, "key-a"));
}

test "authorized compares all keys even when the last one matches" {
    const keys = [_][]const u8{ "first", "second", "third" };
    try std.testing.expect(authorized(&keys, "third"));
    try std.testing.expect(!authorized(&keys, "fourth"));
}

test "bearerFromHeaders prefers Authorization Bearer and tolerates spacing/case" {
    const headers =
        "POST /v1/messages HTTP/1.1\r\n" ++
        "Host: 127.0.0.1\r\n" ++
        "authorization:   bEaReR    secret-token  \r\n" ++
        "x-api-key: fallback-key\r\n";
    try std.testing.expectEqualStrings("secret-token", bearerFromHeaders(headers).?);
}

test "bearerFromHeaders falls back to x-api-key" {
    const only_api_key = "GET /v1/models HTTP/1.1\r\nX-Api-Key: fallback-key\r\n";
    try std.testing.expectEqualStrings("fallback-key", bearerFromHeaders(only_api_key).?);

    const basic_auth = "GET /v1/models HTTP/1.1\r\nAuthorization: Basic abc\r\nx-api-key: fallback-key\r\n";
    try std.testing.expectEqualStrings("fallback-key", bearerFromHeaders(basic_auth).?);

    const none = "GET /v1/models HTTP/1.1\r\nHost: x\r\n";
    try std.testing.expectEqual(@as(?[]const u8, null), bearerFromHeaders(none));

    const bare_scheme = "GET /v1/models HTTP/1.1\r\nAuthorization: Bearer\r\n";
    try std.testing.expectEqual(@as(?[]const u8, null), bearerFromHeaders(bare_scheme));
}

test "isProtectedPath only guards /v1/ and /zed/ prefixes" {
    try std.testing.expect(isProtectedPath("/v1/models"));
    try std.testing.expect(isProtectedPath("/v1/messages"));
    try std.testing.expect(isProtectedPath("/v1/"));
    try std.testing.expect(isProtectedPath("/zed/accounts"));
    try std.testing.expect(isProtectedPath("/zed/login/status"));
    try std.testing.expect(!isProtectedPath("/"));
    try std.testing.expect(!isProtectedPath("/health"));
    try std.testing.expect(!isProtectedPath("/api/event_logging/batch"));
    try std.testing.expect(!isProtectedPath("/zed"));
    try std.testing.expect(!isProtectedPath("/v1"));
    try std.testing.expect(!isProtectedPath("/V1/models"));
    try std.testing.expect(!isProtectedPath(""));
}

test "isLoopbackHost matrix" {
    const loopback = [_][]const u8{ "localhost", "LocalHost", "127.0.0.1", "127.0.0.53", "127.255.255.255", "::1", "[::1]", "0:0:0:0:0:0:0:1" };
    for (loopback) |host| try std.testing.expect(isLoopbackHost(host));
    const not_loopback = [_][]const u8{ "0.0.0.0", "::", "[::]", "192.0.2.10", "198.51.100.5", "203.0.113.1", "example.com", "", "127.0.0.256" };
    for (not_loopback) |host| try std.testing.expect(!isLoopbackHost(host));
}

test "bindPolicyError refuses public bind without keys" {
    try std.testing.expectEqual(@as(?[]const u8, null), bindPolicyError("127.0.0.1", 0));
    try std.testing.expectEqual(@as(?[]const u8, null), bindPolicyError("localhost", 0));
    try std.testing.expectEqual(@as(?[]const u8, null), bindPolicyError("0.0.0.0", 1));
    try std.testing.expectEqual(@as(?[]const u8, null), bindPolicyError("198.51.100.2", 2));

    const reason = bindPolicyError("0.0.0.0", 0).?;
    try std.testing.expect(std.mem.indexOf(u8, reason, "监听地址非回环") != null);
    try std.testing.expect(std.mem.indexOf(u8, reason, "必须配置 ZED_API_KEY 或 <data>/api_key，或改回 127.0.0.1") != null);
    try std.testing.expect(bindPolicyError("::", 0) != null);
    try std.testing.expect(bindPolicyError("198.51.100.2", 0) != null);
}
