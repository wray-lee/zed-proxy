const std = @import("std");
const builtin = @import("builtin");

/// Why a request against an account failed. Drives how long the account is
/// benched by the health scheduler.
pub const FailureKind = enum {
    /// Bad credential / banned / trial blocked (401/403). Account-level and
    /// sticky, so bench it for a while and prefer other nodes.
    auth,
    /// Upstream rate limit (429). Transient but account-level; short cooldown.
    rate_limit,
    /// Upstream 5xx or network error. Ambiguous (could be model-specific or a
    /// blip), so only bench after repeated hits.
    transient,
};

/// Result of the most recent explicit model probe. This is separate from the
/// scheduler's cooldown state: one transient probe failure should be visible
/// in the UI even though the scheduler only benches an account after repeated
/// transient failures.
pub const ProbeState = enum {
    unchecked,
    healthy,
    auth_error,
    rate_limited,
    upstream_error,
};

pub const AUTH_COOLDOWN_S: i64 = 300;
pub const RATE_LIMIT_COOLDOWN_S: i64 = 30;
pub const TRANSIENT_COOLDOWN_S: i64 = 20;
pub const TRANSIENT_TRIP_THRESHOLD: u32 = 3;

// After this many auth failures in a row the credential is almost certainly
// dead (expired token / banned account), not a transient blip, so it gets a
// long cooldown and sinks to the bottom of the try order instead of stealing
// the first request of every session.
pub const DEAD_CREDENTIAL_THRESHOLD: u32 = 3;
pub const DEAD_CREDENTIAL_COOLDOWN_S: i64 = 3600;

// File next to accounts.json that remembers which account was last active, so a
// restart resumes on that (known-good) account instead of reverting to the
// first entry in accounts.json, which may be a dead one.
const ACTIVE_ACCOUNT_FILE = "active_account.txt";

// Temporary sibling for the setOrbUrl read-modify-write, renamed over the
// original once written.
const ACCOUNTS_TMP_FILE = "accounts.json.tmp";

pub const Account = struct {
    name: []const u8,
    user_id: []const u8,
    credential_json: []const u8,
    // Optional Orb portal link with a bearer token in its query string. It is
    // credential-equivalent, so it is never logged or returned by the API.
    orb_url: ?[]const u8 = null,
    jwt_token: ?[]const u8 = null,
    jwt_exp: i64 = 0,

    // ── Health tracking ──
    // Unix seconds until which this account is skipped by the scheduler.
    // 0 means healthy. Set when a request fails so a bad node stops being
    // hammered and other nodes get priority.
    disabled_until: i64 = 0,
    consecutive_failures: u32 = 0,
    // Last upstream HTTP status observed for this account (0 = network/none).
    last_status: u16 = 0,
    // Unix seconds of the last successful request (for diagnostics).
    last_ok: i64 = 0,
    // Explicit low-cost model probe diagnostics. They are intentionally kept
    // in memory only; credentials remain solely in accounts.json.
    last_probe_state: ProbeState = .unchecked,
    last_probe_at: i64 = 0,
    last_probe_latency_ms: i64 = 0,
};

