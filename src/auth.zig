const std = @import("std");
const builtin = @import("builtin");

pub const Credentials = struct {
    user_id: []const u8,
    access_token: []const u8,
};

// ── Platform constants ──
const is_windows = builtin.os.tag == .windows;
const is_macos = builtin.os.tag == .macos;

// ── Windows BCrypt API ──
const BCRYPT_ALG_HANDLE = *anyopaque;
const BCRYPT_KEY_HANDLE = *anyopaque;
const NTSTATUS = i32;
const ULONG = u32;
const PUCHAR = [*]u8;

extern "bcrypt" fn BCryptOpenAlgorithmProvider(phAlgorithm: *BCRYPT_ALG_HANDLE, pszAlgId: [*:0]const u16, pszImplementation: ?*anyopaque, dwFlags: ULONG) callconv(.c) NTSTATUS;
extern "bcrypt" fn BCryptCloseAlgorithmProvider(hAlgorithm: BCRYPT_ALG_HANDLE, dwFlags: ULONG) callconv(.c) NTSTATUS;
extern "bcrypt" fn BCryptGenerateKeyPair(hAlgorithm: BCRYPT_ALG_HANDLE, phKey: *BCRYPT_KEY_HANDLE, dwLength: ULONG, dwFlags: ULONG) callconv(.c) NTSTATUS;
extern "bcrypt" fn BCryptFinalizeKeyPair(hKey: BCRYPT_KEY_HANDLE, dwFlags: ULONG) callconv(.c) NTSTATUS;
extern "bcrypt" fn BCryptExportKey(hKey: BCRYPT_KEY_HANDLE, hExportKey: ?BCRYPT_KEY_HANDLE, pszBlobType: [*:0]const u16, pbOutput: ?PUCHAR, cbOutput: ULONG, pcbResult: *ULONG, dwFlags: ULONG) callconv(.c) NTSTATUS;
extern "bcrypt" fn BCryptDecrypt(hKey: BCRYPT_KEY_HANDLE, pbInput: [*]const u8, cbInput: ULONG, pPaddingInfo: ?*anyopaque, pbIV: ?*anyopaque, cbIV: ULONG, pbOutput: ?PUCHAR, cbOutput: ULONG, pcbResult: *ULONG, dwFlags: ULONG) callconv(.c) NTSTATUS;
extern "bcrypt" fn BCryptDestroyKey(hKey: BCRYPT_KEY_HANDLE) callconv(.c) NTSTATUS;

const BCRYPT_RSAPUBLIC_BLOB_W = std.unicode.utf8ToUtf16LeStringLiteral("RSAPUBLICBLOB");
const BCRYPT_RSA_ALGORITHM_W = std.unicode.utf8ToUtf16LeStringLiteral("RSA");
const BCRYPT_PAD_OAEP: ULONG = 0x00000004;
const BCRYPT_PAD_PKCS1: ULONG = 0x00000002;
const BCRYPT_SHA256_W = std.unicode.utf8ToUtf16LeStringLiteral("SHA256");

const BCRYPT_OAEP_PADDING_INFO = extern struct {
    pszAlgId: [*:0]const u16,
    pbLabel: ?*anyopaque,
    cbLabel: ULONG,
};

const BCRYPT_RSAKEY_BLOB = extern struct {
    Magic: u32,
    BitLength: u32,
    cbPublicExp: u32,
    cbModulus: u32,
    cbPrime1: u32,
    cbPrime2: u32,
};

// ── RsaKeyPair: cross-platform RSA key management ──

