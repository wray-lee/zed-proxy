const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    // WebUI build step skipped: embedding the pre-built webui/dist/index.html
    // (avoids needing webui/node_modules for tsc + vite).

    // Zig module
    const mod = b.createModule(.{
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    mod.addAnonymousImport("web_index_html", .{ .root_source_file = b.path("webui/dist/index.html") });

    const exe = b.addExecutable(.{
        .name = "zed-proxy",
        .root_module = mod,
    });

    // WebUI is pre-built; HTML embedded directly from webui/dist/index.html.

    if (target.result.os.tag == .windows) {
        exe.root_module.linkSystemLibrary("bcrypt", .{});
        exe.root_module.linkSystemLibrary("advapi32", .{});
        exe.root_module.linkSystemLibrary("crypt32", .{});
        exe.root_module.linkSystemLibrary("ws2_32", .{});
    }

    b.installArtifact(exe);

    const run_cmd = b.addRunArtifact(exe);
    run_cmd.step.dependOn(b.getInstallStep());
    if (b.args) |args| {
        run_cmd.addArgs(args);
    }

    const run_step = b.step("run", "Run zed-proxy server");
    run_step.dependOn(&run_cmd.step);

    // Keep request conversion, streaming, upstream status parsing, and health
    // probe behavior executable through the standard `zig build test` command.
    const providers_test_mod = b.createModule(.{
        .root_source_file = b.path("src/providers.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    const providers_tests = b.addTest(.{ .root_module = providers_test_mod });
    const run_providers_tests = b.addRunArtifact(providers_tests);

    const stream_test_mod = b.createModule(.{
        .root_source_file = b.path("src/stream.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    const stream_tests = b.addTest(.{ .root_module = stream_test_mod });
    if (target.result.os.tag == .windows) {
        stream_tests.root_module.linkSystemLibrary("bcrypt", .{});
        stream_tests.root_module.linkSystemLibrary("advapi32", .{});
        stream_tests.root_module.linkSystemLibrary("crypt32", .{});
        stream_tests.root_module.linkSystemLibrary("ws2_32", .{});
    }
    const run_stream_tests = b.addRunArtifact(stream_tests);

    // Upstream status-message handling lives here: Zed reports rejected
    // requests as HTTP 200 plus a `status.failed` line, so the parser that
    // recognizes them needs its own coverage.
    const proxy_test_mod = b.createModule(.{
        .root_source_file = b.path("src/proxy.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    const proxy_tests = b.addTest(.{ .root_module = proxy_test_mod });
    if (target.result.os.tag == .windows) {
        proxy_tests.root_module.linkSystemLibrary("advapi32", .{});
        proxy_tests.root_module.linkSystemLibrary("ws2_32", .{});
    }
    const run_proxy_tests = b.addRunArtifact(proxy_tests);

    const server_test_mod = b.createModule(.{
        .root_source_file = b.path("src/server.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    // `zig build test` must also run on a fresh clone where the generated
    // webui/dist/index.html does not exist yet: it is produced by `npm run
    // build` and never committed. Tests never read the page, so substitute a
    // placeholder file; the executable above still requires the real artifact.
    var server_web_index: std.Build.LazyPath = b.path("webui/dist/index.html");
    b.build_root.handle.access("webui/dist/index.html", .{}) catch {
        const placeholder = b.addWriteFiles();
        server_web_index = placeholder.add("index.html", "<!doctype html><title>zed-proxy</title>");
    };
    server_test_mod.addAnonymousImport("web_index_html", .{ .root_source_file = server_web_index });
    const server_tests = b.addTest(.{ .root_module = server_test_mod });
    if (target.result.os.tag == .windows) {
        server_tests.root_module.linkSystemLibrary("bcrypt", .{});
        server_tests.root_module.linkSystemLibrary("advapi32", .{});
        server_tests.root_module.linkSystemLibrary("crypt32", .{});
        server_tests.root_module.linkSystemLibrary("ws2_32", .{});
    }
    const run_server_tests = b.addRunArtifact(server_tests);

    // Gateway auth rules (key loading, header parsing, bind policy) are pure
    // functions; they are tested without opening a socket.
    const gateway_auth_test_mod = b.createModule(.{
        .root_source_file = b.path("src/gateway_auth.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    const gateway_auth_tests = b.addTest(.{ .root_module = gateway_auth_test_mod });
    if (target.result.os.tag == .windows) {
        gateway_auth_tests.root_module.linkSystemLibrary("bcrypt", .{});
        gateway_auth_tests.root_module.linkSystemLibrary("advapi32", .{});
        gateway_auth_tests.root_module.linkSystemLibrary("crypt32", .{});
        gateway_auth_tests.root_module.linkSystemLibrary("ws2_32", .{});
    }
    const run_gateway_auth_tests = b.addRunArtifact(gateway_auth_tests);

    // Paste-callback parsing and the single-login slot are pure functions.
    const zed_login_test_mod = b.createModule(.{
        .root_source_file = b.path("src/zed_login.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    const zed_login_tests = b.addTest(.{ .root_module = zed_login_test_mod });
    if (target.result.os.tag == .windows) {
        zed_login_tests.root_module.linkSystemLibrary("bcrypt", .{});
        zed_login_tests.root_module.linkSystemLibrary("advapi32", .{});
        zed_login_tests.root_module.linkSystemLibrary("crypt32", .{});
        zed_login_tests.root_module.linkSystemLibrary("ws2_32", .{});
    }
    const run_zed_login_tests = b.addRunArtifact(zed_login_tests);

    // accounts.json writing: name/user_id are JSON keys, so escaping and the
    // length/non-JSON-credential guards need a real temp file round-trip.
    const accounts_test_mod = b.createModule(.{
        .root_source_file = b.path("src/accounts.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    const accounts_tests = b.addTest(.{ .root_module = accounts_test_mod });
    if (target.result.os.tag == .windows) {
        accounts_tests.root_module.linkSystemLibrary("bcrypt", .{});
        accounts_tests.root_module.linkSystemLibrary("advapi32", .{});
        accounts_tests.root_module.linkSystemLibrary("crypt32", .{});
        accounts_tests.root_module.linkSystemLibrary("ws2_32", .{});
    }
    const run_accounts_tests = b.addRunArtifact(accounts_tests);

    // Orb portal link validation and ledger parsing are pure functions; they
    // run on invented JSON without touching the network.
    const orb_quota_test_mod = b.createModule(.{
        .root_source_file = b.path("src/orb_quota.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    const orb_quota_tests = b.addTest(.{ .root_module = orb_quota_test_mod });
    if (target.result.os.tag == .windows) {
        orb_quota_tests.root_module.linkSystemLibrary("bcrypt", .{});
        orb_quota_tests.root_module.linkSystemLibrary("advapi32", .{});
        orb_quota_tests.root_module.linkSystemLibrary("crypt32", .{});
        orb_quota_tests.root_module.linkSystemLibrary("ws2_32", .{});
    }
    const run_orb_quota_tests = b.addRunArtifact(orb_quota_tests);

    // RSA keypair round-trip drives the installed openssl binary.
    const auth_test_mod = b.createModule(.{
        .root_source_file = b.path("src/auth.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
    });
    const auth_tests = b.addTest(.{ .root_module = auth_test_mod });
    if (target.result.os.tag == .windows) {
        auth_tests.root_module.linkSystemLibrary("bcrypt", .{});
        auth_tests.root_module.linkSystemLibrary("advapi32", .{});
        auth_tests.root_module.linkSystemLibrary("crypt32", .{});
        auth_tests.root_module.linkSystemLibrary("ws2_32", .{});
    }
    const run_auth_tests = b.addRunArtifact(auth_tests);

    const test_step = b.step("test", "Run protocol, streaming, and health regression tests");
    test_step.dependOn(&run_providers_tests.step);
    test_step.dependOn(&run_stream_tests.step);
    test_step.dependOn(&run_proxy_tests.step);
    test_step.dependOn(&run_server_tests.step);
    test_step.dependOn(&run_gateway_auth_tests.step);
    test_step.dependOn(&run_zed_login_tests.step);
    test_step.dependOn(&run_accounts_tests.step);
    test_step.dependOn(&run_orb_quota_tests.step);
    test_step.dependOn(&run_auth_tests.step);
}
