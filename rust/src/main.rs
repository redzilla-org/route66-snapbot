// Rust side of the image-preprocessing benchmark.
//
// Pipeline, identical in every variant: PNG decode -> luma grayscale -> global
// min/max contrast stretch -> integer bilinear upscale -> binary PGM out.
// Single-threaded, phase-timed, 3 warmups + 15 timed iterations, median reported.
//
// WHY MORE THAN ONE VARIANT: the first round of this benchmark compared an
// unoptimized Rust scaler ("C", a faithful port of Go's naive variant) against a
// four-times-optimized Go scaler ("D") and reported the difference as a language
// result. It was not one. The variants below port each of Go D's four
// optimizations into Rust so the comparison is like-for-like, and keep C in the
// binary so every optimized variant can be diffed against it pixel by pixel.
//
//   C     naive: f64 luma side-buffer, fused 4-tap f64 bilinear      (reference)
//   D     no luma buffer + separable two-pass 16.16 fixed-point scaler
//   DB    D but KEEPING the f64 luma buffer -- the fastest measured, because in
//         Rust the "kill the buffer" optimization is a LOSS (see below)
//   DS    D with an explicit AVX2 vertical pass                      (hypothesis: SIMD)
//   D1    no luma buffer + C's naive scaler        (isolates opt 4: kill the buffer)
//   D2    C's buffered stretch + single-pass fixed (isolates opt 1: axis tables)
//   D3    no luma buffer + single-pass fixed       (D minus separability)
//   F64   D with the separable scaler carrying f64 (isolates opt 3: not-f64)
//   F32   D with the separable scaler carrying f32
//
// Every optimized variant is validated against C's output with `rsbench cmp`.
use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Instant;

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
    let dec = png::Decoder::new(raw);
    let mut reader = dec.read_info().expect("png read_info");
    let mut buf = vec![0u8; reader.output_buffer_size()];
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

