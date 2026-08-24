// Rust side of the image-preprocessing benchmark.
//
// Pipeline, identical in every variant: PNG decode -> luma grayscale -> global
// min/max contrast stretch -> integer bilinear upscale -> binary PGM out.
// Single-threaded, phase-timed, 3 warmups + a 250 ms timed window.
//
// KSTREAM is the live end-to-end candidate. Older transform variants remain
// callable for direct comparison, but the competition ranks the complete pipeline.
use std::fs::File;
use std::io::{BufWriter, Cursor, Write};
use std::time::Instant;

extern "C" {
    fn r66_transform_klut(
        source: *const u8,
        source_length: usize,
        width: usize,
        height: usize,
        channels: u32,
        scale: u32,
        destination: *mut u8,
        destination_length: usize,
    ) -> i32;

    #[cfg(feature = "zig")]
    fn r66_transform_zig(
        source: *const u8,
        source_length: usize,
        width: usize,
        height: usize,
        channels: u32,
        scale: u32,
        destination: *mut u8,
        destination_length: usize,
        gray: *mut u8,
        gray_length: usize,
        middle: *mut u16,
        middle_length: usize,
        lut: *mut u8,
        lut_length: usize,
    ) -> i32;

    #[cfg(feature = "nim")]
    fn r66_transform_nim(
        source: *const u8,
        source_length: usize,
        width: usize,
        height: usize,
        channels: u32,
        scale: u32,
        destination: *mut u8,
        destination_length: usize,
        gray: *mut u8,
        gray_length: usize,
        middle: *mut u16,
        middle_length: usize,
        lut: *mut u8,
        lut_length: usize,
    ) -> i32;
}

#[cfg(any(feature = "zig", feature = "nim"))]
type ExternalTransform = unsafe extern "C" fn(
    *const u8,
    usize,
    usize,
    usize,
    u32,
    u32,
    *mut u8,
    usize,
    *mut u8,
    usize,
    *mut u16,
    usize,
    *mut u8,
    usize,
) -> i32;

const OCR_UPSCALE_FACTOR: usize = 3;
const OCR_UPSCALE_PIXEL_BUDGET: usize = 20_000_000;

// Mirrors ocrUpscaleFactorFor: the largest factor <= 3 whose output fits the budget.
fn upscale_factor_for(w: usize, h: usize) -> usize {
    let px = w * h;
    if px == 0 {
        return OCR_UPSCALE_FACTOR;
    }
    let mut f = OCR_UPSCALE_FACTOR;
    while f > 1 {
        if px * f * f <= OCR_UPSCALE_PIXEL_BUDGET {
            return f;
        }
        f -= 1;
    }
    1
}

#[inline(always)]
fn luma(r: u8, g: u8, b: u8) -> f64 {
    0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64
}