pub const RsaKeyPair = struct {
    allocator: std.mem.Allocator,
    // Windows: BCrypt key handle
    key_handle: if (is_windows) ?BCRYPT_KEY_HANDLE else void,
    alg_handle: if (is_windows) ?BCRYPT_ALG_HANDLE else void,
    // Linux: PEM private key stored in memory
    private_key_pem: if (!is_windows) ?[]const u8 else void,

    pub fn generate(allocator: std.mem.Allocator) !RsaKeyPair {
        if (comptime is_windows) {
            return generateWindows(allocator);
        } else {
            return generateLinux(allocator);
        }
    }

    fn generateWindows(allocator: std.mem.Allocator) !RsaKeyPair {
        var alg: BCRYPT_ALG_HANDLE = undefined;
        var status = BCryptOpenAlgorithmProvider(&alg, BCRYPT_RSA_ALGORITHM_W, null, 0);
        if (status != 0) return error.CryptoError;

        var key: BCRYPT_KEY_HANDLE = undefined;
        status = BCryptGenerateKeyPair(alg, &key, 2048, 0);
        if (status != 0) {
            _ = BCryptCloseAlgorithmProvider(alg, 0);
            return error.CryptoError;
        }

        status = BCryptFinalizeKeyPair(key, 0);
        if (status != 0) {
            _ = BCryptDestroyKey(key);
            _ = BCryptCloseAlgorithmProvider(alg, 0);
            return error.CryptoError;
        }

        return .{
            .allocator = allocator,
            .key_handle = key,
            .alg_handle = alg,
            .private_key_pem = {},
        };
    }

    fn generateLinux(allocator: std.mem.Allocator) !RsaKeyPair {
        // Use openssl CLI to generate RSA keypair
        const result = std.process.Child.run(.{
            .allocator = allocator,
            .argv = &.{ "openssl", "genrsa", "2048" },
            .max_output_bytes = 8192,
        }) catch return error.CryptoError;
        defer allocator.free(result.stderr);

        if (result.term != .Exited or result.term.Exited != 0) {
            allocator.free(result.stdout);
            return error.CryptoError;
        }

        return .{
            .allocator = allocator,
            .key_handle = {},
            .alg_handle = {},
            .private_key_pem = result.stdout,
        };
    }

    pub fn deinit(self: *RsaKeyPair) void {
        if (comptime is_windows) {
            if (self.key_handle) |k| _ = BCryptDestroyKey(k);
            if (self.alg_handle) |a| _ = BCryptCloseAlgorithmProvider(a, 0);
            self.key_handle = null;
            self.alg_handle = null;
        } else {
            if (self.private_key_pem) |pem| {
                wipeSecret(pem);
                self.allocator.free(pem);
            }
            self.private_key_pem = null;
        }
    }

    /// Overwrite the PEM through a volatile pointer so the optimizer cannot
    /// drop the write, then hand the storage back.
    fn wipeSecret(pem: []const u8) void {
        const bytes: [*]volatile u8 = @constCast(pem.ptr);
        for (0..pem.len) |i| bytes[i] = 0;
    }

    /// Export public key as PKCS#1 DER, base64url encoded (no padding)
    pub fn exportPublicKeyB64(self: *RsaKeyPair, allocator: std.mem.Allocator) ![]const u8 {
        if (comptime is_windows) {
            return self.exportPublicKeyB64Windows(allocator);
        } else {
            return self.exportPublicKeyB64Linux(allocator);
        }
    }

    fn exportPublicKeyB64Windows(self: *RsaKeyPair, allocator: std.mem.Allocator) ![]const u8 {
        const key = self.key_handle orelse return error.NoKey;

        // Get export size
        var blob_size: ULONG = 0;
        var status = BCryptExportKey(key, null, BCRYPT_RSAPUBLIC_BLOB_W, null, 0, &blob_size, 0);
        if (status != 0) return error.CryptoError;

        const blob = try allocator.alloc(u8, blob_size);
        defer allocator.free(blob);

        var actual_size: ULONG = 0;
        status = BCryptExportKey(key, null, BCRYPT_RSAPUBLIC_BLOB_W, blob.ptr, blob_size, &actual_size, 0);
        if (status != 0) return error.CryptoError;

        // Parse BCRYPT_RSAKEY_BLOB to get exponent and modulus
        const header: *const BCRYPT_RSAKEY_BLOB = @ptrCast(@alignCast(blob.ptr));
        const exp_offset: usize = @sizeOf(BCRYPT_RSAKEY_BLOB);
        const mod_offset = exp_offset + header.cbPublicExp;
        const exponent = blob[exp_offset .. exp_offset + header.cbPublicExp];
        const modulus = blob[mod_offset .. mod_offset + header.cbModulus];

        // Encode as PKCS#1 DER
        const der = try encodePkcs1Der(allocator, modulus, exponent);
        defer allocator.free(der);

        // base64url encode (no padding)
        const encoder = std.base64.url_safe_no_pad.Encoder;
        const b64_len = encoder.calcSize(der.len);
        const b64 = try allocator.alloc(u8, b64_len);
        _ = encoder.encode(b64, der);
        return b64;
    }

    fn exportPublicKeyB64Linux(self: *RsaKeyPair, allocator: std.mem.Allocator) ![]const u8 {
        const pem = self.private_key_pem orelse return error.NoKey;

        // Feed the PEM through stdin: the private key never touches disk.
        // DER lands on stdout; openssl writes "writing RSA key" on stderr.
        const result = runWithStdin(allocator, &.{ "openssl", "rsa", "-in", "/dev/stdin", "-RSAPublicKey_out", "-outform", "DER" }, pem, 8192) catch return error.CryptoError;
        defer allocator.free(result.stdout);
        defer allocator.free(result.stderr);
        if (result.term != .Exited or result.term.Exited != 0) return error.CryptoError;
        const der = result.stdout;
        if (der.len == 0) return error.CryptoError;

        // base64url encode (no padding)
        const encoder = std.base64.url_safe_no_pad.Encoder;
        const b64_len = encoder.calcSize(der.len);
        const b64 = try allocator.alloc(u8, b64_len);
        _ = encoder.encode(b64, der);
        return b64;
    }

    /// Decrypt ciphertext using RSA-OAEP SHA-256
    pub fn decrypt(self: *RsaKeyPair, allocator: std.mem.Allocator, ciphertext: []const u8) ![]const u8 {
        if (comptime is_windows) {
            return self.decryptWindows(allocator, ciphertext);
        } else {
            return self.decryptLinux(allocator, ciphertext);
        }
    }

    fn decryptWindows(self: *RsaKeyPair, allocator: std.mem.Allocator, ciphertext: []const u8) ![]const u8 {
        const key = self.key_handle orelse return error.NoKey;

        var padding_info = BCRYPT_OAEP_PADDING_INFO{
            .pszAlgId = BCRYPT_SHA256_W,
            .pbLabel = null,
            .cbLabel = 0,
        };

        // Get output size
        var out_size: ULONG = 0;
        var status = BCryptDecrypt(key, ciphertext.ptr, @intCast(ciphertext.len), @ptrCast(&padding_info), null, 0, null, 0, &out_size, BCRYPT_PAD_OAEP);
        if (status != 0) return error.DecryptFailed;

        const output = try allocator.alloc(u8, out_size);
        var actual: ULONG = 0;
        status = BCryptDecrypt(key, ciphertext.ptr, @intCast(ciphertext.len), @ptrCast(&padding_info), null, 0, output.ptr, out_size, &actual, BCRYPT_PAD_OAEP);
        if (status != 0) {
            allocator.free(output);
            return error.DecryptFailed;
        }

        if (actual < out_size) {
            const trimmed = allocator.realloc(output, actual) catch return output;
            return trimmed;
        }
        return output;
    }

    fn decryptLinux(self: *RsaKeyPair, allocator: std.mem.Allocator, ciphertext: []const u8) ![]const u8 {
        const pem = self.private_key_pem orelse return error.NoKey;

        // The private key goes through stdin; only the (public, useless alone)
        // ciphertext needs a temp file, and it is zero-permission and unique.
        const tmp_enc = tempCiphertextPath(allocator) catch return error.DecryptFailed;
        defer allocator.free(tmp_enc);
        defer std.fs.cwd().deleteFile(tmp_enc) catch {};

        {
            const f = std.fs.cwd().createFile(tmp_enc, .{ .mode = 0o600 }) catch return error.DecryptFailed;
            defer f.close();
            f.writeAll(ciphertext) catch return error.DecryptFailed;
        }

        // Decrypt with OAEP SHA-256
        const result = runWithStdin(allocator, &.{ "openssl", "pkeyutl", "-decrypt", "-inkey", "/dev/stdin", "-in", tmp_enc, "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha256" }, pem, 8192) catch return error.DecryptFailed;
        defer allocator.free(result.stderr);
        defer allocator.free(result.stdout);

        if (result.term != .Exited or result.term.Exited != 0) return error.DecryptFailed;

        return try allocator.dupe(u8, result.stdout);
    }
};

