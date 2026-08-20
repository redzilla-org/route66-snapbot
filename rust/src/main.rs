// Variant C: the Rust implementation of the image preprocessing pipeline.
// Same math as the Go variants: luma grayscale -> global min/max contrast stretch
// -> integer bilinear upscale -> binary PGM out. Single-threaded, phase-timed.
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

// Decode a PNG into (rgba-ish bytes, width, height, channels).
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

// Grayscale + global contrast stretch at source resolution.
fn gray_stretch(
    buf: &[u8],
    w: usize,
    h: usize,
    ct: png::ColorType,
    bd: png::BitDepth,
) -> Vec<u8> {
    assert_eq!(bd, png::BitDepth::Eight, "only 8-bit PNGs supported");
    let ch = match ct {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => panic!("indexed png: expand not enabled"),
    };
    let n = w * h;
    let mut lum = vec![0f64; n];
    let mut min_l = f64::MAX;
    let mut max_l = f64::MIN;
    for i in 0..n {
        let p = &buf[i * ch..];
        // The Go At() path yields ALPHA-PREMULTIPLIED 8-bit channels; png gives
        // non-premultiplied, so premultiply here to keep the values identical.
        let l = match ch {
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
        };
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

// Integer bilinear upscale with clamped edge sampling -- same sample grid as the Go
// reference: source coord = (out + 0.5)/scale - 0.5.
fn upscale(src: &[u8], w: usize, h: usize, scale: usize) -> Vec<u8> {
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let inp = &args[1];
    let outp = &args[2];
    let raw = std::fs::read(inp).expect("read input");
    let (warm, iters) = (3usize, 15usize);
    let (mut dec, mut tr, mut enc, mut tot) = (vec![], vec![], vec![], vec![]);
    let (mut ow, mut oh, mut scale) = (0usize, 0usize, 0usize);
    for i in 0..(warm + iters) {
        let t0 = Instant::now();
        let (buf, w, h, ct, bd) = decode(&raw);
        let t1 = Instant::now();
        scale = upscale_factor_for(w, h);
        let small = gray_stretch(&buf, w, h, ct, bd);
        let big = upscale(&small, w, h, scale);
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
    println!(
        "{{\"variant\":\"C\",\"input\":\"{}\",\"scale\":{},\"outW\":{},\"outH\":{},\"outBytes\":{},\"iters\":{},\
\"decode\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"transform\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"encode\":{{\"min\":{},\"med\":{},\"mean\":{}}},\
\"total\":{{\"min\":{},\"med\":{},\"mean\":{}}}}}",
        inp, scale, ow, oh, sz, iters, dn, dm, da, tn, tm, ta, en, em, ea, on, om, oa
    );
}
