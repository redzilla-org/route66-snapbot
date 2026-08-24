//! kstream — the image pipeline: PNG bytes in, OCR-ready grayscale plane out.
//!
//! Ported from the WINNING variant of `img-scaling-benchmark` (D:/git/
//! img-scaling-benchmark/rust, variant `KSTREAM`): "Pure Rust KSTREAM, png
//! 0.18.1 + fdeflate, WITHOUT PGO" — 28.98 / 54.77 / 34.14 ms on the
//! benchmark's three fixtures, ahead of Go, Zig, Nim, Wuffs and the C++
//! hybrid. PGO measured as a wash there (29.06 / 54.83 / 34.21 ms), so this
//! build deliberately does not use it.
//!
//! What KSTREAM does differently from the older KLUT variant this module
//! replaces:
//!
//! * The min/max contrast pass KEEPS its 8.8 fixed-point luma plane, so the
//!   RGB source is traversed once, not twice.
//! * The factor-2 upscale is STREAMING: two horizontally-interpolated rows of
//!   storage (~11 KB on the benchmark's typical.png) instead of a full-image
//!   intermediate (~16 MB).
//! * The PNG decoder is configured for TRUSTED input (Playwright-produced
//!   screenshots handed over by the same caller): checksums and text/iCCP
//!   chunks are skipped. Structural decode failures still error immediately.
//!
//! The arithmetic contract is unchanged from KLUT: global min/max contrast
//! stretch (constant image maps to black), round-half-up bilinear upscale.
//! Differences against the f64 reference are at most one gray level on sparse
//! exact half-way ties — irrelevant to OCR.
//!
//! Error style: this module returns `Result<_, String>` instead of panicking.
//! The daemon builds with `panic = "abort"`, so a panic here would take down
//! every in-flight read on the box; a bad image must only fail its own request.

use std::io::Cursor;

// ---- decode ----

/// Decode a PNG into (pixel bytes, width, height, color type, bit depth).
///
/// KSTREAM decoder configuration for trusted input: the screenshots are
/// produced and handed over by the same local caller, so CRC validation and
/// unused metadata chunks are deliberate overhead. Structural decode failures
/// still surface as errors immediately.
fn decode(raw: &[u8]) -> Result<(Vec<u8>, usize, usize, png::ColorType, png::BitDepth), String> {
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(true);
    options.set_ignore_text_chunk(true);
    options.set_ignore_iccp_chunk(true);
    let dec = png::Decoder::new_with_options(Cursor::new(raw), options);
    let mut reader = dec.read_info().map_err(|e| format!("png read_info: {e}"))?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| "png output size overflow".to_string())?;
    let mut buf = vec![0u8; size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png next_frame: {e}"))?;
    buf.truncate(info.buffer_size());
    Ok((
        buf,
        info.width as usize,
        info.height as usize,
        info.color_type,
        info.bit_depth,
    ))
}

fn channels(ct: png::ColorType, bd: png::BitDepth) -> Result<usize, String> {
    if bd != png::BitDepth::Eight {
        return Err(format!("unsupported bit depth {bd:?}: only 8-bit PNG is handled"));
    }
    match ct {
        png::ColorType::Rgba => Ok(4),
        png::ColorType::Rgb => Ok(3),
        png::ColorType::Grayscale => Ok(1),
        png::ColorType::GrayscaleAlpha => Ok(2),
        png::ColorType::Indexed => Err("indexed png: palette expansion not enabled".to_string()),
    }
}

// ---- non-RGB fallback: integer-luma LUT stretch (KLUT lineage) ----
//
// Screenshots decode as RGB8 in practice, so this path is the correctness
// backstop for the other valid PNG formats, not a hot path. It is the same
// per-pixel arithmetic as the RGB path below, expressed per-channel-count.

/// Integer luma: one stable key per source color. Alpha-bearing formats retain
/// the reference implementation's premultiplied-channel semantics.
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