/// Run an openssl subcommand with the given bytes on stdin. `Child.run`
/// leaves stdin ignored, so the pipe is driven by hand: write all, close,
/// drop the handle, collect stdout/stderr, then reap.
fn runWithStdin(allocator: std.mem.Allocator, argv: []const []const u8, stdin_data: []const u8, max_output: usize) !std.process.Child.RunResult {
    var child = std.process.Child.init(argv, allocator);
    child.stdin_behavior = .Pipe;
    child.stdout_behavior = .Pipe;
    child.stderr_behavior = .Pipe;
    try child.spawn();
    errdefer _ = child.kill() catch {};

    if (child.stdin) |stdin_file| {
        // A child that exits before reading (bad key) closes the pipe; the
        // write error is irrelevant next to its exit status.
        stdin_file.writeAll(stdin_data) catch {};
        stdin_file.close();
        child.stdin = null;
    }

    var stdout: std.ArrayList(u8) = .empty;
    errdefer stdout.deinit(allocator);
    var stderr: std.ArrayList(u8) = .empty;
    errdefer stderr.deinit(allocator);
    try child.collectOutput(allocator, &stdout, &stderr, max_output);

    return .{
        .term = try child.wait(),
        .stdout = try stdout.toOwnedSlice(allocator),
        .stderr = try stderr.toOwnedSlice(allocator),
    };
}

