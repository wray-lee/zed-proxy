const std = @import("std");

// The model slugs Zed's /models lists. The gateway takes its model list from
// upstream; this registry only lets the request path ask "does upstream serve
// this slug?" so a listed slug is forwarded unchanged. It is refreshed whenever
// the catalog is fetched and seeded from the embedded fallback catalog, so it is
// usable before the first fetch.

const alloc = std.heap.page_allocator;

var mutex: std.Thread.Mutex = .{};
var seeded = false;
// slug -> max_output_tokens (0 when upstream did not say)
var slugs: std.StringHashMapUnmanaged(i64) = .empty;

/// `gpt-5.6` is a local alias for Sol, not an upstream model, so it never
/// counts as a listed slug.
pub const SOL_ALIAS = "gpt-5.6";

fn replace(next: std.StringHashMapUnmanaged(i64)) void {
    var old = slugs;
    slugs = next;
    var it = old.keyIterator();
    while (it.next()) |key| alloc.free(key.*);
    old.deinit(alloc);
    seeded = true;
}

fn add(map: *std.StringHashMapUnmanaged(i64), id: []const u8, max_output: i64) void {
    if (id.len == 0 or std.mem.eql(u8, id, SOL_ALIAS)) return;
    const key = alloc.dupe(u8, id) catch return;
    map.put(alloc, key, max_output) catch alloc.free(key);
}

fn intField(obj: std.json.ObjectMap, name: []const u8) i64 {
    const value = obj.get(name) orelse return 0;
    return if (value == .integer) value.integer else 0;
}

/// Replace the registry from Zed's raw `/models` response (`models[].id`).
/// An unparsable or empty response leaves the current registry alone.
pub fn refreshFromZed(raw: []const u8) void {
    var arena = std.heap.ArenaAllocator.init(alloc);
    defer arena.deinit();
    const parsed = std.json.parseFromSlice(std.json.Value, arena.allocator(), raw, .{}) catch return;
    if (parsed.value != .object) return;
    const list = parsed.value.object.get("models") orelse return;
    if (list != .array) return;

    var next: std.StringHashMapUnmanaged(i64) = .empty;
    for (list.array.items) |model| {
        if (model != .object) continue;
        const id = model.object.get("id") orelse continue;
        if (id != .string) continue;
        if (model.object.get("is_disabled")) |d| if (d == .bool and d.bool) continue;
        add(&next, id.string, intField(model.object, "max_output_tokens"));
    }
    mutex.lock();
    defer mutex.unlock();
    if (next.count() == 0) {
        next.deinit(alloc);
        return;
    }
    replace(next);
}

fn seedLocked() void {
    if (seeded) return;
    var arena = std.heap.ArenaAllocator.init(alloc);
    defer arena.deinit();
    const parsed = std.json.parseFromSlice(std.json.Value, arena.allocator(), @embedFile("models.json"), .{}) catch return;
    if (parsed.value != .object) return;
    const data = parsed.value.object.get("data") orelse return;
    if (data != .array) return;
    var next: std.StringHashMapUnmanaged(i64) = .empty;
    for (data.array.items) |model| {
        if (model != .object) continue;
        const id = model.object.get("id") orelse continue;
        if (id == .string) add(&next, id.string, intField(model.object, "max_output_tokens"));
    }
    replace(next);
}

/// True when upstream lists this slug (live catalog, else the embedded one).
pub fn has(slug: []const u8) bool {
    mutex.lock();
    defer mutex.unlock();
    seedLocked();
    return slugs.contains(slug);
}

/// Upstream's output cap for a listed slug, when it reported one.
pub fn maxOutputTokens(slug: []const u8) ?i64 {
    mutex.lock();
    defer mutex.unlock();
    seedLocked();
    const value = slugs.get(slug) orelse return null;
    return if (value > 0) value else null;
}