// Decode a PNG into (pixel bytes, width, height, color type, bit depth).
fn decode(raw: &[u8]) -> (Vec<u8>, usize, usize, png::ColorType, png::BitDepth) {
    // Playwright owns this local payload, so checksums and unused metadata are
    // deliberate overhead. Structural decode failures still abort immediately.
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(true);
    options.set_ignore_text_chunk(true);
    options.set_ignore_iccp_chunk(true);
    let dec = png::Decoder::new_with_options(Cursor::new(raw), options);
    let mut reader = dec.read_info().expect("png read_info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("PNG output size")];
    let info = reader.next_frame(&mut buf).expect("png next_frame");
    buf.truncate(info.buffer_size());
    (
        buf,
        info.width as usize,
        info.height as usize,
        info.color_type,
        info.bit_depth,
    )
}

fn channels(ct: png::ColorType, bd: png::BitDepth) -> usize {
    assert_eq!(bd, png::BitDepth::Eight, "only 8-bit PNGs supported");
    match ct {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => panic!("indexed png: expand not enabled"),
    }
}

// The one per-pixel luma expression, shared by every stretch implementation so
// that "kill the buffer" changes ONLY the buffering and not the arithmetic.
// The Go At() reference path yields ALPHA-PREMULTIPLIED 8-bit channels; the png
// crate gives non-premultiplied, so premultiply here to keep values identical.
#[inline(always)]
fn luma_at(p: &[u8], ch: usize) -> f64 {
    match ch {
        4 => {
            let a = p[3] as u32;
            luma(
                (p[0] as u32 * a / 255) as u8,
                (p[1] as u32 * a / 255) as u8,
                (p[2] as u32 * a / 255) as u8,
            )
        }
        3 => luma(p[0], p[1], p[2]),
        2 => {
            let a = p[1] as u32;
            let g = (p[0] as u32 * a / 255) as u8;
            luma(g, g, g)
        }
        _ => luma(p[0], p[0], p[0]),
    }
}

// Integer luma gives the LUT candidate one stable key for each source color.
// Alpha-bearing formats retain the reference's premultiplied-channel semantics.
#[inline(always)]
fn luma_integer_at(p: &[u8], ch: usize) -> u32 {
    let (r, g, b) = match ch {
        4 => {
            let a = p[3] as u32;
            (
                p[0] as u32 * a / 255,
                p[1] as u32 * a / 255,
                p[2] as u32 * a / 255,
            )
        }
        3 => (p[0] as u32, p[1] as u32, p[2] as u32),
        2 => {
            let g = p[0] as u32 * p[1] as u32 / 255;
            (g, g, g)
        }
        _ => {
            let g = p[0] as u32;
            (g, g, g)
        }
    };
    (299 * r) + (587 * g) + (114 * b)
}

// Both benchmark fixtures decode to RGB8. The caller validates that format and
// pixel count before this kernel; raw pointers let LLVM see one fixed-stride
// loop without repeated slice-bound checks obscuring its vectorization.
unsafe fn gray_stretch_lut_rgb(buf: &[u8], n: usize) -> Vec<u8> {
    let source = buf.as_ptr();
    let mut min_l = u32::MAX;
    let mut max_l = 0u32;
    for i in 0..n {
        let pixel = source.add(i * 3);
        let value = (299 * u32::from(*pixel))
            + (587 * u32::from(*pixel.add(1)))
            + (114 * u32::from(*pixel.add(2)));
        min_l = min_l.min(value);
        max_l = max_l.max(value);
    }

    let range = max_l - min_l;
    let span = u64::from(range.max(1));
    let mut lut = vec![0u8; range as usize + 1];
    for (offset, output) in lut.iter_mut().enumerate() {
        let value = ((2 * offset as u64 * 255) + span) / (2 * span);
        *output = value.min(255) as u8;
    }

    // A fixed-length destination removes Vec::push's capacity branch per pixel.
    let mut gray = vec![0u8; n];
    let output = gray.as_mut_ptr();
    let table = lut.as_ptr();
    for i in 0..n {
        let pixel = source.add(i * 3);
        let value = (299 * u32::from(*pixel))
            + (587 * u32::from(*pixel.add(1)))
            + (114 * u32::from(*pixel.add(2)));
        *output.add(i) = *table.add((value - min_l) as usize);
    }
    gray
}

// The monotone stretch depends only on integer luma, so a bounded byte table
// replaces per-pixel floating point. The accepted difference is at most one gray
// level on sparse exact half-way ties.
fn gray_stretch_lut(
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
) -> Vec<u8> {
    let ch = channels(ct, bd);
    let n = w * h;
    if ch == 3 {
        // PNG decoding and the channel check above establish the pointer
        // kernel's required three-bytes-per-pixel precondition.
        return unsafe { gray_stretch_lut_rgb(buf, n) };
    }
    let mut min_l = u32::MAX;
    let mut max_l = 0u32;
    for pixel in buf.chunks_exact(ch).take(n) {
        let value = luma_integer_at(pixel, ch);
        min_l = min_l.min(value);
        max_l = max_l.max(value);
    }

    let range = max_l - min_l;
    let span = u64::from(range.max(1));
    let mut lut = vec![0u8; range as usize + 1];
    for (offset, output) in lut.iter_mut().enumerate() {
        let value = ((2 * offset as u64 * 255) + span) / (2 * span);
        *output = value.min(255) as u8;
    }

    let mut gray = Vec::with_capacity(n);
    for pixel in buf.chunks_exact(ch).take(n) {
        let value = luma_integer_at(pixel, ch);
        gray.push(lut[(value - min_l) as usize]);
    }
    gray
}

// HYBRID keeps Rust's PNG decoder and PGM writer while calling the faster native
// KLUT/K transform through one in-process C ABI boundary.
fn transform_hybrid(
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
    scale: usize,
) -> Vec<u8> {
    let ch = channels(ct, bd);
    let mut output = vec![0u8; w * h * scale * scale];
    let status = unsafe {
        r66_transform_klut(
            buf.as_ptr(),
            buf.len(),
            w,
            h,
            ch as u32,
            scale as u32,
            output.as_mut_ptr(),
            output.len(),
        )
    };
    assert_eq!(status, 0, "native KLUT transform failed");
    output
}

// Zig and Nim use the same decoder, writer, allocations and algorithm as the
// Rust/C++ contestants. Only the transform kernel behind this function pointer
// changes, which makes their end-to-end results directly comparable.
#[cfg(any(feature = "zig", feature = "nim"))]
fn transform_external(
    kernel: ExternalTransform,
    name: &str,
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
    scale: usize,
) -> Vec<u8> {
    let ch = channels(ct, bd);
    let pixels = w * h;
    let mut output = vec![0u8; pixels * scale * scale];
    let mut gray = vec![0u8; pixels];
    let mut middle = if scale == 2 {
        vec![0u16; w * 2 * h]
    } else {
        Vec::new()
    };
    // Maximum integer RGB luma is 255000, so this fixed scratch table covers
    // every possible observed min/max range without allocation inside foreign code.
    let mut lut = vec![0u8; 255001];
    let status = unsafe {
        kernel(
            buf.as_ptr(),
            buf.len(),
            w,
            h,
            ch as u32,
            scale as u32,
            output.as_mut_ptr(),
            output.len(),
            gray.as_mut_ptr(),
            gray.len(),
            middle.as_mut_ptr(),
            middle.len(),
            lut.as_mut_ptr(),
            lut.len(),
        )
    };
    assert_eq!(status, 0, "{name} transform failed");
    output
}

// Variant C's stretch: a full w*h Vec<f64> of luma, written in pass 1 and read
// back in pass 2. 9.8 MB on big.png, allocated, zeroed by the allocator path,
// written and re-read purely to avoid recomputing three multiplies.
fn gray_stretch_buf(
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
) -> Vec<u8> {
    let ch = channels(ct, bd);
    let n = w * h;
    let mut lum = vec![0f64; n];
    let mut min_l = f64::MAX;
    let mut max_l = f64::MIN;
    for i in 0..n {
        let l = luma_at(&buf[i * ch..], ch);
        lum[i] = l;
        if l < min_l {
            min_l = l;
        }
        if l > max_l {
            max_l = l;
        }
    }
    let mut span = max_l - min_l;
    if span < 1e-6 {
        span = 1.0;
    }
    let mut out = vec![0u8; n];
    for i in 0..n {
        let mut v = (lum[i] - min_l) / span * 255.0;
        if v < 0.0 {
            v = 0.0;
        } else if v > 255.0 {
            v = 255.0;
        }
        out[i] = (v + 0.5) as u8;
    }
    out
}

// OPTIMIZATION 4: the same stretch with the Vec<f64> deleted. Pass 1 keeps only
// min/max; pass 2 recomputes the IDENTICAL luma expression and writes the u8
// plane directly. Not one arithmetic operation changes, so the result is
// bit-for-bit gray_stretch_buf -- this is a pure memory-traffic change:
// 4 bytes read twice, instead of 4 read + 8 written + 8 read.
fn gray_stretch_nobuf(
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
) -> Vec<u8> {
    let ch = channels(ct, bd);
    let n = w * h;
    let mut min_l = f64::MAX;
    let mut max_l = f64::MIN;
    for i in 0..n {
        let l = luma_at(&buf[i * ch..], ch);
        if l < min_l {
            min_l = l;
        }
        if l > max_l {
            max_l = l;
        }
    }
    let mut span = max_l - min_l;
    if span < 1e-6 {
        span = 1.0;
    }
    let mut out = vec![0u8; n];
    for i in 0..n {
        let l = luma_at(&buf[i * ch..], ch);
        // Kept as C's exact `(l-min)/span*255`: folding span into a reciprocal
        // multiply is a different f64 rounding and would break bit-identity.
        let mut v = (l - min_l) / span * 255.0;
        if v < 0.0 {
            v = 0.0;
        } else if v > 255.0 {
            v = 255.0;
        }
        out[i] = (v + 0.5) as u8;
    }
    out
}

// ---- the naive scaler (variant C) ----

// Integer bilinear upscale with clamped edge sampling. Per OUTPUT PIXEL it
// recomputes a float divide, a floor, two clamps and a weight subtraction --
// 15.9 million times on typical.png, for 2732 distinct answers on x and 5830 on y.
fn upscale_naive(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    let (ow, oh) = (w * scale, h * scale);
    let mut dst = vec![0u8; ow * oh];
    let inv = 1.0 / scale as f64;
    let clamp = |v: i64, n: usize| -> usize {
        if v < 0 {
            0
        } else if v as usize >= n {
            n - 1
        } else {
            v as usize
        }
    };
    for oy in 0..oh {
        let sy = (oy as f64 + 0.5) * inv - 0.5;
        let y0i = sy.floor() as i64;
        let fy = sy - y0i as f64;
        let r0 = clamp(y0i, h) * w;
        let r1 = clamp(y0i + 1, h) * w;
        let orow = oy * ow;
        for ox in 0..ow {
            let sx = (ox as f64 + 0.5) * inv - 0.5;
            let x0i = sx.floor() as i64;
            let fx = sx - x0i as f64;
            let x0 = clamp(x0i, w);
            let x1 = clamp(x0i + 1, w);
            let top = src[r0 + x0] as f64 * (1.0 - fx) + src[r0 + x1] as f64 * fx;
            let bot = src[r1 + x0] as f64 * (1.0 - fx) + src[r1 + x1] as f64 * fx;
            dst[orow + ox] = (top * (1.0 - fy) + bot * fy + 0.5) as u8;
        }
    }
    dst
}

// ---- OPTIMIZATION 1: precomputed per-axis tables, 16.16 fixed point ----

// 16 fractional bits. At factor 2 the only weights are 0.25 and 0.75, exact in
// binary, so the fixed-point scaler is bit-identical to the f64 one there. At
// factor 3 the weights 1/3, 2/3 carry at most 1/131072 of error -- far under the
// half-gray-level rounding boundary. 16 bits is chosen over 8 for exactly that.
const FIX_SHIFT: u32 = 16;
const FIX_ONE: u32 = 1 << FIX_SHIFT;

// Precomputes the (i0, i1, weight) triple of every output position on one axis.
// The sample position s = (o+0.5)/scale - 0.5 has only `scale` distinct
// fractional parts, so the weights repeat with period `scale`; the CLAMPED
// indices do not repeat at the two edges, so the table is materialized at full
// output length (a few tens of KB, read sequentially) rather than as a
// scale-length pattern plus an edge special case. Built once per image.
fn bilinear_axis(n: usize, scale: usize) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
    let on = n * scale;
    let mut i0s = vec![0u32; on];
    let mut i1s = vec![0u32; on];
    let mut ws = vec![0u32; on];
    for o in 0..on {
        // Integer form of s = (o+0.5)/scale - 0.5, i.e. s = (2o+1-scale)/(2*scale).
        let num = 2 * o as i64 + 1 - scale as i64;
        let den = 2 * scale as i64;
        let mut f = num / den;
        let mut r = num - f * den;
        if r < 0 {
            // Rust truncates toward zero; bilinear needs floor.
            f -= 1;
            r += den;
        }
        let a = (f.max(0) as usize).min(n - 1);
        let b = ((f + 1).max(0) as usize).min(n - 1);
        i0s[o] = a as u32;
        i1s[o] = b as u32;
        ws[o] = ((r * FIX_ONE as i64 + den / 2) / den) as u32;
    }
    (i0s, i1s, ws)
}