pub const AccountManager = struct {
    allocator: std.mem.Allocator,
    list: std.ArrayListUnmanaged(Account) = .empty,
    current: usize = 0,
    arena: std.heap.ArenaAllocator,

    pub fn init(allocator: std.mem.Allocator) AccountManager {
        return .{
            .allocator = allocator,
            .arena = std.heap.ArenaAllocator.init(allocator),
        };
    }

    pub fn deinit(self: *AccountManager) void {
        self.list.deinit(self.allocator);
        self.arena.deinit();
    }

    pub fn getCurrent(self: *AccountManager) ?*Account {
        if (self.list.items.len == 0) return null;
        return &self.list.items[self.current];
    }

    pub fn switchTo(self: *AccountManager, name: []const u8) bool {
        for (self.list.items, 0..) |acc, i| {
            if (std.mem.eql(u8, acc.name, name)) {
                self.setCurrent(i);
                return true;
            }
        }
        return false;
    }

    /// Set the active account and persist the choice so a restart resumes here
    /// instead of reverting to accounts.json's first entry. No-op if unchanged.
    pub fn setCurrent(self: *AccountManager, idx: usize) void {
        if (idx >= self.list.items.len) return;
        self.current = idx;
        self.saveActive();
    }

    /// Write the active account name next to accounts.json. Best-effort: a
    /// failure here must never break request handling.
    fn saveActive(self: *AccountManager) void {
        if (self.current >= self.list.items.len) return;
        const name = self.list.items[self.current].name;
        const file = std.fs.cwd().createFile(ACTIVE_ACCOUNT_FILE, .{ .mode = 0o600 }) catch return;
        defer file.close();
        file.writeAll(name) catch {};
    }

    /// Restore the persisted active account, then make sure `current` points at
    /// a usable account: prefer the remembered one, otherwise the first healthy
    /// account, so the very first request doesn't land on a dead credential.
    fn selectStartupCurrent(self: *AccountManager) void {
        const total = self.list.items.len;
        if (total == 0) return;

        var buf: [256]u8 = undefined;
        if (std.fs.cwd().readFile(ACTIVE_ACCOUNT_FILE, &buf)) |raw| {
            const name = std.mem.trim(u8, raw, " \t\r\n");
            for (self.list.items, 0..) |acc, i| {
                if (std.mem.eql(u8, acc.name, name)) {
                    self.current = i;
                    return;
                }
            }
        } else |_| {}

        // No usable persisted choice: fall back to the first healthy account.
        if (isAvailable(&self.list.items[self.current])) return;
        for (self.list.items, 0..) |*acc, i| {
            if (isAvailable(acc)) {
                self.current = i;
                return;
            }
        }
    }

    /// True if the account is not currently benched by the health scheduler.
    pub fn isAvailable(acc: *const Account) bool {
        return std.time.timestamp() >= acc.disabled_until;
    }

    /// Clear failure state after a request goes through.
    pub fn markSuccess(acc: *Account, status: u16) void {
        acc.consecutive_failures = 0;
        acc.disabled_until = 0;
        acc.last_status = status;
        acc.last_ok = std.time.timestamp();
    }

    /// Record a failure and bench the account for a cooldown that depends on
    /// the failure kind. Auth failures bench immediately; ambiguous 5xx/network
    /// failures only bench once they repeat, so a one-off blip (or a
    /// model-specific 500) doesn't take a healthy account out of rotation.
    pub fn markFailure(acc: *Account, kind: FailureKind, status: u16) void {
        acc.consecutive_failures += 1;
        acc.last_status = status;
        const now = std.time.timestamp();
        acc.disabled_until = switch (kind) {
            // A credential that keeps failing auth is dead (expired/banned), not
            // a transient blip: give it a long cooldown so buildTryOrder ranks
            // it below any usable account and it stops wasting the first request.
            .auth => if (acc.consecutive_failures >= DEAD_CREDENTIAL_THRESHOLD)
                now + DEAD_CREDENTIAL_COOLDOWN_S
            else
                now + AUTH_COOLDOWN_S,
            .rate_limit => now + RATE_LIMIT_COOLDOWN_S,
            .transient => if (acc.consecutive_failures >= TRANSIENT_TRIP_THRESHOLD)
                now + TRANSIENT_COOLDOWN_S
            else
                0,
        };
    }

    /// Record an explicit model probe without conflating "not checked" with a
    /// healthy scheduler state. A successful real inference is strong enough
    /// to clear an earlier cooldown.
    pub fn markProbeSuccess(acc: *Account, latency_ms: i64) void {
        markSuccess(acc, 200);
        acc.last_probe_state = .healthy;
        acc.last_probe_at = std.time.timestamp();
        acc.last_probe_latency_ms = latency_ms;
    }

    /// Preserve the exact probe category for the UI, then feed the same result
    /// into the normal cooldown scheduler so unhealthy accounts lose priority.
    pub fn markProbeFailure(acc: *Account, state: ProbeState, kind: FailureKind, status: u16, latency_ms: i64) void {
        markFailure(acc, kind, status);
        acc.last_probe_state = state;
        acc.last_probe_at = std.time.timestamp();
        acc.last_probe_latency_ms = latency_ms;
    }

    /// Build the order in which accounts should be attempted for one request.
    /// Healthy accounts come first (current account leads if it is healthy),
    /// then benched accounts as a last resort, soonest-to-recover first, so
    /// even if every account is in cooldown we still try the least-bad one
    /// instead of hard-failing. Returns the number of entries written.
    pub fn buildTryOrder(self: *AccountManager, out: []usize) usize {
        const total = self.list.items.len;
        const count = @min(total, out.len);
        if (count == 0) return 0;

        var n: usize = 0;
        var seen = [_]bool{false} ** 64;

        // 1) Current account first, if healthy.
        if (self.current < total and isAvailable(&self.list.items[self.current])) {
            out[n] = self.current;
            if (self.current < seen.len) seen[self.current] = true;
            n += 1;
        }
        // 2) Remaining healthy accounts.
        for (0..total) |i| {
            if (n >= count) break;
            if (i < seen.len and seen[i]) continue;
            if (!isAvailable(&self.list.items[i])) continue;
            out[n] = i;
            if (i < seen.len) seen[i] = true;
            n += 1;
        }
        // 3) Benched accounts as fallback, soonest-to-recover first.
        while (n < count) {
            var best: ?usize = null;
            for (0..total) |i| {
                if (i < seen.len and seen[i]) continue;
                if (best == null or
                    self.list.items[i].disabled_until < self.list.items[best.?].disabled_until)
                    best = i;
            }
            const b = best orelse break;
            out[n] = b;
            if (b < seen.len) seen[b] = true;
            n += 1;
        }
        return n;
    }

    pub fn loadFromFile(self: *AccountManager) !void {
        const alloc = self.arena.allocator();
        const file = std.fs.cwd().openFile("accounts.json", .{}) catch return;
        defer file.close();

        const content = try file.readToEndAlloc(alloc, 4 * 1024 * 1024);
        const parsed = try std.json.parseFromSlice(std.json.Value, alloc, content, .{});
        const root = parsed.value;

        const accs_val = root.object.get("accounts") orelse return;
        var it = accs_val.object.iterator();
        while (it.next()) |entry| {
            const name = entry.key_ptr.*;
            const val = entry.value_ptr.*;
            if (val != .object) continue;

            const obj = val.object;
            const uid = blk: {
                const v = obj.get("user_id") orelse continue;
                break :blk switch (v) {
                    .string => |s| s,
                    .integer => |i| try std.fmt.allocPrint(alloc, "{d}", .{i}),
                    else => continue,
                };
            };

            const cred_val = obj.get("credential") orelse continue;
            const cred_json = std.json.Stringify.valueAlloc(alloc, cred_val, .{}) catch continue;

            // A missing or non-string orb_url is simply "not configured"; it must
            // never abort loading the rest of the account.
            const orb_url: ?[]const u8 = if (obj.get("orb_url")) |v| switch (v) {
                .string => |s| if (s.len > 0) s else null,
                else => null,
            } else null;

            self.list.append(self.allocator, .{
                .name = name,
                .user_id = uid,
                .credential_json = cred_json,
                .orb_url = orb_url,
            }) catch continue;
        }

        self.selectStartupCurrent();
    }
};

