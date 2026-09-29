//! In-process OCR engine for route66-snapbot.
//!
//! WHY A LIBRARY (GH #4115). Owner 2026-09-29: "also avoid image transmission;
//! do the OCR at capture point in snapbot" and "what can't it pass in-memory?!".
//! The former `--stdio` child took each screenshot as base64 JSON over a pipe;
//! the snapbot handler now links this crate and hands it the PNG bytes it just
//! captured. One `Engine` per pool process owns one warm Tesseract instance.
//!
//! GENERIC BY CONSTRUCTION (owner 2026-09-29: "can you keep domain knowledge out
//! of snapbot"): every policy choice -- segmentation mode, language, dpi,
//! upscale, pixel budget -- arrives as a `Pass`; nothing here knows a caller.

pub mod kstream;

use leptess::leptonica::Pix;
use leptess::tesseract::TessApi;
use std::time::Instant;

/// One OCR read's parameters. Zero values normalize to the historical ocrd
/// defaults because callers may encode unset numbers as zero.
#[derive(Clone, Debug)]
pub struct Pass {
    pub psm: u32,
    pub lang: String,
    pub dpi: u32,
    pub upscale: u32,
    pub pixel_budget: u64,
}

impl Pass {
    /// Apply the ocrd defaults (psm 3, eng, 300 dpi, upscale 1, 20 MPix).
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
        if self.upscale == 0 {
            self.upscale = 1;
        }
        if self.pixel_budget == 0 {
            self.pixel_budget = 20_000_000;
        }
        self
    }
}

/// A rectangle of the ORIGINAL image in pixels. OCR of a region reads only
/// those pixels (Tesseract's SetRectangle over the already-set image), so a
/// caller that knows where the text is pays for that area alone.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

/// Where one pass spent its time. decode_ms is non-zero only on the pass that
/// actually decoded the image; later passes reuse the decoded pixels.
#[derive(Clone, Debug, Default)]
pub struct PassTiming {
    pub scale: usize,
    pub decode_ms: u64,
    pub preprocess_ms: u64,
    pub ocr_ms: u64,
    /// Per-region recognition ms, in region order (empty for a whole-image read).
    pub region_ms: Vec<u64>,
}

/// Clamp `r` to a `w` x `h` image; None when nothing of it remains.
fn clamp(r: &Rect, w: i64, h: i64) -> Option<(i64, i64, i64, i64)> {
    let x0 = r.x.clamp(0, w);
    let y0 = r.y.clamp(0, h);
    let x1 = (r.x + r.width).clamp(0, w);
    let y1 = (r.y + r.height).clamp(0, h);
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// One captured PNG plus its lazily decoded forms. Scale-1 passes read the
/// Leptonica decode (the measured ocrd behavior: feeding the original image to
/// Leptonica keeps Tesseract off its slow grayscale path on tall pages); scaled
/// passes read the kstream decode. Each decoder runs at most once per image.
pub struct Image<'a> {
    raw: &'a [u8],
    pix: Option<Pix>,
    decoded: Option<kstream::Decoded>,
}

impl<'a> Image<'a> {
    pub fn new(raw: &'a [u8]) -> Self {
        Self { raw, pix: None, decoded: None }
    }
}

/// A resident Tesseract engine. A language change is the only reason to
/// rebuild it; one owner means no C handle is ever shared across threads.
#[derive(Default)]
pub struct Engine {
    slot: Option<(String, TessApi)>,
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

impl Engine {
    pub fn new() -> Self {
        Self { slot: None }
    }

    fn api(&mut self, lang: &str) -> Result<&mut TessApi, String> {
        let rebuild = match &self.slot {
            Some((have, _)) => have != lang,
            None => true,
        };
        if rebuild {
            let tess = TessApi::new(None, lang).map_err(|err| format!("init tesseract ({lang}): {err}"))?;
            self.slot = Some((lang.to_string(), tess));
        }
        Ok(&mut self.slot.as_mut().expect("engine inserted above").1)
    }