const temp_dirs = [_][]const u8{ "/tmp", "/var/tmp", "/dev/shm" };

/// Unique ciphertext scratch path in the first writable temp dir. The pid plus
/// an atomic counter keeps concurrent logins from sharing a file.
fn tempCiphertextPath(allocator: std.mem.Allocator) ![]u8 {
    const serial = temp_counter.fetchAdd(1, .monotonic);
    for (temp_dirs) |dir| {
        const path = try std.fmt.allocPrint(allocator, "{s}/zed-proxy_enc_{d}_{d}.bin", .{ dir, std.c.getpid(), serial });
        // Probe write access so a read-only /tmp on a locked-down container
        // falls through to the next candidate instead of failing decryption.
        var probe = std.fs.cwd().createFile(path, .{ .mode = 0o600 }) catch {
            allocator.free(path);
            continue;
        };
        probe.close();
        return path;
    }
    return error.TempDirUnavailable;
}

var temp_counter: std.atomic.Value(u64) = .init(0);

/// Decode the `access_token` query parameter: base64url, with or without
/// padding, into raw ciphertext bytes.
pub fn decodeCiphertext(allocator: std.mem.Allocator, enc_b64url: []const u8) ![]u8 {
    var input = std.mem.trim(u8, enc_b64url, " \t\r\n");
    while (input.len > 0 and input[input.len - 1] == '=') input = input[0 .. input.len - 1];

    const decoder = std.base64.url_safe_no_pad.Decoder;
    const decoded_len = decoder.calcSizeForSlice(input) catch return error.BadBase64;
    const ciphertext = try allocator.alloc(u8, decoded_len);
    errdefer allocator.free(ciphertext);
    decoder.decode(ciphertext, input) catch return error.BadBase64;
    return ciphertext;
}

// ── Tests ──
// Every test below drives the real openssl binary and fake credentials only.

const fake_probe_json = "{\"github_user_id\":4242,\"github_user_login\":\"fake-probe\",\"access_token\":\"fake-token\"}";

test "acceptBefore gives up at the deadline instead of blocking" {
    if (comptime is_windows) return error.SkipZigTest;

    var tcp = try std.net.Address.initIp4(.{ 127, 0, 0, 1 }, 0).listen(.{});
    defer tcp.deinit();

    const started = std.time.timestamp();
    try std.testing.expectError(error.DeadlineExceeded, acceptBefore(&tcp, started + 1));
    // One 1s slice, so the call returns promptly rather than waiting forever.
    try std.testing.expect(std.time.timestamp() - started <= 5);
}

test "decodeCiphertext tolerates padded and unpadded base64url" {
    const raw = "hello world!!";
    const padded = try encodeBase64(std.testing.allocator, std.base64.url_safe.Encoder, raw);
    defer std.testing.allocator.free(padded);
    const unpadded = try encodeBase64(std.testing.allocator, std.base64.url_safe_no_pad.Encoder, raw);
    defer std.testing.allocator.free(unpadded);

    const from_padded = try decodeCiphertext(std.testing.allocator, padded);
    defer std.testing.allocator.free(from_padded);
    const from_unpadded = try decodeCiphertext(std.testing.allocator, unpadded);
    defer std.testing.allocator.free(from_unpadded);

    try std.testing.expectEqualStrings(raw, from_padded);
    try std.testing.expectEqualStrings(raw, from_unpadded);
    try std.testing.expectError(error.BadBase64, decodeCiphertext(std.testing.allocator, "not*base64"));
}

