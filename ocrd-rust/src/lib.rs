//! In-process OCR engine for route66-snapbot.
//!
//! WHY A LIBRARY (GH #4115). Owner 2026-09-29: "also avoid image transmission;
//! do the OCR at capture point in snapbot". The snapbot handler links this crate
//! and hands it the raw BGRA frame CEF just painted (owner 2026-10-07: "hands
//! over the raw pixel buffer in the same process"). One `Engine` per pool
//! process owns one warm Tesseract instance.
//!
//! NO CODEC: the frame is converted exactly once into a contrast-stretched 8-bit
//! gray plane (`Gray`). Every pass, region crop and rescale reads that plane in
//! memory and hands raw pixels to Tesseract (SetImage); nothing is decoded or
//! encoded anywhere between the paint and the read.
//!
//! GENERIC BY CONSTRUCTION (owner 2026-09-29: "can you keep domain knowledge out
//! of snapbot"): every policy choice -- segmentation mode, language, dpi, scale,
//! pixel budget -- arrives as a `Pass`; nothing here knows a caller.

pub mod kstream;

use leptess::tesseract::TessApi;
use std::time::Instant;

/// One OCR read's parameters. `scale` < 1 is a downscaled (cheap) read, 1 the
/// captured pixels, an integer > 1 an upscaled (expensive) read.
#[derive(Clone, Debug)]
pub struct Pass {
    pub psm: u32,
    pub lang: String,
    pub dpi: u32,
    pub scale: f64,
    pub pixel_budget: u64,
}

impl Pass {
    /// The historical ocrd defaults (psm 3, eng, 300 dpi, scale 1, 20 MPix)
    /// for unset (zero) fields.
    pub fn normalized(mut self) -> Self {
        if self.psm == 0 {
            self.psm = 3;
        }
        if self.lang.is_empty() {
            self.lang = "eng".to_string();
        }
        if self.dpi == 0 {
            self.dpi = 300;
        }
        if self.scale == 0.0 {
            self.scale = 1.0;
        }
        if self.pixel_budget == 0 {
            self.pixel_budget = 20_000_000;
        }
        self
    }

    /// Fail fast on a scale this engine cannot honor: downscales are any
    /// factor in (0, 1); upscales are whole factors.
    pub fn validate(&self) -> Result<(), String> {
        let s = self.scale;
        if !(s.is_finite() && s > 0.0 && (s <= 1.0 || s.fract() == 0.0) && s <= 4.0) {
            return Err(format!("scale {s} must be in (0, 1] or a whole factor 2..4"));
        }
        Ok(())
    }
}

/// A rectangle of the captured image in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

/// The screenshot as OCR reads it: one contrast-stretched 8-bit gray plane.
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// The raw BGRA frame CEF painted -> gray plane: the only conversion a frame
/// gets, straight from the paint buffer (no codec in between).
pub fn from_bgra(buf: &[u8], w: usize, h: usize, stride: usize) -> Result<Gray, String> {
    Ok(Gray { w, h, px: kstream::gray_from_bgra(buf, w, h, stride)? })
}

/// Clamp `r` to a `w` x `h` image; None when nothing of it remains.
fn clamp(r: &Rect, w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let (w, h) = (w as i64, h as i64);
    let x0 = r.x.clamp(0, w);
    let y0 = r.y.clamp(0, h);
    let x1 = (r.x + r.width).clamp(0, w);
    let y1 = (r.y + r.height).clamp(0, h);
    (x1 > x0 && y1 > y0).then_some((x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize))
}

/// Copy the rect out of the plane (a view would do, but Tesseract copies
/// its input anyway and a crop keeps the resample loops simple).
fn crop(g: &Gray, x: usize, y: usize, w: usize, h: usize) -> Gray {
    if x == 0 && y == 0 && w == g.w && h == g.h {
        return Gray { w, h, px: g.px.clone() };
    }
    let mut px = Vec::with_capacity(w * h);
    for row in y..y + h {
        px.extend_from_slice(&g.px[row * g.w + x..][..w]);
    }
    Gray { w, h, px }
}

/// Per-axis source spans of an area-average downscale: output o averages
/// source [lo[o], hi[o]).
fn spans(n: usize, on: usize) -> (Vec<usize>, Vec<usize>) {
    let lo = (0..on).map(|o| o * n / on).collect();
    let hi = (0..on).map(|o| (((o + 1) * n + on - 1) / on).min(n).max(o * n / on + 1)).collect();
    (lo, hi)
}

/// Area-average (box) downscale: every source pixel contributes, so thin
/// glyph strokes fade instead of vanishing the way point sampling drops them.
fn downscale(g: &Gray, s: f64) -> Gray {
    let ow = ((g.w as f64 * s).round() as usize).max(1);
    let oh = ((g.h as f64 * s).round() as usize).max(1);
    let (xl, xh) = spans(g.w, ow);
    let (yl, yh) = spans(g.h, oh);
    // Column sums per output row band, then one division per output pixel.
    let mut px = vec![0u8; ow * oh];
    let mut col = vec![0u32; g.w];
    for oy in 0..oh {
        col.iter_mut().for_each(|c| *c = 0);
        for y in yl[oy]..yh[oy] {
            for (c, &p) in col.iter_mut().zip(&g.px[y * g.w..][..g.w]) {
                *c += p as u32;
            }
        }
        let rows = (yh[oy] - yl[oy]) as u32;
        let out = &mut px[oy * ow..][..ow];
        for ox in 0..ow {
            let sum: u32 = col[xl[ox]..xh[ox]].iter().sum();
            let n = rows * (xh[ox] - xl[ox]) as u32;
            out[ox] = ((sum + n / 2) / n) as u8;
        }
    }
    Gray { w: ow, h: oh, px }
}