/// Global min/max contrast stretch through a bounded byte table. The monotone
/// stretch depends only on integer luma, so the table replaces per-pixel
/// floating point; accepted difference is at most one gray level on sparse
/// exact half-way ties.
fn gray_stretch_lut(buf: &[u8], n: usize, ch: usize) -> Vec<u8> {
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

// ---- bilinear upscale tables ----

const FIX_SHIFT: u32 = 16;
const FIX_ONE: u32 = 1 << FIX_SHIFT;

/// Precomputes the (i0, i1, weight) triple of every output position on one
/// axis. The sample position s = (o+0.5)/scale - 0.5 has only `scale` distinct
/// fractional parts, so the weights repeat with period `scale`; the CLAMPED
/// indices do not repeat at the two edges, so the table is materialized at full
/// output length (a few tens of KB, read sequentially) rather than as a
/// scale-length pattern plus an edge special case. Built once per image.
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

// ---- general-scale separable two-pass upscale ----

/// Horizontal pass into a u32 intermediate holding value<<16, then a vertical
/// pass over two already-filtered rows. The intermediate is large and it still
/// wins, because the vertical pass then reads two contiguous rows instead of
/// gathering four taps per output pixel.
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

// ---- factor-2 specialization (gray input; used by the non-RGB fallback) ----

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

/// Factor-2 specialization when available, general scaler for any other factor.
fn upscale_k(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    if scale == 2 {
        return upscale_2x(src, w, h);
    }
    upscale_separable(src, w, h, scale)
}

// ---- the KSTREAM RGB path ----

/// KSTREAM's 8.8 fixed-point luma pass. Keeping the luma plane from the
/// min/max pass removes the second RGB traversal the older KLUT variant paid.
///
/// SAFETY: the caller has established that `buf` holds at least `pixels * 3`
/// bytes (RGB8, validated by `channels`).
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

/// Contrast normalization table over the observed 8.8 luma range. A constant
/// image maps to black, matching the KLUT contract.
fn luma_stretch_table_8p8(minimum: u16, maximum: u16) -> Vec<u8> {
    let range = usize::from(maximum - minimum);
    let span = (range as u64).max(1);
    let mut table = vec![0u8; range + 1];
    for (offset, output) in table.iter_mut().enumerate() {
        *output = (((2 * offset as u64 * 255) + span) / (2 * span)).min(255) as u8;
    }
    table
}

/// Used for scale 1 and the general-scale (e.g. 3x) path. Full-range images
/// avoid the data-dependent lookup entirely.
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

/// Converts and interpolates one row in a single traversal.
///
/// SAFETY: raw pointers expose fixed non-aliasing streams to LLVM after the
/// caller has established all geometry and allocation bounds: `source` holds
/// `width` u16s, `output` holds `width * 2` u16s, and when `full_range` is
/// false every luma value lies within `table`'s indexed range.
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

/// Streaming factor-2 upscale straight off the 8.8 luma plane: holds only two
/// horizontally interpolated rows and writes both vertically interpolated
/// outputs together. On the benchmark's typical.png this replaces a roughly
/// 16 MB intermediate with about 11 KB of row storage — the "STREAM" in
/// KSTREAM, and the core of its win.
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

/// The KSTREAM transform: RGB takes the fused 8.8 luma path (streaming for
/// scale 2, materialize + separable for the general scale); every other valid
/// PNG format takes the KLUT-lineage fallback.
fn transform_kstream(
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
    scale: usize,
) -> Result<Vec<u8>, String> {
    let ch = channels(ct, bd)?;
    if ch != 3 {
        return Ok(upscale_k(&gray_stretch_lut(buf, w * h, ch), w, h, scale));
    }
    let pixels = w * h;
    let (luma, minimum, maximum) = unsafe { luma_plane_8p8_rgb(buf, pixels) };
    if scale == 2 {
        return Ok(upscale_luma_2x_streaming(&luma, w, h, minimum, maximum));
    }
    let gray = materialize_gray_8p8(&luma, minimum, maximum);
    if scale <= 1 {
        return Ok(gray);
    }
    Ok(upscale_separable(&gray, w, h, scale))
}

// ---- public seam ----

/// The public entry point: PNG bytes in, 8-bit grayscale plane out.
///
/// Returns the plane with its post-scale geometry, ready to hand straight to an
/// OCR engine — no intermediate file, which is the other half of what this
/// service exists to remove.
pub fn preprocess(
    raw: &[u8],
    max_scale: usize,
    pixel_budget: u64,
) -> Result<(Vec<u8>, usize, usize), String> {
    let (buf, w, h, ct, bd) = decode(raw)?;
    let scale = factor_within_budget(w, h, max_scale, pixel_budget);
    let scaled = transform_kstream(&buf, w, h, ct, bd, scale)?;
    Ok((scaled, w * scale, h * scale))
}

/// The scale `preprocess` would apply, computed from the PNG header alone
/// (no frame decode). Exposed so the caller can route scale-1 requests around
/// the preprocess entirely — see read_page in main.rs for the measured WHY.
pub fn planned_scale(raw: &[u8], max_scale: usize, pixel_budget: u64) -> Result<usize, String> {
    let mut options = png::DecodeOptions::default();
    options.set_ignore_checksums(true);
    options.set_ignore_text_chunk(true);
    options.set_ignore_iccp_chunk(true);
    let dec = png::Decoder::new_with_options(Cursor::new(raw), options);
    let reader = dec.read_info().map_err(|e| format!("png read_info: {e}"))?;
    let info = reader.info();
    Ok(factor_within_budget(
        info.width as usize,
        info.height as usize,
        max_scale,
        pixel_budget,
    ))
}

/// Largest factor up to `max` whose output respects `pixel_budget`.
///
/// The upscale is quadratic, so an unbounded factor on a tall page can cost more
/// than every other page in a batch combined. The budget belongs to the caller;
/// this only picks the best factor that honors it, and 1 is always admissible.
fn factor_within_budget(w: usize, h: usize, max: usize, pixel_budget: u64) -> usize {
    let pixels = w as u64 * h as u64;
    let mut factor = max.max(1);

    while factor > 1 && pixels * (factor as u64) * (factor as u64) > pixel_budget {
        factor -= 1;
    }

    factor
}