test "exportPublicKeyB64 and decrypt round-trip through openssl OAEP-SHA256" {
    if (comptime is_windows) return error.SkipZigTest;

    const allocator = std.testing.allocator;
    var keypair = try RsaKeyPair.generate(allocator);
    defer keypair.deinit();

    const pub_b64 = try keypair.exportPublicKeyB64(allocator);
    defer allocator.free(pub_b64);

    const der = try decodeCiphertext(allocator, pub_b64);
    defer allocator.free(der);
    // PKCS#1 RSAPublicKey DER starts with SEQUENCE { INTEGER ... }.
    try std.testing.expectEqual(@as(u8, 0x30), der[0]);

    const ciphertext = try encryptWithPublicDer(allocator, der, fake_probe_json);
    defer allocator.free(ciphertext);
    try std.testing.expectEqual(@as(usize, 256), ciphertext.len);

    const plaintext = try keypair.decrypt(allocator, ciphertext);
    defer allocator.free(plaintext);
    try std.testing.expectEqualStrings(fake_probe_json, plaintext);
}

test "decrypt rejects a foreign key and a tampered ciphertext" {
    if (comptime is_windows) return error.SkipZigTest;

    const allocator = std.testing.allocator;
    var owner = try RsaKeyPair.generate(allocator);
    defer owner.deinit();
    const owner_pub = try owner.exportPublicKeyB64(allocator);
    defer allocator.free(owner_pub);
    const owner_der = try decodeCiphertext(allocator, owner_pub);
    defer allocator.free(owner_der);

    const ciphertext = try encryptWithPublicDer(allocator, owner_der, fake_probe_json);
    defer allocator.free(ciphertext);

    var stranger = try RsaKeyPair.generate(allocator);
    defer stranger.deinit();
    try std.testing.expectError(error.DecryptFailed, stranger.decrypt(allocator, ciphertext));

    const tampered = try allocator.dupe(u8, ciphertext);
    defer allocator.free(tampered);
    tampered[tampered.len / 2] ^= 0xff;
    try std.testing.expectError(error.DecryptFailed, owner.decrypt(allocator, tampered));

    // A short blob must fail the same way, not crash.
    try std.testing.expectError(error.DecryptFailed, owner.decrypt(allocator, "short"));
}

test "key export and decryption leave no private PEM in the working directory" {
    if (comptime is_windows) return error.SkipZigTest;

    const allocator = std.testing.allocator;
    // Run in a fresh temp dir so the check covers only this test's files.
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const cwd = try tmp.dir.realpathAlloc(allocator, ".");
    defer allocator.free(cwd);
    const original = try std.process.getCwdAlloc(allocator);
    defer allocator.free(original);
    try std.posix.chdir(cwd);
    defer std.posix.chdir(original) catch {};

    var keypair = try RsaKeyPair.generate(allocator);
    defer keypair.deinit();
    const pub_b64 = try keypair.exportPublicKeyB64(allocator);
    defer allocator.free(pub_b64);
    const der = try decodeCiphertext(allocator, pub_b64);
    defer allocator.free(der);
    const ciphertext = try encryptWithPublicDer(allocator, der, fake_probe_json);
    defer allocator.free(ciphertext);
    const plaintext = try keypair.decrypt(allocator, ciphertext);
    defer allocator.free(plaintext);

    try std.testing.expectEqualStrings(fake_probe_json, plaintext);

    // Ciphertext scratch files contain no key material, but must not survive.
    var scan_dir = try std.fs.cwd().openDir(cwd, .{ .iterate = true });
    defer scan_dir.close();
    var it = scan_dir.iterate();
    while (try it.next()) |entry| try std.testing.expect(std.mem.indexOf(u8, entry.name, "zed-proxy") == null);
}

test "wipeSecret zeroes the private PEM buffer in place" {
    // Freeing poison-fills memory in debug builds, so the wipe itself (not
    // deinit's free) is what this test can observe.
    var storage: [64]u8 = @splat(0xAA);
    RsaKeyPair.wipeSecret(&storage);
    for (storage) |byte| try std.testing.expectEqual(@as(u8, 0), byte);
}

fn encodeBase64(allocator: std.mem.Allocator, encoder: std.base64.Base64Encoder, raw: []const u8) ![]u8 {
    const out = try allocator.alloc(u8, encoder.calcSize(raw.len));
    _ = encoder.encode(out, raw);
    return out;
}