/// Account names and user_ids become JSON object keys; the credential is
/// inserted as a JSON value. Cap the keys and reject a credential that is not
/// valid JSON so one bad login can never leave accounts.json unparseable.
pub const MAX_ACCOUNT_FIELD_BYTES = 256;

pub fn addAccount(allocator: std.mem.Allocator, name: []const u8, user_id: []const u8, access_token_json: []const u8) !void {
    if (name.len > MAX_ACCOUNT_FIELD_BYTES or user_id.len > MAX_ACCOUNT_FIELD_BYTES)
        return error.AccountFieldTooLong;
    {
        const parsed_cred = std.json.parseFromSlice(std.json.Value, allocator, access_token_json, .{}) catch
            return error.InvalidCredentialJson;
        defer parsed_cred.deinit();
    }

    // Read existing. A file that fills the buffer cannot be re-emitted safely;
    // refuse instead of silently dropping every account it held.
    var buf: [4 * 1024 * 1024]u8 = undefined;
    var existing_content: ?[]const u8 = null;

    if (std.fs.cwd().openFile("accounts.json", .{})) |file| {
        defer file.close();
        const n = try file.readAll(&buf);
        if (n == buf.len) return error.AccountsFileTooLarge;
        existing_content = buf[0..n];
    } else |_| {}

    var output: std.io.Writer.Allocating = .init(allocator);
    defer output.deinit();
    const w = &output.writer;

    try w.writeAll("{\n  \"accounts\": {\n");
    var first = true;

    // Re-emit existing accounts, skipping any that collide with the new one
    // by name OR by user_id (dedup) so the same Zed account is never stored
    // twice and we never emit a duplicate JSON key.
    if (existing_content) |content| {
        const parsed = std.json.parseFromSlice(std.json.Value, allocator, content, .{}) catch null;
        if (parsed) |p| {
            defer p.deinit();
            if (p.value.object.get("accounts")) |accs| {
                if (accs == .object) {
                    var it = accs.object.iterator();
                    while (it.next()) |entry| {
                        const ekey = entry.key_ptr.*;
                        const eval = entry.value_ptr.*;

                        if (std.mem.eql(u8, ekey, name)) continue;

                        if (eval == .object) {
                            if (eval.object.get("user_id")) |uidv| {
                                var uid_buf: [32]u8 = undefined;
                                const euid = switch (uidv) {
                                    .string => |s| s,
                                    .integer => |iv| std.fmt.bufPrint(&uid_buf, "{d}", .{iv}) catch "",
                                    else => "",
                                };
                                if (std.mem.eql(u8, euid, user_id)) continue;
                            }
                        }

                        if (!first) try w.writeAll(",\n");
                        first = false;
                        const val_str = try std.json.Stringify.valueAlloc(allocator, eval, .{});
                        defer allocator.free(val_str);
                        try w.writeAll("    ");
                        try std.json.Stringify.encodeJsonString(ekey, .{}, w);
                        try w.print(": {s}", .{val_str});
                    }
                }
            }
        }
    }

    if (!first) try w.writeAll(",\n");
    try w.writeAll("    ");
    try std.json.Stringify.encodeJsonString(name, .{}, w);
    try w.writeAll(": {\"user_id\":");
    try std.json.Stringify.encodeJsonString(user_id, .{}, w);
    try w.print(",\"credential\":{s}}}", .{access_token_json});
    try w.writeAll("\n  }\n}");

    const file = try std.fs.cwd().createFile("accounts.json", .{ .mode = 0o600 });
    defer file.close();
    try file.writeAll(output.written());
}