// Single-pass 4-tap fixed-point scaler: tables, but the kernel is still fused.
// D3-vs-D isolates separability; D2-vs-C isolates the tables.
fn upscale_fixed(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    let (ow, oh) = (w * scale, h * scale);
    let mut dst = vec![0u8; ow * oh];
    let (x0s, x1s, wxs) = bilinear_axis(w, scale);
    let (y0s, y1s, wys) = bilinear_axis(h, scale);
    for oy in 0..oh {
        let wy = wys[oy] as u64;
        let iwy = FIX_ONE as u64 - wy;
        let r0 = &src[y0s[oy] as usize * w..][..w];
        let r1 = &src[y1s[oy] as usize * w..][..w];
        let out = &mut dst[oy * ow..][..ow];
        for ox in 0..ow {
            let (x0, x1) = (x0s[ox] as usize, x1s[ox] as usize);
            let wx = wxs[ox];
            let iwx = FIX_ONE - wx;
            let top = r0[x0] as u32 * iwx + r0[x1] as u32 * wx;
            let bot = r1[x0] as u32 * iwx + r1[x1] as u32 * wx;
            // One rounding, at the end: +0.5 ulp then shift, the same
            // round-half-up that the float path's `(v + 0.5) as u8` performs.
            out[ox] = ((top as u64 * iwy + bot as u64 * wy + (1 << (2 * FIX_SHIFT - 1)))
                >> (2 * FIX_SHIFT)) as u8;
        }
    }
    dst
}

// ---- OPTIMIZATION 2: separable two-pass ----

// Horizontal pass into a u32 intermediate holding value<<16, then a vertical
// pass over two already-filtered rows. The intermediate is 31.8 MB on
// typical.png and it still wins, because the vertical pass then reads two
// contiguous rows instead of gathering four taps per output pixel.
fn horiz_pass(
    src: &[u8],
    w: usize,
    h: usize,
    ow: usize,
    x0s: &[u32],
    x1s: &[u32],
    wxs: &[u32],
) -> Vec<u32> {
    let mut mid = vec![0u32; ow * h];
    for y in 0..h {
        let row = &src[y * w..][..w];
        let mrow = &mut mid[y * ow..][..ow];
        for ox in 0..ow {
            let wx = wxs[ox];
            mrow[ox] =
                row[x0s[ox] as usize] as u32 * (FIX_ONE - wx) + row[x1s[ox] as usize] as u32 * wx;
        }
    }
    mid
}

