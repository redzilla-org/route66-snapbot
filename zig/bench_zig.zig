// Pure-Zig end-to-end benchmark: zignal PNG decode, KLUT transform, PGM write.
const std = @import("std");
const zignal = @import("zignal");
const Io = std.Io;
const Rgb = zignal.Rgb(u8);

const warmups = 3;
const max_iterations = 4096;
const timed_nanoseconds = 250 * std.time.ns_per_ms;
const pixel_budget = 20_000_000;

fn scaleFor(width: usize, height: usize) usize {
    var scale: usize = 3;
    while (scale > 1) : (scale -= 1) {
        if (width * height * scale * scale <= pixel_budget) return scale;
    }
    return 1;
}

fn transform(allocator: std.mem.Allocator, source: []const Rgb, width: usize, height: usize, scale: usize) ![]u8 {
    @setRuntimeSafety(false);
    const pixels = width * height;
    if (@sizeOf(Rgb) != 3 or source.len != pixels) return error.InvalidRgbLayout;
    const source_bytes: [*]const u8 = @ptrCast(source.ptr);
    var lo: u32 = 0xFFFFFFFF;
    var hi: u32 = 0;
    var i: usize = 0;
    while (i < pixels) : (i += 1) {
        const pixel = source_bytes + i * 3;
        const value = 299 * @as(u32, pixel[0]) + 587 * @as(u32, pixel[1]) + 114 * @as(u32, pixel[2]);
        lo = @min(lo, value);
        hi = @max(hi, value);
    }

    const range = hi - lo;
    const span: u64 = @max(@as(u64, range), 1);
    const lut = try allocator.alloc(u8, @as(usize, range) + 1);
    defer allocator.free(lut);
    i = 0;
    while (i < lut.len) : (i += 1) {
        lut[i] = @intCast(@min(((2 * @as(u64, i) * 255) + span) / (2 * span), 255));
    }
    const gray = try allocator.alloc(u8, pixels);
    defer allocator.free(gray);
    const gray_ptr = gray.ptr;
    i = 0;
    while (i < pixels) : (i += 1) {
        const pixel = source_bytes + i * 3;
        const value = 299 * @as(u32, pixel[0]) + 587 * @as(u32, pixel[1]) + 114 * @as(u32, pixel[2]);
        gray_ptr[i] = lut[value - lo];
    }
    if (scale == 1) return allocator.dupe(u8, gray);
    if (scale != 2) return error.UnsupportedScale;

    const output_width = width * 2;
    const middle = try allocator.alloc(u16, output_width * height);
    defer allocator.free(middle);
    const middle_ptr = middle.ptr;
    var y: usize = 0;
    while (y < height) : (y += 1) {
        const row = gray_ptr + y * width;
        const output = middle_ptr + y * output_width;
        output[0] = @as(u16, row[0]) * 4;
        output[output_width - 1] = @as(u16, row[width - 1]) * 4;
        var x: usize = 0;
        while (x + 1 < width) : (x += 1) {
            const a: u16 = row[x];
            const b: u16 = row[x + 1];
            output[2 * x + 1] = 3 * a + b;
            output[2 * x + 2] = a + 3 * b;
        }
    }

    const destination = try allocator.alloc(u8, output_width * height * 2);
    const destination_ptr = destination.ptr;
    const Row = struct {
        fn write(comptime c0: u32, comptime c1: u32, output: [*]u8, row0: [*]const u16, row1: [*]const u16, count: usize) void {
            @setRuntimeSafety(false);
            var x: usize = 0;
            while (x < count) : (x += 1) {
                output[x] = @intCast((c0 * @as(u32, row0[x]) + c1 * @as(u32, row1[x]) + 8) >> 4);
            }
        }
    };
    Row.write(2, 2, destination_ptr, middle_ptr, middle_ptr, output_width);
    const last = middle_ptr + (height - 1) * output_width;
    Row.write(2, 2, destination_ptr + (height * 2 - 1) * output_width, last, last, output_width);
    y = 0;
    while (y + 1 < height) : (y += 1) {
        const row0 = middle_ptr + y * output_width;
        const row1 = row0 + output_width;
        Row.write(3, 1, destination_ptr + (2 * y + 1) * output_width, row0, row1, output_width);
        Row.write(1, 3, destination_ptr + (2 * y + 2) * output_width, row0, row1, output_width);
    }
    return destination;
}

