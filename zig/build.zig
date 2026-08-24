// Build only the benchmark and the pinned pure-Zig zignal dependency.
// Version options mirror zignal's own build because its root module exposes them.
const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});
    const zignal = b.addModule("zignal", .{
        .root_source_file = b.path("zignal/src/root.zig"),
        .target = target,
    });
    const options = b.addOptions();
    options.addOption([]const u8, "version", "pinned-benchmark");
    zignal.addOptions("build_options", options);

    const executable = b.addExecutable(.{
        .name = "zigbench",
        .root_module = b.createModule(.{
            .root_source_file = b.path("bench_zig.zig"),
            .target = target,
            .optimize = optimize,
            .strip = true,
            .link_libc = target.result.os.tag == .windows,
            .imports = &.{.{ .name = "zignal", .module = zignal }},
        }),
    });
    b.installArtifact(executable);
}
