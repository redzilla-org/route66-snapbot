//! The one PNG encode a screenshot gets: BGRA frame -> RGB8 PNG.
//!
//! WHY (owner 2026-10-07): the stored screenshot and the attested evidence stay
//! PNG with the same width/height, and snapbot encodes it itself from the BGRA
//! buffer, once. RGB8 matches the opaque PNGs Page.captureScreenshot produced;
//! the fast zlib level was the owner's order for the writer.

use crate::shm::Frame;
use anyhow::{anyhow, Result};

/// Encode `f` as an 8-bit RGB PNG.
pub fn encode(f: &Frame) -> Result<Vec<u8>> {
    let src = f.seg.as_slice();
    // BGRA -> RGB, row by row (the stride may pad rows).
    let mut rgb = Vec::with_capacity(f.width * f.height * 3);
    for y in 0..f.height {
        for p in src[y * f.stride..][..f.width * 4].chunks_exact(4) {
            rgb.extend_from_slice(&[p[2], p[1], p[0]]);
        }
    }
    let mut out = Vec::with_capacity(rgb.len() / 4);
    let mut enc = png::Encoder::new(&mut out, f.width as u32, f.height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut w = enc.write_header().map_err(|e| anyhow!("png header: {e}"))?;
    w.write_image_data(&rgb).map_err(|e| anyhow!("png data: {e}"))?;
    w.finish().map_err(|e| anyhow!("png finish: {e}"))?;
    Ok(out)
}