    /// Read one pass over `img` (only `regions` of it when non-empty, their
    /// text concatenated in region order), returning the text and timings.
    pub fn read(&mut self, img: &mut Image, pass: &Pass, regions: &[Rect]) -> Result<(String, PassTiming), String> {
        if img.raw.is_empty() {
            return Err("empty image".to_string());
        }
        let mut timing = PassTiming::default();
        let (w, h) = kstream::dimensions(img.raw).unwrap_or((0, 0));
        // The upscale budget applies to the pixels actually read.
        let area: u64 = if regions.is_empty() {
            w as u64 * h as u64
        } else {
            regions.iter().filter_map(|r| clamp(r, w as i64, h as i64)).map(|(_, _, rw, rh)| (rw * rh) as u64).max().unwrap_or(0)
        };
        let scale = if w == 0 { 1 } else { kstream::factor_within_budget(area as usize, 1, pass.upscale as usize, pass.pixel_budget) };
        timing.scale = scale;

        if scale == 1 {
            // Scale one: the Leptonica decode, set once per image.
            if img.pix.is_none() {
                let t = Instant::now();
                img.pix = Some(leptess::leptonica::pix_read_mem(img.raw).map_err(|err| format!("pix_read_mem: {err:?}"))?);
                timing.decode_ms = ms(t);
            }
            let tess = self.api(&pass.lang)?;
            set_psm(tess, pass.psm);
            let t = Instant::now();
            tess.set_image(img.pix.as_ref().expect("pix decoded above"));
            tess.set_source_resolution(pass.dpi as i32);
            let text = recognize(tess, regions, w as i64, h as i64, 1, &mut timing)?;
            timing.ocr_ms = ms(t);
            return Ok((text, timing));
        }

        // Scale two or more: the kstream decode once, then the per-pass
        // grayscale + contrast-stretch + upscale into the engine's own buffer.
        if img.decoded.is_none() {
            let t = Instant::now();
            img.decoded = Some(kstream::decode_png(img.raw)?);
            timing.decode_ms = ms(t);
        }
        let t = Instant::now();
        let (pixels, width, height) = kstream::transform_at(img.decoded.as_ref().expect("decoded above"), scale)?;
        timing.preprocess_ms = ms(t);
        let tess = self.api(&pass.lang)?;
        set_psm(tess, pass.psm);
        let t = Instant::now();
        tess.raw
            .set_image(&pixels, width as i32, height as i32, 1, width as i32)
            .map_err(|err| format!("set_image: {err}"))?;
        tess.set_source_resolution(pass.dpi as i32);
        let text = recognize(tess, regions, w as i64, h as i64, scale as i64, &mut timing)?;
        timing.ocr_ms = ms(t);
        Ok((text, timing))
    }
}

/// Recognize the whole set image, or each region of it in order.
fn recognize(tess: &mut TessApi, regions: &[Rect], w: i64, h: i64, scale: i64, timing: &mut PassTiming) -> Result<String, String> {
    if regions.is_empty() {
        return tess.get_utf8_text().map_err(|err| format!("ocr: {err}"));
    }
    let mut text = String::new();
    for r in regions {
        let t = Instant::now();
        if let Some((x, y, rw, rh)) = clamp(r, w, h) {
            let _ = tess.raw.set_rectangle((x * scale) as i32, (y * scale) as i32, (rw * scale) as i32, (rh * scale) as i32);
            text.push_str(&tess.get_utf8_text().map_err(|err| format!("ocr: {err}"))?);
        }
        timing.region_ms.push(ms(t));
    }
    Ok(text)
}

/// The page segmentation mode is set on every read so warm state never leaks
/// from one caller's pass into the next.
fn set_psm(tess: &mut TessApi, psm: u32) {
    if let (Ok(name), Ok(value)) = (
        std::ffi::CString::new("tessedit_pageseg_mode"),
        std::ffi::CString::new(psm.to_string()),
    ) {
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
}
