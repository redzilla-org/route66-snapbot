// Zig implementation of the accepted RGB KLUT and factor-2 scaler.
// Rust owns every allocation so this freestanding object needs no Zig runtime.

fn luma(pixel: [*]const u8) u32 {
    return (299 * @as(u32, pixel[0])) +
        (587 * @as(u32, pixel[1])) +
        (114 * @as(u32, pixel[2]));
}

export fn r66_transform_zig(
    source: [*]const u8,
    source_length: usize,
    width: usize,
    height: usize,
    channels: u32,
    scale: u32,
    destination: [*]u8,
    destination_length: usize,
    gray: [*]u8,
    gray_length: usize,
    middle: [*]u16,
    middle_length: usize,
    lut: [*]u8,
    lut_length: usize,
) callconv(.C) c_int {
    // ReleaseFast already removes safety checks, but keeping the intent local
    // makes it explicit that the validated C ABI lengths govern pointer access.
    @setRuntimeSafety(false);
    if (width == 0 or height == 0 or channels != 3 or (scale != 1 and scale != 2)) return 1;
    const pixels = width * height;
    const output_pixels = pixels * scale * scale;
    if (source_length != pixels * 3 or destination_length != output_pixels or
        gray_length < pixels or lut_length < 255001)
    {
        return 1;
    }
    if (scale == 2 and middle_length < width * 2 * height) return 1;

    // Fixed RGB loads let LLVM vectorize the min/max reduction without channel
    // dispatch or slice checks inside the pixel loop.
    var lo: u32 = 0xFFFFFFFF;
    var hi: u32 = 0;
    var i: usize = 0;
    while (i < pixels) : (i += 1) {
        const value = luma(source + i * 3);
        lo = @min(lo, value);
        hi = @max(hi, value);
    }

    const range = hi - lo;
    const span: u64 = @max(@as(u64, range), 1);
    i = 0;
    while (i <= range) : (i += 1) {
        const value = ((2 * @as(u64, i) * 255) + span) / (2 * span);
        lut[i] = @intCast(@min(value, 255));
    }
    i = 0;
    while (i < pixels) : (i += 1) {
        gray[i] = lut[luma(source + i * 3) - lo];
    }

    if (scale == 1) {
        @memcpy(destination[0..pixels], gray[0..pixels]);
        return 0;
    }

    // At 2x the exact bilinear weights are 1/4 and 3/4. The intermediate keeps
    // four times the horizontal value so rounding happens once after vertical.
    const output_width = width * 2;
    var y: usize = 0;
    while (y < height) : (y += 1) {
        const row = gray + y * width;
        const output = middle + y * output_width;
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

    // Comptime coefficients generate specialized row kernels, mirroring the
    // macro-expanded Rust and inlined C++ loops used by the other contenders.
    const writeRow = struct {
        fn run(
            comptime c0: u32,
            comptime c1: u32,
            output: [*]u8,
            row0: [*]const u16,
            row1: [*]const u16,
            count: usize,
        ) void {
            @setRuntimeSafety(false);
            var x: usize = 0;
            while (x < count) : (x += 1) {
                output[x] = @intCast((c0 * @as(u32, row0[x]) +
                    c1 * @as(u32, row1[x]) + 8) >> 4);
            }
        }
    }.run;

    const first = middle;
    const last = middle + (height - 1) * output_width;
    writeRow(2, 2, destination, first, first, output_width);
    writeRow(2, 2, destination + (height * 2 - 1) * output_width, last, last, output_width);
    y = 0;
    while (y + 1 < height) : (y += 1) {
        const row0 = middle + y * output_width;
        const row1 = row0 + output_width;
        writeRow(3, 1, destination + (2 * y + 1) * output_width, row0, row1, output_width);
        writeRow(1, 3, destination + (2 * y + 2) * output_width, row0, row1, output_width);
    }
    return 0;
}
