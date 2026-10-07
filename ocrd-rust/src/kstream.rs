//! kstream — the image pipeline: a raw BGRA frame in, OCR-ready grayscale plane out.
//!
//! WHY NO CODEC (owner 2026-10-07: "Embed Chromium in snapbot ... hands over the
//! raw pixel buffer in the same process"). The frame arrives as the BGRA buffer
//! CEF's off-screen renderer painted; the PNG decode this module used to carry is
//! deleted with the CDP screenshot path. Nothing here encodes or decodes.
//!
//! The arithmetic is the KSTREAM winner of `img-scaling-benchmark` (8.8 fixed-point
//! luma kept from the min/max pass, global min/max contrast stretch with a constant
//! image mapping to black, round-half-up bilinear upscale), now reading BGRA order.
//!
//! Error style: `Result<_, String>`, never a panic: the binary builds with
//! `panic = "abort"`, so a bad frame must only fail its own request.

// ---- bilinear upscale tables ----

const FIX_SHIFT: u32 = 16;
const FIX_ONE: u32 = 1 << FIX_SHIFT;

/// Precomputes the (i0, i1, weight) triple of every output position on one
/// axis. The sample position s = (o+0.5)/scale - 0.5 has only `scale` distinct
/// fractional parts, so the weights repeat with period `scale`; the CLAMPED
/// indices do not repeat at the two edges, so the table is materialized at full
/// output length (a few tens of KB, read sequentially). Built once per image.
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
/// pass over two already-filtered rows: the vertical pass reads two contiguous
/// rows instead of gathering four taps per output pixel.
fn horiz_pass(src: &[u8], w: usize, h: usize, ow: usize, x0s: &[u32], x1s: &[u32], wxs: &[u32]) -> Vec<u32> {
    let mut mid = vec![0u32; ow * h];
    for y in 0..h {
        let row = &src[y * w..][..w];
        let mrow = &mut mid[y * ow..][..ow];
        for ox in 0..ow {
            let wx = wxs[ox];
            mrow[ox] = row[x0s[ox] as usize] as u32 * (FIX_ONE - wx) + row[x1s[ox] as usize] as u32 * wx;
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
            out[ox] = ((m0[ox] as u64 * iwy + m1[ox] as u64 * wy + (1 << (2 * FIX_SHIFT - 1))) >> (2 * FIX_SHIFT)) as u8;
        }
    }
    dst
}

// ---- factor-2 specialization ----

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
    // Each expansion receives literal coefficients: four branch-free row
    // kernels instead of one closure with runtime weights.
    macro_rules! write_row {
        ($dst_y:expr, $row0:expr, $row1:expr, $c0:expr, $c1:expr) => {{
            let destination = dst.as_mut_ptr().add($dst_y * ow);
            let c0 = $c0 as u32;
            let c1 = $c1 as u32;
            for x in 0..ow {
                *destination.add(x) = ((c0 * u32::from(*$row0.add(x)) + c1 * u32::from(*$row1.add(x)) + 8) >> 4) as u8;
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

/// Upscale a gray plane (or a crop of it) by a whole factor: the engine
/// rescales the one converted plane per pass.
pub fn upscale_gray(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
    if scale <= 1 {
        return src.to_vec();
    }
    if scale == 2 {
        return upscale_2x(src, w, h);
    }
    upscale_separable(src, w, h, scale)
}

// ---- BGRA -> stretched gray ----

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

/// The CEF paint buffer (BGRA, `stride` bytes per row, opaque page pixels) to a
/// contrast-stretched 8-bit gray plane: one pass building the 8.8 luma plane and
/// its min/max, one pass through the stretch table. Same weights as the former
/// RGB path (77/150/29 on R/G/B), only the channel order differs.
pub fn gray_from_bgra(buf: &[u8], w: usize, h: usize, stride: usize) -> Result<Vec<u8>, String> {
    if w == 0 || h == 0 {
        return Err(format!("empty frame {w}x{h}"));
    }
    if stride < w * 4 || buf.len() < stride * (h - 1) + w * 4 {
        return Err(format!("frame buffer of {} bytes is short for {w}x{h} at stride {stride}", buf.len()));
    }
    let mut luma = vec![0u16; w * h];
    let mut minimum = u16::MAX;
    let mut maximum = 0u16;
    for y in 0..h {
        let row = &buf[y * stride..][..w * 4];
        let out = &mut luma[y * w..][..w];
        for (o, p) in out.iter_mut().zip(row.chunks_exact(4)) {
            // BGRA: p[0] = B, p[1] = G, p[2] = R.
            let v = 77 * u16::from(p[2]) + 150 * u16::from(p[1]) + 29 * u16::from(p[0]);
            *o = v;
            minimum = minimum.min(v);
            maximum = maximum.max(v);
        }
    }
    // Full-range frames skip the data-dependent lookup, as the RGB path did.
    let mut gray = vec![0u8; luma.len()];
    if minimum == 0 && maximum == 255 * 256 {
        for (o, &v) in gray.iter_mut().zip(&luma) {
            *o = ((v + 128) >> 8) as u8;
        }
    } else {
        let table = luma_stretch_table_8p8(minimum, maximum);
        for (o, &v) in gray.iter_mut().zip(&luma) {
            *o = table[usize::from(v - minimum)];
        }
    }
    Ok(gray)
}

/// Largest factor up to `max` whose output respects `pixel_budget`.
///
/// The upscale is quadratic, so an unbounded factor on a tall page can cost more
/// than every other page in a batch combined. The budget belongs to the caller;
/// this only picks the best factor that honors it, and 1 is always admissible.
pub fn factor_within_budget(w: usize, h: usize, max: usize, pixel_budget: u64) -> usize {
    let pixels = w as u64 * h as u64;
    let mut factor = max.max(1);
    while factor > 1 && pixels * (factor as u64) * (factor as u64) > pixel_budget {
        factor -= 1;
    }
    factor
}
