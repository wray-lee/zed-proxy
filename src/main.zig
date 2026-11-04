const std = @import("std");
const server = @import("server.zig");
const accounts = @import("accounts.zig");
const auth = @import("auth.zig");

const DEFAULT_HOST = "127.0.0.1";
const DEFAULT_PORT: u16 = 8001;

pub fn main() !void {
    var gpa = std.heap.GeneralPurposeAllocator(.{}){};
    defer _ = gpa.deinit();
    const allocator = gpa.allocator();

    var args = try std.process.argsWithAllocator(allocator);
    defer args.deinit();
    _ = args.skip();

    const cmd = args.next() orelse "serve";

    // Opt-in: when set, move to the data dir before any AccountManager exists,
    // so accounts.json / active_account.txt / login temp files land in a
    // mountable directory instead of the repo. Unset/empty keeps old behavior.
    try applyDataDir(allocator);

    if (std.mem.eql(u8, cmd, "serve")) {
        const host_owned = std.process.getEnvVarOwned(allocator, "ZED_API_HOST") catch null;
        defer if (host_owned) |value| allocator.free(value);
        const host = if (host_owned) |value| if (value.len > 0) value else DEFAULT_HOST else DEFAULT_HOST;

        const port_owned = std.process.getEnvVarOwned(allocator, "ZED_API_PORT") catch null;
        defer if (port_owned) |value| allocator.free(value);
        const env_port = if (port_owned) |value|
            (if (value.len > 0) std.fmt.parseInt(u16, value, 10) catch DEFAULT_PORT else DEFAULT_PORT)
        else
            DEFAULT_PORT;

        // Positional `serve [port]` wins over ZED_API_PORT, which wins over 8001.
        const port = if (args.next()) |port_str| std.fmt.parseInt(u16, port_str, 10) catch DEFAULT_PORT else env_port;

        try server.run(allocator, host, port);
    } else if (std.mem.eql(u8, cmd, "login")) {
        const name = args.next();
        try doLogin(allocator, name);
    } else if (std.mem.eql(u8, cmd, "accounts")) {
        try listAccounts(allocator);
    } else {
        printUsage();
    }
}

fn applyDataDir(allocator: std.mem.Allocator) !void {
    const data_dir = std.process.getEnvVarOwned(allocator, "ZED_DATA_DIR") catch return;
    defer allocator.free(data_dir);
    if (data_dir.len == 0) return;
    std.fs.cwd().makePath(data_dir) catch |err| {
        std.debug.print("[zed-proxy] cannot create ZED_DATA_DIR '{s}': {}\n", .{ data_dir, err });
        return err;
    };
    try std.posix.chdir(data_dir);
}

fn printUsage() void {
    std.debug.print(
        \\zed-proxy - Zed LLM API Proxy
        \\
        \\Usage:
        \\  zed-proxy serve [port]    Start API server (default: 8001)
        \\  zed-proxy login [name]    Login via GitHub OAuth
        \\  zed-proxy accounts        List configured accounts
        \\
        \\Environment:
        \\  ZED_API_HOST     Bind address (default: 127.0.0.1)
        \\  ZED_API_PORT     Bind port (default: 8001; positional port wins)
        \\  ZED_DATA_DIR     Directory for accounts.json and login state
        \\  ZED_API_KEY      Comma/newline separated gateway API keys
        \\  ZED_API_KEY_FILE File containing gateway API keys
        \\  ZED_LOGIN_PORT   Paste-login callback port (default: 58443)
        \\  ZED_SYSTEM_ID    x-zed-system-id to send (default: none)
        \\
        \\Endpoints:
        \\  GET  /health                Liveness probe (no auth)
        \\  POST /v1/responses          OpenAI Responses (Codex, stream/non-stream)
        \\  POST /v1/chat/completions   OpenAI compatible
        \\  POST /v1/messages           Anthropic native
        \\  GET  /v1/models             List models
        \\  GET  /zed/accounts          List accounts
        \\  GET  /zed/accounts/status   Passive token/quota status (no model call)
        \\  POST /zed/accounts/health   Low-cost single/all account model probe
        \\  POST /zed/login             Start OAuth login (mode=paste for paste)
        \\  POST /zed/login/complete    Submit the pasted callback URL
        \\  GET  /zed/login/status      Poll login state
        \\  GET  /                      Web UI
        \\
    , .{});
}

fn doLogin(allocator: std.mem.Allocator, name: ?[]const u8) !void {
    const creds = try auth.login(allocator);
    defer allocator.free(creds.user_id);
    defer allocator.free(creds.access_token);

    std.debug.print("\n[ok] user_id: {s}\n", .{creds.user_id});

    const account_name = name orelse creds.user_id;
    try accounts.addAccount(allocator, account_name, creds.user_id, creds.access_token);
    std.debug.print("[ok] saved to accounts.json as '{s}'\n", .{account_name});
}

fn listAccounts(allocator: std.mem.Allocator) !void {
    var mgr = accounts.AccountManager.init(allocator);
    defer mgr.deinit();
    mgr.loadFromFile() catch {};

    if (mgr.list.items.len == 0) {
        std.debug.print("No accounts. Run: zed-proxy login\n", .{});
        return;
    }
    for (mgr.list.items) |acc| {
        std.debug.print("  {s} (uid: {s})\n", .{ acc.name, acc.user_id });
    }
}