/// Encrypt test plaintext with an exported DER key. The public key rides on
/// stdin, so the (fake) message goes through a scratch file.
fn encryptWithPublicDer(allocator: std.mem.Allocator, der: []const u8, plaintext: []const u8) ![]u8 {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "plain.json", .data = plaintext });
    const plain_path = try tmp.dir.realpathAlloc(allocator, "plain.json");
    defer allocator.free(plain_path);

    const result = try runWithStdin(allocator, &.{ "openssl", "pkeyutl", "-encrypt", "-pubin", "-inkey", "/dev/stdin", "-keyform", "DER", "-in", plain_path, "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha256" }, der, 4096);
    defer allocator.free(result.stderr);
    if (result.term != .Exited or result.term.Exited != 0) {
        allocator.free(result.stdout);
        return error.EncryptFailed;
    }
    return result.stdout;
}

// ── DER encoding helpers for PKCS#1 RSAPublicKey ──

fn encodePkcs1Der(allocator: std.mem.Allocator, modulus: []const u8, exponent: []const u8) ![]const u8 {
    // PKCS#1 RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }
    var buf: std.ArrayListUnmanaged(u8) = .empty;
    defer buf.deinit(allocator);
    const w = buf.writer(allocator);

    // Encode inner: modulus INTEGER + exponent INTEGER
    var inner: std.ArrayListUnmanaged(u8) = .empty;
    defer inner.deinit(allocator);
    const iw = inner.writer(allocator);
    try derWriteInteger(iw, modulus);
    try derWriteInteger(iw, exponent);

    // SEQUENCE tag + length + inner
    try w.writeByte(0x30);
    try derWriteLength(w, inner.items.len);
    try w.writeAll(inner.items);

    return try allocator.dupe(u8, buf.items);
}

fn derWriteInteger(w: anytype, data: []const u8) !void {
    // Skip leading zeros but keep at least one byte
    var start: usize = 0;
    while (start < data.len - 1 and data[start] == 0) start += 1;
    const trimmed = data[start..];

    // If high bit set, prepend 0x00
    const needs_pad = (trimmed[0] & 0x80) != 0;
    const total_len = trimmed.len + @as(usize, if (needs_pad) 1 else 0);

    try w.writeByte(0x02); // INTEGER tag
    try derWriteLength(w, total_len);
    if (needs_pad) try w.writeByte(0x00);
    try w.writeAll(trimmed);
}

fn derWriteLength(w: anytype, length: usize) !void {
    if (length < 128) {
        try w.writeByte(@intCast(length));
    } else if (length < 256) {
        try w.writeByte(0x81);
        try w.writeByte(@intCast(length));
    } else {
        try w.writeByte(0x82);
        try w.writeByte(@intCast(length >> 8));
        try w.writeByte(@intCast(length & 0xFF));
    }
}

// ── Browser launch ──

pub fn openBrowserPublic(url: []const u8) void {
    openBrowser(url);
}

fn openBrowser(url: []const u8) void {
    if (comptime is_windows) {
        openBrowserWindows(url);
    } else if (comptime is_macos) {
        openBrowserMacOS(url);
    } else {
        openBrowserLinux(url);
    }
}

fn openBrowserMacOS(url: []const u8) void {
    const result = std.process.Child.run(.{
        .allocator = std.heap.page_allocator,
        .argv = &.{ "/usr/bin/open", url },
        .max_output_bytes = 256,
    }) catch return;
    std.heap.page_allocator.free(result.stdout);
    std.heap.page_allocator.free(result.stderr);
}

fn openBrowserWindows(url: []const u8) void {
    // Try Chrome incognito first
    _ = std.process.Child.run(.{
        .allocator = std.heap.page_allocator,
        .argv = &.{ "cmd", "/c", "start", "chrome", "--incognito", url },
        .max_output_bytes = 256,
    }) catch {
        // Try Edge inprivate
        _ = std.process.Child.run(.{
            .allocator = std.heap.page_allocator,
            .argv = &.{ "cmd", "/c", "start", "msedge", "--inprivate", url },
            .max_output_bytes = 256,
        }) catch {
            // Try Firefox private
            _ = std.process.Child.run(.{
                .allocator = std.heap.page_allocator,
                .argv = &.{ "cmd", "/c", "start", "firefox", "--private-window", url },
                .max_output_bytes = 256,
            }) catch {
                // Fallback: rundll32 (no private mode but at least opens)
                _ = std.process.Child.run(.{
                    .allocator = std.heap.page_allocator,
                    .argv = &.{ "rundll32", "url.dll,FileProtocolHandler", url },
                    .max_output_bytes = 256,
                }) catch {};
            };
        };
    };
}