// Variant C's stretch: a full w*h Vec<f64> of luma, written in pass 1 and read
// back in pass 2. 9.8 MB on big.png, allocated, zeroed by the allocator path,
// written and re-read purely to avoid recomputing three multiplies.
fn gray_stretch_buf(buf: &[u8], w: usize, h: usize, ct: png::ColorType, bd: png::BitDepth) -> Vec<u8> {
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
fn gray_stretch_nobuf(buf: &[u8], w: usize, h: usize, ct: png::ColorType, bd: png::BitDepth) -> Vec<u8> {
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
            mrow[ox] = row[x0s[ox] as usize] as f64 * (1.0 - wx) + row[x1s[ox] as usize] as f64 * wx;
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
            mrow[ox] = row[x0s[ox] as usize] as f32 * (1.0 - wx) + row[x1s[ox] as usize] as f32 * wx;
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
unsafe fn vert_pass_avx2(mid: &[u32], ow: usize, oh: usize, y0s: &[u32], y1s: &[u32], wys: &[u32], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    let round = _mm256_set1_epi64x(1i64 << (2 * FIX_SHIFT - 1));
    // Byte selector: take byte 0 of each of the 4 dwords in each 128-bit lane
    // and pack them into the low 4 bytes of that lane. Values are <= 255 so only
    // the low byte of each result dword carries information.
    let sel = _mm256_setr_epi8(
        0, 4, 8, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 4, 8, 12, -1, -1, -1, -1, -1, -1, -1, -1,
        -1, -1, -1, -1,
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
    for y in 0..h {
        let row = &src[y * w..][..w];
        let mrow = &mut mid[y * ow..][..ow];
        // Edge columns are the clamped x0 == x1 case: both taps are the same
        // pixel, so the weighted sum is 4x that pixel.
        mrow[0] = row[0] as u16 * 4;
        mrow[ow - 1] = row[w - 1] as u16 * 4;
        for x in 0..w - 1 {
            let a = row[x] as u16;
            let b = row[x + 1] as u16;
            mrow[2 * x + 1] = 3 * a + b;
            mrow[2 * x + 2] = a + 3 * b;
        }
    }
    let mut dst = vec![0u8; ow * oh];
    {
        let mut row_out = |dsty: usize, m0: &[u16], m1: &[u16], c0: u32, c1: u32| {
            let out = &mut dst[dsty * ow..][..ow];
            for ox in 0..ow {
                out[ox] = ((c0 * m0[ox] as u32 + c1 * m1[ox] as u32 + 8) >> 4) as u8;
            }
        };
        let first = mid[0..ow].to_vec();
        row_out(0, &first, &first, 2, 2);
        let last = mid[(h - 1) * ow..h * ow].to_vec();
        row_out(oh - 1, &last, &last, 2, 2);
        for y in 0..h - 1 {
            let (lo, hi) = mid.split_at((y + 1) * ow);
            let m0 = &lo[y * ow..][..ow];
            let m1 = &hi[..ow];
            row_out(2 * y + 1, m0, m1, 3, 1);
            row_out(2 * y + 2, m0, m1, 1, 3);
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
    let f = File::create(path).expect("create out");
    let mut bw = BufWriter::with_capacity(1 << 20, f);
    write!(bw, "P5\n{} {}\n255\n", w, h).unwrap();
    bw.write_all(pix).unwrap();
    bw.flush().unwrap();
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

// Sub-phase instrumentation, used INSTEAD of a sampling profiler.
//
// Honest statement of method: no sampling profiler was available on this host.
// ETW (`wpr -start CPU`) requires elevation and was refused; cargo-flamegraph
// needs blondie or dtrace, neither installed; no Superluminal or VTune present.
// So the attribution below is manual coarse instrumentation -- each stage of the
// recommended variant timed with its own Instant, median over the same 15
// iterations the benchmark uses. It gives per-stage totals, not per-instruction
// hot spots, and it is labelled as such rather than dressed up as a profile.
fn profile(inp: &str, iters: usize) {
    let raw = std::fs::read(inp).expect("read input");
    let mut acc: Vec<(&str, Vec<f64>)> = vec![
        ("decode", vec![]),
        ("stretch:minmax", vec![]),
        ("stretch:apply", vec![]),
        ("scale:tables", vec![]),
        ("scale:horiz", vec![]),
        ("scale:vert", vec![]),
    ];
    let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1000.0;
    for _ in 0..iters {
        let t0 = Instant::now();
        let (buf, w, h, ct, bd) = decode(&raw);
        let t1 = Instant::now();
        let scale = upscale_factor_for(w, h);
        let ch = channels(ct, bd);
        let n = w * h;

        // stretch pass 1: luma + min/max only
        let mut lum = vec![0f64; n];
        let (mut min_l, mut max_l) = (f64::MAX, f64::MIN);
        for i in 0..n {
            let l = luma_at(&buf[i * ch..], ch);
            lum[i] = l;
            if l < min_l { min_l = l; }
            if l > max_l { max_l = l; }
        }
        let t2 = Instant::now();

        // stretch pass 2: apply the stretch into the u8 plane
        let mut span = max_l - min_l;
        if span < 1e-6 { span = 1.0; }
        let mut small = vec![0u8; n];
        for i in 0..n {
            let mut v = (lum[i] - min_l) / span * 255.0;
            if v < 0.0 { v = 0.0; } else if v > 255.0 { v = 255.0; }
            small[i] = (v + 0.5) as u8;
        }
        let t3 = Instant::now();

        // scaler, split into table build / horizontal / vertical
        let (t4, t5, t6);
        if scale > 1 {
            let (ow, oh) = (w * scale, h * scale);
            let (x0s, x1s, wxs) = bilinear_axis(w, scale);
            let (y0s, y1s, wys) = bilinear_axis(h, scale);
            t4 = Instant::now();
            let mid = horiz_pass(&small, w, h, ow, &x0s, &x1s, &wxs);
            t5 = Instant::now();
            let mut dst = vec![0u8; ow * oh];
            for oy in 0..oh {
                let wy = wys[oy] as u64;
                let iwy = FIX_ONE as u64 - wy;
                let m0 = &mid[y0s[oy] as usize * ow..][..ow];
                let m1 = &mid[y1s[oy] as usize * ow..][..ow];
                let out = &mut dst[oy * ow..][..ow];
                for ox in 0..ow {
                    out[ox] = ((m0[ox] as u64 * iwy + m1[ox] as u64 * wy
                        + (1 << (2 * FIX_SHIFT - 1))) >> (2 * FIX_SHIFT)) as u8;
                }
            }
            t6 = Instant::now();
            std::hint::black_box(&dst);
        } else {
            t4 = Instant::now(); t5 = t4; t6 = t4;
        }
        let vals = [ms(t0, t1), ms(t1, t2), ms(t2, t3), ms(t3, t4), ms(t4, t5), ms(t5, t6)];
        for (i, v) in vals.iter().enumerate() { acc[i].1.push(*v); }
    }
    println!("stage attribution (median of {} iters), {}", iters, inp);
    let mut tot = 0.0;
    let meds: Vec<(&str, f64)> = acc
        .iter_mut()
        .map(|(k, v)| { let (_, m, _) = stats(v); tot += m; (*k, m) })
        .collect();
    for (k, m) in meds {
        println!("  {:<16} {:8.2} ms  {:5.1}%", k, m, m * 100.0 / tot);
    }
    println!("  {:<16} {:8.2} ms", "SUM(no write)", tot);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "prof" {
        profile(&args[2], 15);
        return;
    }
    if args.len() > 1 && args[1] == "cmp" {
        cmp_pgm(&args[2], &args[3]);
        return;
    }
    // Usage: rsbench <VARIANT> <input.png> <output.pgm>
    let variant = args[1].clone();
    let inp = &args[2];
    let outp = &args[3];
    let raw = std::fs::read(inp).expect("read input");
    // Optional per-phase instrumentation of the transform, for profiling without
    // a sampling profiler: RSBENCH_PHASES=1 prints stretch/scale medians too.
    let phases = std::env::var("RSBENCH_PHASES").is_ok();
    let (warm, iters) = (3usize, 15usize);
    let (mut dec, mut tr, mut enc, mut tot) = (vec![], vec![], vec![], vec![]);
    let (mut st, mut sc) = (vec![], vec![]);
    let (mut ow, mut oh, mut scale) = (0usize, 0usize, 0usize);
    for i in 0..(warm + iters) {
        let t0 = Instant::now();
        let (buf, w, h, ct, bd) = decode(&raw);
        let t1 = Instant::now();
        scale = upscale_factor_for(w, h);
        let big = if phases {
            // Split path used ONLY under RSBENCH_PHASES; the timed default runs
            // the single fused `transform` call so the harness measures the same
            // thing the Go harness does.
            let a = Instant::now();
            let small = match variant.as_str() {
                "C" | "D2" | "DB" => gray_stretch_buf(&buf, w, h, ct, bd),
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
        ow = w * scale;
        oh = h * scale;
        let t2 = Instant::now();
        write_pgm(outp, &big, ow, oh);
        let t3 = Instant::now();
        if i >= warm {
            let msf = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
            dec.push(msf(t1 - t0));
            tr.push(msf(t2 - t1));
            enc.push(msf(t3 - t2));
            tot.push(msf(t3 - t0));
        }
    }
    let sz = std::fs::metadata(outp).unwrap().len();
    let (dn, dm, da) = stats(&mut dec);
    let (tn, tm, ta) = stats(&mut tr);
    let (en, em, ea) = stats(&mut enc);
    let (on, om, oa) = stats(&mut tot);
    let extra = if phases {
        let (_, sm, _) = stats(&mut st);
        let (_, cm, _) = stats(&mut sc);
        format!(",\"stretch\":{{\"med\":{}}},\"scale\":{{\"med\":{}}}", sm, cm)
    } else {
        String::new()
    };
    println!(
        "{{\"variant\":\"{}\",\"input\":\"{}\",\"scale\":{},\"outW\":{},\"outH\":{},\"outBytes\":{},\"iters\":{},\
\"decode\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"transform\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"encode\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"total\":{{\"min\":{},\"med\":{},\"mean\":{}}}{}}}",
        variant, inp, scale, ow, oh, sz, iters, dn, dm, da, tn, tm, ta, en, em, ea, on, om, oa, extra
    );
}