/// Set (non-empty `url`) or clear (empty `url`) one account's Orb portal link.
/// Every other field of accounts.json is re-emitted untouched so saving a link
/// can never drop a credential. Returns error.AccountNotFound for an unknown
/// name.
pub fn setOrbUrl(allocator: std.mem.Allocator, name: []const u8, url: []const u8) !void {
    var buf: [4 * 1024 * 1024]u8 = undefined;
    const file = std.fs.cwd().openFile("accounts.json", .{}) catch return error.AccountNotFound;
    defer file.close();
    const n = try file.readAll(&buf);
    if (n == buf.len) return error.AccountsFileTooLarge;

    const parsed = std.json.parseFromSlice(std.json.Value, allocator, buf[0..n], .{}) catch
        return error.InvalidAccountsFile;
    defer parsed.deinit();
    if (parsed.value != .object) return error.InvalidAccountsFile;
    const accs = parsed.value.object.getPtr("accounts") orelse return error.AccountNotFound;
    if (accs.* != .object) return error.AccountNotFound;
    const entry = accs.object.getPtr(name) orelse return error.AccountNotFound;
    if (entry.* != .object) return error.AccountNotFound;

    if (url.len == 0) {
        _ = entry.object.orderedRemove("orb_url");
    } else {
        try entry.object.put("orb_url", .{ .string = url });
    }

    // Re-emitting the parsed tree drops this file's original formatting.
    // Everything else in it is a credential, so write the new content to a
    // 0600 sibling and rename it over the original; a crash or a failed write
    // leaves the old file intact.
    const output = try std.json.Stringify.valueAlloc(allocator, parsed.value, .{ .whitespace = .indent_2 });
    defer allocator.free(output);

    const tmp_file = try std.fs.cwd().createFile(ACCOUNTS_TMP_FILE, .{ .mode = 0o600 });
    tmp_file.writeAll(output) catch |err| {
        tmp_file.close();
        std.fs.cwd().deleteFile(ACCOUNTS_TMP_FILE) catch {};
        return err;
    };
    tmp_file.close();
    std.fs.cwd().rename(ACCOUNTS_TMP_FILE, "accounts.json") catch |err| {
        std.fs.cwd().deleteFile(ACCOUNTS_TMP_FILE) catch {};
        return err;
    };
}

// ── Tests ──

test "addAccount escapes JSON-special characters and reloads" {
    const allocator = std.testing.allocator;
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const cwd = try tmp.dir.realpathAlloc(allocator, ".");
    defer allocator.free(cwd);
    const original = try std.process.getCwdAlloc(allocator);
    defer allocator.free(original);
    try std.posix.chdir(cwd);
    defer std.posix.chdir(original) catch {};

    const name = "evil\"x,back\\slash\ttab\nz";
    const user_id = "42\"injected\":{\"x\":1}";
    try addAccount(allocator, name, user_id, "{\"access_token\":\"fake\"}");
    try addAccount(allocator, "plain", "7", "{\"access_token\":\"fake\"}");

    const content = try std.fs.cwd().readFileAlloc(allocator, "accounts.json", 1 << 20);
    defer allocator.free(content);
    const parsed = try std.json.parseFromSlice(std.json.Value, allocator, content, .{});
    defer parsed.deinit();
    const accs = parsed.value.object.get("accounts").?.object;
    try std.testing.expect(accs.get(name) != null);
    try std.testing.expectEqualStrings(user_id, accs.get(name).?.object.get("user_id").?.string);
    try std.testing.expect(accs.get("plain") != null);
}