fn openBrowserLinux(url: []const u8) void {
    // Try Chrome incognito first
    _ = std.process.Child.run(.{
        .allocator = std.heap.page_allocator,
        .argv = &.{ "google-chrome", "--incognito", url },
        .max_output_bytes = 256,
    }) catch {
        // Try Chromium incognito
        _ = std.process.Child.run(.{
            .allocator = std.heap.page_allocator,
            .argv = &.{ "chromium", "--incognito", url },
            .max_output_bytes = 256,
        }) catch {
            // Try Firefox private
            _ = std.process.Child.run(.{
                .allocator = std.heap.page_allocator,
                .argv = &.{ "firefox", "--private-window", url },
                .max_output_bytes = 256,
            }) catch {
                // Fallback: xdg-open
                _ = std.process.Child.run(.{
                    .allocator = std.heap.page_allocator,
                    .argv = &.{ "xdg-open", url },
                    .max_output_bytes = 256,
                }) catch {};
            };
        };
    };
}

// ── Platform-adapted socket I/O for auth callback server ──

/// `Server.accept` with a deadline: poll the listening socket in short slices
/// so the caller can stop waiting instead of holding the login slot forever.
fn acceptBefore(tcp: *std.net.Server, deadline: i64) error{ DeadlineExceeded, AcceptFailed }!std.net.Server.Connection {
    while (true) {
        const remaining = deadline - std.time.timestamp();
        if (remaining <= 0) return error.DeadlineExceeded;
        const slice_s: u32 = @intCast(@min(remaining, 5));
        var fds = [1]std.posix.pollfd{.{ .fd = tcp.stream.handle, .events = std.posix.POLL.IN, .revents = 0 }};
        const ready = std.posix.poll(&fds, @intCast(slice_s * std.time.ms_per_s)) catch return error.AcceptFailed;
        if (ready == 0) continue;
        return tcp.accept() catch return error.AcceptFailed;
    }
}

fn authSocketRecv(stream: std.net.Stream, buf: []u8) !usize {
    if (comptime is_windows) {
        const ws2 = std.os.windows.ws2_32;
        const rc = ws2.recv(stream.handle, buf.ptr, @intCast(buf.len), 0);
        if (rc == ws2.SOCKET_ERROR) return error.RecvFailed;
        if (rc == 0) return 0;
        return @intCast(rc);
    } else {
        return stream.read(buf);
    }
}

fn authSocketSend(stream: std.net.Stream, data: []const u8) !void {
    if (comptime is_windows) {
        const ws2 = std.os.windows.ws2_32;
        var sent: usize = 0;
        while (sent < data.len) {
            const rc = ws2.send(stream.handle, data[sent..].ptr, @intCast(data.len - sent), 0);
            if (rc == ws2.SOCKET_ERROR) return error.SendFailed;
            sent += @intCast(rc);
        }
    } else {
        _ = try stream.write(data);
    }
}

// ── URL decoding ──

fn urlDecode(allocator: std.mem.Allocator, input: []const u8) ![]const u8 {
    var result: std.ArrayListUnmanaged(u8) = .empty;
    defer result.deinit(allocator);
    const w = result.writer(allocator);

    var i: usize = 0;
    while (i < input.len) {
        if (input[i] == '%' and i + 2 < input.len) {
            const hi = hexVal(input[i + 1]) orelse {
                try w.writeByte(input[i]);
                i += 1;
                continue;
            };
            const lo = hexVal(input[i + 2]) orelse {
                try w.writeByte(input[i]);
                i += 1;
                continue;
            };
            try w.writeByte((hi << 4) | lo);
            i += 3;
        } else if (input[i] == '+') {
            try w.writeByte(' ');
            i += 1;
        } else {
            try w.writeByte(input[i]);
            i += 1;
        }
    }
    return try allocator.dupe(u8, result.items);
}

fn hexVal(c: u8) ?u8 {
    if (c >= '0' and c <= '9') return c - '0';
    if (c >= 'a' and c <= 'f') return c - 'a' + 10;
    if (c >= 'A' and c <= 'F') return c - 'A' + 10;
    return null;
}