fn upscale_separable(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    let (ow, oh) = (w * scale, h * scale);
    let (x0s, x1s, wxs) = bilinear_axis(w, scale);
    let (y0s, y1s, wys) = bilinear_axis(h, scale);
    let mid = horiz_pass(src, w, h, ow, &x0s, &x1s, &wxs);
    let mut dst = vec![0u8; ow * oh];
    for oy in 0..oh {
        let wy = wys[oy] as u64;
        let iwy = FIX_ONE as u64 - wy;
        let m0 = &mid[y0s[oy] as usize * ow..][..ow];
        let m1 = &mid[y1s[oy] as usize * ow..][..ow];
        let out = &mut dst[oy * ow..][..ow];
        for ox in 0..ow {
            out[ox] = ((m0[ox] as u64 * iwy + m1[ox] as u64 * wy + (1 << (2 * FIX_SHIFT - 1)))
                >> (2 * FIX_SHIFT)) as u8;
        }
    }
    dst
}

// ---- OPTIMIZATION 3: the arithmetic type ----
//
// Same tables, same separable structure, only the accumulator type changes.
// F64 vs the fixed-point version is exactly the cost of f64, held otherwise
// constant; F32 vs fixed point says whether the win is "integers" or "not f64".

fn upscale_sep_f64(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    let (ow, oh) = (w * scale, h * scale);
    let (x0s, x1s, wxs) = bilinear_axis(w, scale);
    let (y0s, y1s, wys) = bilinear_axis(h, scale);
    let fwx: Vec<f64> = wxs.iter().map(|&v| v as f64 / FIX_ONE as f64).collect();
    let mut mid = vec![0f64; ow * h];
    for y in 0..h {
        let row = &src[y * w..][..w];
        let mrow = &mut mid[y * ow..][..ow];
        for ox in 0..ow {
            let wx = fwx[ox];
            mrow[ox] =
                row[x0s[ox] as usize] as f64 * (1.0 - wx) + row[x1s[ox] as usize] as f64 * wx;
        }
    }
    let mut dst = vec![0u8; ow * oh];
    for oy in 0..oh {
        let wy = wys[oy] as f64 / FIX_ONE as f64;
        let m0 = &mid[y0s[oy] as usize * ow..][..ow];
        let m1 = &mid[y1s[oy] as usize * ow..][..ow];
        let out = &mut dst[oy * ow..][..ow];
        for ox in 0..ow {
            out[ox] = (m0[ox] * (1.0 - wy) + m1[ox] * wy + 0.5) as u8;
        }
    }
    dst
}

fn upscale_sep_f32(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    let (ow, oh) = (w * scale, h * scale);
    let (x0s, x1s, wxs) = bilinear_axis(w, scale);
    let (y0s, y1s, wys) = bilinear_axis(h, scale);
    let fwx: Vec<f32> = wxs.iter().map(|&v| v as f32 / FIX_ONE as f32).collect();
    let mut mid = vec![0f32; ow * h];
    for y in 0..h {
        let row = &src[y * w..][..w];
        let mrow = &mut mid[y * ow..][..ow];
        for ox in 0..ow {
            let wx = fwx[ox];
            mrow[ox] =
                row[x0s[ox] as usize] as f32 * (1.0 - wx) + row[x1s[ox] as usize] as f32 * wx;
        }
    }
    let mut dst = vec![0u8; ow * oh];
    for oy in 0..oh {
        let wy = wys[oy] as f32 / FIX_ONE as f32;
        let m0 = &mid[y0s[oy] as usize * ow..][..ow];
        let m1 = &mid[y1s[oy] as usize * ow..][..ow];
        let out = &mut dst[oy * ow..][..ow];
        for ox in 0..ow {
            out[ox] = (m0[ox] * (1.0 - wy) + m1[ox] * wy + 0.5) as u8;
        }
    }
    dst
}

// ---- OPTIMIZATION 5 (Rust-only probe): explicit AVX2 vertical pass ----
//
// The vertical pass of the separable scaler is the ideal SIMD candidate: two
// contiguous u32 streams, one scalar weight pair per row, no gathers. The
// arithmetic is a 32x32->64 multiply, which AVX2 provides only as
// _mm256_mul_epu32 (even 32-bit lanes -> four 64-bit products), so each 8-lane
// vector needs two multiplies per input stream, with the odd lanes shifted down
// first. That is bit-exact with the scalar path -- no precision is traded.
//
// The horizontal pass is deliberately NOT vectorized: its reads are
// table-driven gathers of single bytes, and AVX2's gather is 32-bit-granular and
// slow enough to lose to the scalar loop.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn vert_pass_avx2(
    mid: &[u32],
    ow: usize,
    oh: usize,
    y0s: &[u32],
    y1s: &[u32],
    wys: &[u32],
    dst: &mut [u8],
) {
    use std::arch::x86_64::*;
    let round = _mm256_set1_epi64x(1i64 << (2 * FIX_SHIFT - 1));
    // Byte selector: take byte 0 of each of the 4 dwords in each 128-bit lane
    // and pack them into the low 4 bytes of that lane. Values are <= 255 so only
    // the low byte of each result dword carries information.
    let sel = _mm256_setr_epi8(
        0, 4, 8, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 4, 8, 12, -1, -1, -1, -1,
        -1, -1, -1, -1, -1, -1, -1, -1,
    );
    for oy in 0..oh {
        let wy = wys[oy] as i64;
        let iwy = FIX_ONE as i64 - wy;
        let vwy = _mm256_set1_epi64x(wy);
        let viwy = _mm256_set1_epi64x(iwy);
        let m0 = mid.as_ptr().add(y0s[oy] as usize * ow);
        let m1 = mid.as_ptr().add(y1s[oy] as usize * ow);
        let o = dst.as_mut_ptr().add(oy * ow);
        let mut ox = 0usize;
        while ox + 8 <= ow {
            let a = _mm256_loadu_si256(m0.add(ox) as *const __m256i);
            let b = _mm256_loadu_si256(m1.add(ox) as *const __m256i);
            // Even 32-bit lanes (0,2,4,6) -> four 64-bit accumulators.
            let ev = _mm256_add_epi64(
                _mm256_add_epi64(_mm256_mul_epu32(a, viwy), _mm256_mul_epu32(b, vwy)),
                round,
            );
            // Odd lanes (1,3,5,7), shifted into the even positions first.
            let ash = _mm256_srli_epi64(a, 32);
            let bsh = _mm256_srli_epi64(b, 32);
            let od = _mm256_add_epi64(
                _mm256_add_epi64(_mm256_mul_epu32(ash, viwy), _mm256_mul_epu32(bsh, vwy)),
                round,
            );
            // >> 32 brings each product back to a 0..255 value in the low dword.
            let evr = _mm256_srli_epi64(ev, (2 * FIX_SHIFT) as i32);
            let odr = _mm256_srli_epi64(od, (2 * FIX_SHIFT) as i32);
            // Re-interleave: even results in dword lanes 0,2,4,6; odd in 1,3,5,7.
            let packed = _mm256_or_si256(evr, _mm256_slli_epi64(odr, 32));
            let bytes = _mm256_shuffle_epi8(packed, sel);
            // 4 bytes in the low dword of each 128-bit lane; store both dwords.
            let lo = _mm256_extract_epi32(bytes, 0) as u32;
            let hi = _mm256_extract_epi32(bytes, 4) as u32;
            std::ptr::copy_nonoverlapping(lo.to_le_bytes().as_ptr(), o.add(ox), 4);
            std::ptr::copy_nonoverlapping(hi.to_le_bytes().as_ptr(), o.add(ox + 4), 4);
            ox += 8;
        }
        // Scalar tail for the last < 8 columns.
        while ox < ow {
            let v = (*m0.add(ox) as u64 * iwy as u64
                + *m1.add(ox) as u64 * wy as u64
                + (1 << (2 * FIX_SHIFT - 1)))
                >> (2 * FIX_SHIFT);
            *o.add(ox) = v as u8;
            ox += 1;
        }
    }
}