test "accounts.json and active_account.txt are 0600 on POSIX" {
    if (comptime builtin.os.tag == .windows) return error.SkipZigTest;
    const allocator = std.testing.allocator;
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const cwd = try tmp.dir.realpathAlloc(allocator, ".");
    defer allocator.free(cwd);
    const original = try std.process.getCwdAlloc(allocator);
    defer allocator.free(original);
    try std.posix.chdir(cwd);
    defer std.posix.chdir(original) catch {};

    // Some filesystems (drvfs/9p mounts, FAT) silently drop mode bits. Probe
    // once so the assertion below only fires on a real permission regression.
    {
        const probe = try std.fs.cwd().createFile("mode_probe", .{ .mode = 0o600 });
        probe.close();
        const probe_mode = (try std.fs.cwd().statFile("mode_probe")).mode;
        if (probe_mode & 0o777 != 0o600) return error.SkipZigTest;
    }

    try addAccount(allocator, "plain", "7", "{\"access_token\":\"fake\"}");
    const accounts_mode = (try std.fs.cwd().statFile("accounts.json")).mode;
    try std.testing.expectEqual(@as(u32, 0o600), accounts_mode & 0o777);

    var mgr = AccountManager.init(allocator);
    defer mgr.deinit();
    try mgr.loadFromFile();
    mgr.setCurrent(0);
    const active_mode = (try std.fs.cwd().statFile(ACTIVE_ACCOUNT_FILE)).mode;
    try std.testing.expectEqual(@as(u32, 0o600), active_mode & 0o777);
}

test "orb_url round-trips through save, load, and a login rewrite" {
    const allocator = std.testing.allocator;
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const cwd = try tmp.dir.realpathAlloc(allocator, ".");
    defer allocator.free(cwd);
    const original = try std.process.getCwdAlloc(allocator);
    defer allocator.free(original);
    try std.posix.chdir(cwd);
    defer std.posix.chdir(original) catch {};

    try addAccount(allocator, "alice", "1", "{\"access_token\":\"fake\"}");
    const link = "https://portal.withorb.com/?token=fake-token-123";
    try setOrbUrl(allocator, "alice", link);
    try setOrbUrl(allocator, "alice", "https://portal.withorb.com/?token=fake-token-456");

    var mgr = AccountManager.init(allocator);
    defer mgr.deinit();
    try mgr.loadFromFile();
    try std.testing.expectEqual(@as(usize, 1), mgr.list.items.len);
    try std.testing.expectEqualStrings("https://portal.withorb.com/?token=fake-token-456", mgr.list.items[0].orb_url.?);
    try std.testing.expectEqualStrings("1", mgr.list.items[0].user_id);

    // A later login rewrites accounts.json; the stored link must survive it.
    try addAccount(allocator, "bob", "2", "{\"access_token\":\"fake2\"}");
    var reloaded = AccountManager.init(allocator);
    defer reloaded.deinit();
    try reloaded.loadFromFile();
    try std.testing.expectEqual(@as(usize, 2), reloaded.list.items.len);
    try std.testing.expectEqualStrings("https://portal.withorb.com/?token=fake-token-456", reloaded.list.items[0].orb_url.?);
    try std.testing.expect(reloaded.list.items[1].orb_url == null);

    // Clearing drops the field and leaves the rest of the account intact.
    try setOrbUrl(allocator, "alice", "");
    var cleared = AccountManager.init(allocator);
    defer cleared.deinit();
    try cleared.loadFromFile();
    try std.testing.expect(cleared.list.items[0].orb_url == null);
    try std.testing.expectEqualStrings("alice", cleared.list.items[0].name);
    try std.testing.expectEqualStrings("{\"access_token\":\"fake\"}", cleared.list.items[0].credential_json);

    try std.testing.expectError(error.AccountNotFound, setOrbUrl(allocator, "nobody", link));
}

test "addAccount rejects oversized keys and a non-JSON credential" {
    const allocator = std.testing.allocator;
    const long_name = "x" ** (MAX_ACCOUNT_FIELD_BYTES + 1);
    try std.testing.expectError(error.AccountFieldTooLong, addAccount(allocator, long_name, "1", "{}"));
    try std.testing.expectError(error.AccountFieldTooLong, addAccount(allocator, "ok", long_name, "{}"));
    try std.testing.expectError(error.InvalidCredentialJson, addAccount(allocator, "ok", "1", "not json"));
}