// ── Login flow ──

/// CLI login: generate keypair, start local server, open browser, wait for callback
pub fn login(allocator: std.mem.Allocator) !Credentials {
    var keypair = try RsaKeyPair.generate(allocator);
    defer keypair.deinit();

    const pub_key = try keypair.exportPublicKeyB64(allocator);
    defer allocator.free(pub_key);

    const addr = std.net.Address.initIp4(.{ 127, 0, 0, 1 }, 0);
    var tcp = try addr.listen(.{});
    defer tcp.deinit();
    const port = tcp.listen_address.getPort();

    const url = try std.fmt.allocPrint(allocator, "https://zed.dev/native_app_signin?native_app_port={d}&native_app_public_key={s}", .{ port, pub_key });
    defer allocator.free(url);

    std.debug.print("[login] opening browser...\n  {s}\n", .{url});
    openBrowser(url);

    std.debug.print("[login] waiting for callback on port {d}...\n", .{port});
    return loginWithServer(allocator, &keypair, &tcp, std.time.timestamp() + LEGACY_LOGIN_TIMEOUT_S);
}

/// CLI logins must not wait forever either; same bound as the server flow.
const LEGACY_LOGIN_TIMEOUT_S: i64 = 600;

/// Wait for OAuth callback on an already-listening TCP server. `deadline` is a
/// Unix timestamp; the callback server gives up at that point instead of
/// blocking the process's single login slot forever.
pub fn loginWithServer(allocator: std.mem.Allocator, keypair: *RsaKeyPair, tcp: *std.net.Server, deadline: i64) !Credentials {
    const conn = acceptBefore(tcp, deadline) catch |err| switch (err) {
        error.DeadlineExceeded => return error.LoginTimedOut,
        else => return error.AcceptFailed,
    };
    defer conn.stream.close();

    var buf: [8192]u8 = undefined;
    var total: usize = 0;
    while (total < buf.len) {
        const n = authSocketRecv(conn.stream, buf[total..]) catch break;
        if (n == 0) break;
        total += n;
        if (std.mem.indexOf(u8, buf[0..total], "\r\n\r\n") != null) break;
    }

    const data = buf[0..total];
    // Parse GET /?user_id=...&access_token=...
    const first_line_end = std.mem.indexOf(u8, data, "\r\n") orelse return error.BadCallback;
    const first_line = data[0..first_line_end];

    // Extract path from "GET /path HTTP/1.1"
    var parts = std.mem.splitScalar(u8, first_line, ' ');
    _ = parts.next(); // GET
    const full_path = parts.next() orelse return error.BadCallback;

    // Parse query string
    const query_start = std.mem.indexOf(u8, full_path, "?") orelse return error.BadCallback;
    const query = full_path[query_start + 1 ..];

    var user_id: ?[]const u8 = null;
    var encrypted_token: ?[]const u8 = null;
    defer if (user_id) |u| allocator.free(u);

    var query_parts = std.mem.splitScalar(u8, query, '&');
    while (query_parts.next()) |param| {
        const eq = std.mem.indexOf(u8, param, "=") orelse continue;
        const key = param[0..eq];
        const val = param[eq + 1 ..];
        if (std.mem.eql(u8, key, "user_id")) {
            user_id = try urlDecode(allocator, val);
        } else if (std.mem.eql(u8, key, "access_token")) {
            encrypted_token = try urlDecode(allocator, val);
        }
    }

    const uid = user_id orelse return error.NoUserId;
    const enc_tok = encrypted_token orelse return error.NoToken;

    const ciphertext = try decodeCiphertext(allocator, enc_tok);
    defer allocator.free(ciphertext);

    // Decrypt
    const plaintext = try keypair.decrypt(allocator, ciphertext);
    defer allocator.free(plaintext);

    // Send redirect response
    const redirect = "HTTP/1.1 302 Found\r\nLocation: https://zed.dev/native_app_signin_succeeded\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    authSocketSend(conn.stream, redirect) catch {};

    // plaintext is the access_token JSON string
    const result_uid = try allocator.dupe(u8, uid);
    const result_token = try allocator.dupe(u8, plaintext);

    std.debug.print("[login] success! user_id: {s}\n", .{result_uid});

    return .{
        .user_id = result_uid,
        .access_token = result_token,
    };
}