// D with the AVX2 vertical pass when the CPU has AVX2, and the scalar vertical
// pass otherwise. Runtime-detected, so the same binary runs anywhere.
fn upscale_separable_simd(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    let (ow, oh) = (w * scale, h * scale);
    let (x0s, x1s, wxs) = bilinear_axis(w, scale);
    let (y0s, y1s, wys) = bilinear_axis(h, scale);
    let mid = horiz_pass(src, w, h, ow, &x0s, &x1s, &wxs);
    let mut dst = vec![0u8; ow * oh];
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            unsafe { vert_pass_avx2(&mid, ow, oh, &y0s, &y1s, &wys, &mut dst) };
            return dst;
        }
    }
    for oy in 0..oh {
        let wy = wys[oy] as u64;
        let iwy = FIX_ONE as u64 - wy;
        let m0 = &mid[y0s[oy] as usize * ow..][..ow];
        let m1 = &mid[y1s[oy] as usize * ow..][..ow];
        let out = &mut dst[oy * ow..][..ow];
        for ox in 0..ow {
            out[ox] = ((m0[ox] as u64 * iwy + m1[ox] as u64 * wy + (1 << (2 * FIX_SHIFT - 1)))
                >> (2 * FIX_SHIFT)) as u8;
        }
    }
    dst
}

// ---- OPTIMIZATION 6: factor-2 specialization (ported from the Go side) ----
//
// WHY: the upscale factor is not merely an integer, it is 1, 2 or 3 --
// upscale_factor_for caps it at 3 and 1 is a no-op. At factor 2 the bilinear
// weights are exactly 1/4 and 3/4, which are exact in binary, so the whole
// 16.16 apparatus collapses to shift-and-add on 16-bit data:
//
//     horizontal: mid = 3a+b  and  a+3b     (max 1020, fits u16)
//     vertical:   out = (3*M0 + M1 + 8) >> 4
//
// That replaces two u32 multiplies plus two u64 multiplies per output pixel and
// halves the intermediate from u32 to u16 (31.8 MB -> 15.9 MB on typical.png).
// It is bit-identical to the general fixed-point scaler by construction, not by
// luck: the weights are exact, and one round-half-up is applied at the end of
// the vertical pass in both.
//
// This is the same win measured at 1.40x on the transform in Go; it is ported
// here so the cross-language comparison measures the LANGUAGE and not which
// side happened to receive the optimization.
fn upscale_2x(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (ow, oh) = (w * 2, h * 2);
    let mut mid = vec![0u16; ow * h];
    // All allocations have exact geometry-derived lengths. Raw row pointers
    // preserve those invariants while exposing simple, non-aliasing loops to
    // LLVM; the equivalent safe iterator form measured materially slower.
    unsafe {
        let source = src.as_ptr();
        let middle = mid.as_mut_ptr();
        for y in 0..h {
            let row = source.add(y * w);
            let output = middle.add(y * ow);
            // Edge columns are the clamped x0 == x1 case: both taps are the
            // same pixel, so the weighted sum is 4x that pixel.
            *output = u16::from(*row) * 4;
            *output.add(ow - 1) = u16::from(*row.add(w - 1)) * 4;
            for x in 0..w - 1 {
                let a = u16::from(*row.add(x));
                let b = u16::from(*row.add(x + 1));
                *output.add(2 * x + 1) = 3 * a + b;
                *output.add(2 * x + 2) = a + 3 * b;
            }
        }
    }

    let mut dst = vec![0u8; ow * oh];
    // Each expansion receives literal coefficients. That gives LLVM four
    // branch-free row kernels instead of one closure with runtime weights.
    macro_rules! write_row {
        ($dst_y:expr, $row0:expr, $row1:expr, $c0:expr, $c1:expr) => {{
            let destination = dst.as_mut_ptr().add($dst_y * ow);
            let c0 = $c0 as u32;
            let c1 = $c1 as u32;
            for x in 0..ow {
                *destination.add(x) =
                    ((c0 * u32::from(*$row0.add(x)) + c1 * u32::from(*$row1.add(x)) + 8) >> 4)
                        as u8;
            }
        }};
    }

    // The horizontal pass initialized every middle element, and destination
    // rows are disjoint. Those facts make each pointer expansion valid.
    unsafe {
        let first = mid.as_ptr();
        let last = first.add((h - 1) * ow);
        write_row!(0, first, first, 2, 2);
        write_row!(oh - 1, last, last, 2, 2);
        for y in 0..h - 1 {
            let row0 = first.add(y * ow);
            let row1 = first.add((y + 1) * ow);
            write_row!(2 * y + 1, row0, row1, 3, 1);
            write_row!(2 * y + 2, row0, row1, 1, 3);
        }
    }
    dst
}