fn writePgm(io: Io, path: []const u8, pixels: []const u8, width: usize, height: usize) !usize {
    var header_buffer: [64]u8 = undefined;
    const header = try std.fmt.bufPrint(&header_buffer, "P5\n{d} {d}\n255\n", .{ width, height });
    const file = try Io.Dir.cwd().createFile(io, path, .{});
    defer file.close(io);
    try file.writeStreamingAll(io, header);
    try file.writeStreamingAll(io, pixels);
    return header.len + pixels.len;
}

fn median(values: []f64) f64 {
    std.mem.sort(f64, values, {}, std.sort.asc(f64));
    return values[values.len / 2];
}

// Report the fastest observed sample as the competitive benchmark metric while
// retaining the median separately to expose scheduler and system noise.
fn minimum(values: []const f64) f64 {
    var result = values[0];
    for (values[1..]) |value| result = @min(result, value);
    return result;
}

fn millis(start: Io.Timestamp, end: Io.Timestamp) f64 {
    return @as(f64, @floatFromInt(start.durationTo(end).toNanoseconds())) / std.time.ns_per_ms;
}

pub fn main(init: std.process.Init) !void {
    var args = try init.minimal.args.iterateAllocator(init.gpa);
    defer args.deinit();
    _ = args.skip();
    const variant = args.next() orelse return error.MissingVariant;
    if (!std.mem.eql(u8, variant, "KLUT")) return error.UnsupportedVariant;
    const input_path = args.next() orelse return error.MissingInput;
    const output_path = args.next() orelse return error.MissingOutput;

    const raw = try Io.Dir.cwd().readFileAlloc(init.io, input_path, init.gpa, .limited(100 * 1024 * 1024));
    defer init.gpa.free(raw);
    var decode_times: [max_iterations]f64 = undefined;
    var transform_times: [max_iterations]f64 = undefined;
    var encode_times: [max_iterations]f64 = undefined;
    var total_times: [max_iterations]f64 = undefined;
    var width: usize = 0;
    var height: usize = 0;
    var scale: usize = 0;
    var output_bytes: usize = 0;
    var iteration: usize = 0;
    var samples: usize = 0;
    var timed_start: Io.Timestamp = undefined;

    while (true) : (iteration += 1) {
        const t0 = Io.Clock.awake.now(init.io);
        if (iteration == warmups) timed_start = t0;
        var image = try zignal.png.loadFromBytes(Rgb, init.gpa, raw, .{});
        const t1 = Io.Clock.awake.now(init.io);
        width = image.cols;
        height = image.rows;
        scale = scaleFor(width, height);
        const output = try transform(init.gpa, image.data, width, height, scale);
        const t2 = Io.Clock.awake.now(init.io);
        output_bytes = try writePgm(init.io, output_path, output, width * scale, height * scale);
        const t3 = Io.Clock.awake.now(init.io);
        if (iteration >= warmups) {
            if (samples == max_iterations) return error.TimerResolutionTooLow;
            decode_times[samples] = millis(t0, t1);
            transform_times[samples] = millis(t1, t2);
            encode_times[samples] = millis(t2, t3);
            total_times[samples] = millis(t0, t3);
            samples += 1;
        }
        init.gpa.free(output);
        image.deinit(init.gpa);
        if (iteration >= warmups and timed_start.durationTo(t3).toNanoseconds() >= timed_nanoseconds) break;
    }

    var stdout_buffer: [2048]u8 = undefined;
    var stdout = Io.File.stdout().writer(init.io, &stdout_buffer);
    const minimum_decode = minimum(decode_times[0..samples]);
    const minimum_transform = minimum(transform_times[0..samples]);
    const minimum_encode = minimum(encode_times[0..samples]);
    const minimum_total = minimum(total_times[0..samples]);
    try stdout.interface.print(
        "{{\"variant\":\"KLUT\",\"input\":\"{s}\",\"scale\":{d},\"outW\":{d},\"outH\":{d},\"outBytes\":{d},\"iters\":{d},\"decode\":{{\"min\":{d},\"med\":{d}}},\"transform\":{{\"min\":{d},\"med\":{d}}},\"encode\":{{\"min\":{d},\"med\":{d}}},\"total\":{{\"min\":{d},\"med\":{d}}}}}\n",
        .{ input_path, scale, width * scale, height * scale, output_bytes, samples, minimum_decode, median(decode_times[0..samples]), minimum_transform, median(transform_times[0..samples]), minimum_encode, median(encode_times[0..samples]), minimum_total, median(total_times[0..samples]) },
    );
    try stdout.interface.flush();
}