/// Rescale a plane by the pass scale; an upscale steps down to honor the budget.
fn rescale(g: Gray, pass: &Pass) -> Gray {
    if pass.scale < 1.0 {
        return downscale(&g, pass.scale);
    }
    let k = kstream::factor_within_budget(g.w, g.h, pass.scale as usize, pass.pixel_budget);
    if k <= 1 {
        return g;
    }
    let px = kstream::upscale_gray(&g.px, g.w, g.h, k);
    Gray { w: g.w * k, h: g.h * k, px }
}

/// Where one pass spent its time.
#[derive(Clone, Debug, Default)]
pub struct PassTiming {
    /// Crop + rescale of the decoded plane.
    pub resample_ms: u64,
    /// Tesseract recognition.
    pub ocr_ms: u64,
    /// Pixels Tesseract read.
    pub pixels: u64,
}

/// A resident Tesseract engine per (datapath, lang). A language change is the
/// only reason to rebuild it; one owner means no C handle crosses threads.
#[derive(Default)]
pub struct Engine {
    datapath: Option<String>,
    slot: Option<(String, TessApi)>,
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// An engine reading models from `datapath` instead of TESSDATA_PREFIX.
    pub fn with_datapath(datapath: &str) -> Self {
        Self { datapath: Some(datapath.to_string()), slot: None }
    }

    fn api(&mut self, lang: &str) -> Result<&mut TessApi, String> {
        if self.slot.as_ref().map(|(have, _)| have != lang).unwrap_or(true) {
            let tess = TessApi::new(self.datapath.as_deref(), lang).map_err(|err| format!("init tesseract ({lang}): {err}"))?;
            self.slot = Some((lang.to_string(), tess));
        }
        Ok(&mut self.slot.as_mut().expect("engine inserted above").1)
    }

    /// Read one pass over the decoded plane: the whole image, or each of
    /// `regions` (cropped BEFORE rescaling, so a crop's cost is its own
    /// area) with their text concatenated in region order.
    pub fn read(&mut self, g: &Gray, pass: &Pass, regions: &[Rect]) -> Result<(String, PassTiming), String> {
        pass.validate()?;
        let mut timing = PassTiming::default();
        let whole = [Rect { x: 0, y: 0, width: g.w as i64, height: g.h as i64 }];
        let rects = if regions.is_empty() { &whole[..] } else { regions };
        let mut text = String::new();
        for r in rects {
            let Some((x, y, w, h)) = clamp(r, g.w, g.h) else { continue };
            let t = Instant::now();
            let img = rescale(crop(g, x, y, w, h), pass);
            timing.resample_ms += ms(t);
            timing.pixels += (img.w * img.h) as u64;
            let tess = self.api(&pass.lang)?;
            set_psm(tess, pass.psm);
            let t = Instant::now();
            tess.raw
                .set_image(&img.px, img.w as i32, img.h as i32, 1, img.w as i32)
                .map_err(|err| format!("set_image: {err}"))?;
            tess.set_source_resolution(pass.dpi as i32);
            text.push_str(&tess.get_utf8_text().map_err(|err| format!("ocr: {err}"))?);
            timing.ocr_ms += ms(t);
        }
        Ok((text, timing))
    }
}

/// The page segmentation mode is set on every read so warm state never leaks
/// from one caller's pass into the next.
fn set_psm(tess: &mut TessApi, psm: u32) {
    if let (Ok(name), Ok(value)) = (std::ffi::CString::new("tessedit_pageseg_mode"), std::ffi::CString::new(psm.to_string())) {
        let _ = tess.raw.set_variable(&name, &value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upscale_steps_down_to_honor_the_pixel_budget() {
        // WHY: the budget is what keeps a tall page's upscaled passes bounded;
        // the factor must step down, and 1 is always admissible.
        assert_eq!(kstream::factor_within_budget(1366, 5477, 3, 20_000_000), 1);
        assert_eq!(kstream::factor_within_budget(1366, 900, 3, 20_000_000), 3);
        assert_eq!(kstream::factor_within_budget(1366, 2000, 3, 20_000_000), 2);
    }

    #[test]
    fn downscale_averages_every_source_pixel() {
        // WHY: point sampling would drop one-pixel glyph strokes outright.
        let g = Gray { w: 4, h: 2, px: vec![0, 255, 0, 0, 255, 0, 0, 0] };
        let d = downscale(&g, 0.5);
        assert_eq!((d.w, d.h, d.px.clone()), (2, 1, vec![128, 0]));
    }

    #[test]
    fn scale_must_be_a_downscale_or_whole_upscale() {
        let p = |scale| Pass { psm: 3, lang: "eng".into(), dpi: 300, scale, pixel_budget: 1 };
        assert!(p(0.5).validate().is_ok());
        assert!(p(2.0).validate().is_ok());
        assert!(p(1.5).validate().is_err());
        assert!(p(-1.0).validate().is_err());
    }
}