// K: D with the factor-2 specialization, general scaler for any other factor.
fn upscale_k(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    if scale == 2 {
        return upscale_2x(src, w, h);
    }
    upscale_separable(src, w, h, scale)
}

// KSTREAM uses the same accepted 8.8 luma approximation as GOFAST. Keeping the
// luma plane from the min/max pass removes the second RGB traversal, while the
// factor-2 path below avoids the old full-image horizontal intermediate.
unsafe fn luma_plane_8p8_rgb(buf: &[u8], pixels: usize) -> (Vec<u16>, u16, u16) {
    let source = buf.as_ptr();
    let mut luma = vec![0u16; pixels];
    let output = luma.as_mut_ptr();
    let mut minimum = u16::MAX;
    let mut maximum = 0u16;
    for i in 0..pixels {
        let pixel = source.add(i * 3);
        let value =
            77 * u16::from(*pixel) + 150 * u16::from(*pixel.add(1)) + 29 * u16::from(*pixel.add(2));
        *output.add(i) = value;
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    (luma, minimum, maximum)
}

// The lookup table performs contrast normalization over the observed 8.8 luma
// range. A constant image maps to black, matching the existing KLUT contract.
fn luma_stretch_table_8p8(minimum: u16, maximum: u16) -> Vec<u8> {
    let range = usize::from(maximum - minimum);
    let span = (range as u64).max(1);
    let mut table = vec![0u8; range + 1];
    for (offset, output) in table.iter_mut().enumerate() {
        *output = (((2 * offset as u64 * 255) + span) / (2 * span)).min(255) as u8;
    }
    table
}

// materialize_gray_8p8 is used for scale 1 and for the uncommon general-scale
// fallback. Full-range screenshots avoid the data-dependent lookup entirely.
fn materialize_gray_8p8(luma: &[u16], minimum: u16, maximum: u16) -> Vec<u8> {
    let full_range = minimum == 0 && maximum == 255 * 256;
    let table = if full_range {
        Vec::new()
    } else {
        luma_stretch_table_8p8(minimum, maximum)
    };
    let mut gray = vec![0u8; luma.len()];
    if full_range {
        for (output, &value) in gray.iter_mut().zip(luma) {
            *output = ((value + 128) >> 8) as u8;
        }
    } else {
        for (output, &value) in gray.iter_mut().zip(luma) {
            *output = table[usize::from(value - minimum)];
        }
    }
    gray
}

// horizontal_luma_2x converts and interpolates one row in a single traversal.
// Raw pointers expose fixed non-aliasing streams to LLVM after the caller has
// established all geometry and allocation bounds.
unsafe fn horizontal_luma_2x(
    source: *const u16,
    width: usize,
    minimum: u16,
    table: &[u8],
    full_range: bool,
    output: *mut u16,
) {
    let gray = |value: u16| -> u16 {
        if full_range {
            (value + 128) >> 8
        } else {
            u16::from(*table.get_unchecked(usize::from(value - minimum)))
        }
    };
    let first = gray(*source);
    let last = gray(*source.add(width - 1));
    *output = first * 4;
    *output.add(width * 2 - 1) = last * 4;
    for x in 0..width - 1 {
        let a = gray(*source.add(x));
        let b = gray(*source.add(x + 1));
        *output.add(2 * x + 1) = 3 * a + b;
        *output.add(2 * x + 2) = a + 3 * b;
    }
}

// upscale_luma_2x_streaming holds only two horizontally interpolated rows and
// writes both vertically interpolated outputs together. For typical.png this
// replaces a roughly 16 MB intermediate with about 11 KB of row storage.
fn upscale_luma_2x_streaming(
    luma: &[u16],
    width: usize,
    height: usize,
    minimum: u16,
    maximum: u16,
) -> Vec<u8> {
    let output_width = width * 2;
    let output_height = height * 2;
    let full_range = minimum == 0 && maximum == 255 * 256;
    let table = if full_range {
        Vec::new()
    } else {
        luma_stretch_table_8p8(minimum, maximum)
    };
    let mut destination = vec![0u8; output_width * output_height];
    let mut current = vec![0u16; output_width];
    let mut next = vec![0u16; output_width];

    unsafe {
        horizontal_luma_2x(
            luma.as_ptr(),
            width,
            minimum,
            &table,
            full_range,
            current.as_mut_ptr(),
        );
        for x in 0..output_width {
            *destination.as_mut_ptr().add(x) = ((*current.as_ptr().add(x) + 2) >> 2) as u8;
        }
        for y in 0..height - 1 {
            horizontal_luma_2x(
                luma.as_ptr().add((y + 1) * width),
                width,
                minimum,
                &table,
                full_range,
                next.as_mut_ptr(),
            );
            let upper = destination.as_mut_ptr().add((2 * y + 1) * output_width);
            let lower = destination.as_mut_ptr().add((2 * y + 2) * output_width);
            for x in 0..output_width {
                let a = u32::from(*current.as_ptr().add(x));
                let b = u32::from(*next.as_ptr().add(x));
                *upper.add(x) = ((3 * a + b + 8) >> 4) as u8;
                *lower.add(x) = ((a + 3 * b + 8) >> 4) as u8;
            }
            std::mem::swap(&mut current, &mut next);
        }
        let bottom = destination
            .as_mut_ptr()
            .add((output_height - 1) * output_width);
        for x in 0..output_width {
            *bottom.add(x) = ((*current.as_ptr().add(x) + 2) >> 2) as u8;
        }
    }
    destination
}

// transform_kstream keeps a general fallback for valid PNG formats and scale 3;
// only the measured RGB scale-1/2 paths receive the lower-memory implementation.
fn transform_kstream(
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
    scale: usize,
) -> Vec<u8> {
    if channels(ct, bd) != 3 {
        return upscale_k(&gray_stretch_lut(buf, w, h, ct, bd), w, h, scale);
    }
    let pixels = w * h;
    let (luma, minimum, maximum) = unsafe { luma_plane_8p8_rgb(buf, pixels) };
    if scale == 2 {
        return upscale_luma_2x_streaming(&luma, w, h, minimum, maximum);
    }
    let gray = materialize_gray_8p8(&luma, minimum, maximum);
    if scale <= 1 {
        return gray;
    }
    upscale_separable(&gray, w, h, scale)
}

// transform_kstream_phased mirrors transform_kstream exactly while exposing
// the optimized RGB luma/minmax scan separately from materialization/scaling.
// Non-RGB inputs retain KSTREAM's existing KLUT + upscale_k fallback, so phase
// instrumentation remains useful for every PNG format accepted by the decoder.
fn transform_kstream_phased(
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
    scale: usize,
) -> (
    Vec<u8>,
    std::time::Duration,
    std::time::Duration,
    &'static str,
    &'static str,
) {
    let phase_a_start = Instant::now();
    if channels(ct, bd) != 3 {
        let gray = gray_stretch_lut(buf, w, h, ct, bd);
        let phase_b_start = Instant::now();
        let output = upscale_k(&gray, w, h, scale);
        let finish = Instant::now();
        return (
            output,
            phase_b_start - phase_a_start,
            finish - phase_b_start,
            "stretchFallback",
            "scaleFallback",
        );
    }

    let pixels = w * h;
    let (luma, minimum, maximum) = unsafe { luma_plane_8p8_rgb(buf, pixels) };
    let phase_b_start = Instant::now();
    let output = if scale == 2 {
        upscale_luma_2x_streaming(&luma, w, h, minimum, maximum)
    } else {
        let gray = materialize_gray_8p8(&luma, minimum, maximum);
        if scale <= 1 {
            gray
        } else {
            upscale_separable(&gray, w, h, scale)
        }
    };
    let finish = Instant::now();
    (
        output,
        phase_b_start - phase_a_start,
        finish - phase_b_start,
        "lumaMinmax8p8",
        "materializeScale",
    )
}

// ---- variant dispatch ----

// Returns (stretched-or-scaled plane, out width, out height). Phase timing wraps
// this whole call, exactly as the Go harness times its preprocess* functions.
fn transform(
    variant: &str,
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
    scale: usize,
) -> Vec<u8> {
    match variant {
        "C" => upscale_naive(&gray_stretch_buf(buf, w, h, ct, bd), w, h, scale),
        "D" => upscale_separable(&gray_stretch_nobuf(buf, w, h, ct, bd), w, h, scale),
        // DB is D with opt 4 REVERSED. It exists because the ablations measured
        // opt 4 as a regression in Rust, not a win: unlike Go -- whose reference
        // reads an already-premultiplied image.RGBA -- the png crate hands over
        // NON-premultiplied bytes, so recomputing luma in pass 2 re-pays three
        // integer divides by 255 per pixel. That is more expensive than the 8
        // bytes of buffer traffic it saves, and it reverses the sign of the win.
        "DB" => upscale_separable(&gray_stretch_buf(buf, w, h, ct, bd), w, h, scale),
        "K" => upscale_k(&gray_stretch_nobuf(buf, w, h, ct, bd), w, h, scale),
        // KB is K on the buffered stretch, because the Rust ablations found the
        // buffer-free stretch to be a REGRESSION here (non-premultiplied png
        // bytes cost three divides per pixel in pass 2). K vs KB keeps that
        // finding attributable now that the scaler underneath has changed.
        "KB" => upscale_k(&gray_stretch_buf(buf, w, h, ct, bd), w, h, scale),
        "KLUT" => upscale_k(&gray_stretch_lut(buf, w, h, ct, bd), w, h, scale),
        "KSTREAM" => transform_kstream(buf, w, h, ct, bd, scale),
        "HYBRID" => transform_hybrid(buf, w, h, ct, bd, scale),
        #[cfg(feature = "zig")]
        "RUST-ZIG" => transform_external(r66_transform_zig, "Zig", buf, w, h, ct, bd, scale),
        #[cfg(feature = "nim")]
        "RUST-NIM" => transform_external(r66_transform_nim, "Nim", buf, w, h, ct, bd, scale),
        "DS" => upscale_separable_simd(&gray_stretch_nobuf(buf, w, h, ct, bd), w, h, scale),
        "D1" => upscale_naive(&gray_stretch_nobuf(buf, w, h, ct, bd), w, h, scale),
        "D2" => upscale_fixed(&gray_stretch_buf(buf, w, h, ct, bd), w, h, scale),
        "D3" => upscale_fixed(&gray_stretch_nobuf(buf, w, h, ct, bd), w, h, scale),
        "F64" => upscale_sep_f64(&gray_stretch_nobuf(buf, w, h, ct, bd), w, h, scale),
        "F32" => upscale_sep_f32(&gray_stretch_nobuf(buf, w, h, ct, bd), w, h, scale),
        _ => panic!("unknown variant {}", variant),
    }
}

fn write_pgm(path: &str, pix: &[u8], w: usize, h: usize) {
    // A large userspace buffer coalesces the header and image writes. Paired
    // measurements beat issuing the full image directly to the OS file cache.
    let file = File::create(path).expect("create out");
    let mut output = BufWriter::with_capacity(1 << 20, file);
    write!(output, "P5\n{} {}\n255\n", w, h).unwrap();
    output.write_all(pix).unwrap();
    output.flush().unwrap();
}

fn stats(v: &mut Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = v[0];
    let med = if v.len() % 2 == 0 {
        (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
    } else {
        v[v.len() / 2]
    };
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    (min, med, mean)
}

// Pixel-diff of two PGMs. The correctness gate: every optimized variant must be
// compared against C's output on both fixtures, and a faster wrong answer counts
// for nothing.
fn cmp_pgm(a: &str, b: &str) {
    let pa = std::fs::read(a).expect("read a");
    let pb = std::fs::read(b).expect("read b");
    // Skip the P5 header: three whitespace-terminated fields after the magic.
    let body = |v: &[u8]| -> usize {
        let mut i = 2usize;
        let mut fields = 0;
        while fields < 3 {
            while v[i].is_ascii_whitespace() {
                i += 1;
            }
            while !v[i].is_ascii_whitespace() {
                i += 1;
            }
            fields += 1;
        }
        i + 1
    };
    let (ia, ib) = (body(&pa), body(&pb));
    let (sa, sb) = (&pa[ia..], &pb[ib..]);
    assert_eq!(sa.len(), sb.len(), "pgm pixel counts differ");
    let mut diff = 0usize;
    let mut maxd = 0i32;
    for i in 0..sa.len() {
        let d = (sa[i] as i32 - sb[i] as i32).abs();
        if d != 0 {
            diff += 1;
            if d > maxd {
                maxd = d;
            }
        }
    }
    println!(
        "{{\"a\":\"{}\",\"b\":\"{}\",\"pixels\":{},\"differing\":{},\"pct\":{:.4},\"maxAbsDelta\":{}}}",
        a,
        b,
        sa.len(),
        diff,
        diff as f64 * 100.0 / sa.len() as f64,
        maxd
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "cmp" {
        cmp_pgm(&args[2], &args[3]);
        return;
    }
    // Usage: rsbench <VARIANT> <input.png> <output.pgm>
    let variant = args[1].clone();
    let inp = &args[2];
    let outp = &args[3];
    let raw = std::fs::read(inp).expect("read input");
    // Optional coarse instrumentation reports minimum, median and mean for the
    // transform's real sub-phases. It does not alter default benchmark timings.
    let phases = std::env::var("RSBENCH_PHASES").is_ok();
    // A 250 ms window caps wall time below the old fixed-count protocol; outer
    // shuffled pairs provide the larger comparison sample.
    let warm = 3usize;
    let timed_for = std::time::Duration::from_millis(250);
    let (mut dec, mut tr, mut enc, mut tot) = (vec![], vec![], vec![], vec![]);
    let (mut st, mut sc) = (vec![], vec![]);
    let (mut phase_a_name, mut phase_b_name) = ("stretch", "scale");
    let mut i = 0usize;
    let mut timed_start = None;
    let (ow, oh, scale) = loop {
        let t0 = Instant::now();
        if i == warm {
            timed_start = Some(t0);
        }
        let (buf, w, h, ct, bd) = decode(&raw);
        let t1 = Instant::now();
        let scale = upscale_factor_for(w, h);
        let big = if phases && variant == "KSTREAM" {
            // KSTREAM's RGB path fuses grayscale materialization into its
            // scaler. Preserve that path and time its two real stages instead
            // of routing it through the legacy gray-plane instrumentation.
            let (out, phase_a, phase_b, name_a, name_b) =
                transform_kstream_phased(&buf, w, h, ct, bd, scale);
            phase_a_name = name_a;
            phase_b_name = name_b;
            if i >= warm {
                st.push(phase_a.as_secs_f64() * 1000.0);
                sc.push(phase_b.as_secs_f64() * 1000.0);
            }
            out
        } else if phases {
            // Split path used ONLY under RSBENCH_PHASES; the timed default runs
            // the single fused `transform` call so the harness measures the same
            // thing the Go harness does.
            let a = Instant::now();
            let small = match variant.as_str() {
                "C" | "D2" | "DB" | "KB" => gray_stretch_buf(&buf, w, h, ct, bd),
                "KLUT" => gray_stretch_lut(&buf, w, h, ct, bd),
                _ => gray_stretch_nobuf(&buf, w, h, ct, bd),
            };
            let b = Instant::now();
            let out = match variant.as_str() {
                "C" | "D1" => upscale_naive(&small, w, h, scale),
                "D" | "DB" => upscale_separable(&small, w, h, scale),
                "DS" => upscale_separable_simd(&small, w, h, scale),
                "D2" | "D3" => upscale_fixed(&small, w, h, scale),
                "F64" => upscale_sep_f64(&small, w, h, scale),
                "F32" => upscale_sep_f32(&small, w, h, scale),
                "K" | "KB" | "KLUT" => upscale_k(&small, w, h, scale),
                _ => panic!("unknown variant"),
            };
            let c = Instant::now();
            if i >= warm {
                st.push((b - a).as_secs_f64() * 1000.0);
                sc.push((c - b).as_secs_f64() * 1000.0);
            }
            out
        } else {
            transform(&variant, &buf, w, h, ct, bd, scale)
        };
        let t2 = Instant::now();
        let ow = w * scale;
        let oh = h * scale;
        write_pgm(outp, &big, ow, oh);
        let t3 = Instant::now();
        if i >= warm {
            let msf = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            dec.push(msf(t1 - t0));
            tr.push(msf(t2 - t1));
            enc.push(msf(t3 - t2));
            tot.push(msf(t3 - t0));
            if t3.duration_since(timed_start.expect("timed start")) >= timed_for {
                break (ow, oh, scale);
            }
        }
        i += 1;
    };
    let sz = std::fs::metadata(outp).unwrap().len();
    let (dn, dm, da) = stats(&mut dec);
    let (tn, tm, ta) = stats(&mut tr);
    let (en, em, ea) = stats(&mut enc);
    let (on, om, oa) = stats(&mut tot);
    let extra = if phases {
        let (sn, sm, sa) = stats(&mut st);
        let (cn, cm, ca) = stats(&mut sc);
        format!(
            ",\"{}\":{{\"min\":{},\"med\":{},\"mean\":{}}},\"{}\":{{\"min\":{},\"med\":{},\"mean\":{}}}",
            phase_a_name, sn, sm, sa, phase_b_name, cn, cm, ca
        )
    } else {
        String::new()
    };
    println!(
        "{{\"variant\":\"{}\",\"input\":\"{}\",\"scale\":{},\"outW\":{},\"outH\":{},\"outBytes\":{},\"iters\":{},\
\"decode\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"transform\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"encode\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"total\":{{\"min\":{},\"med\":{},\"mean\":{}}}{}}}",
		variant, inp, scale, ow, oh, sz, tot.len(), dn, dm, da, tn, tm, ta, en, em, ea, on, om, oa, extra
    );
}
