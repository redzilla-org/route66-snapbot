//! Measure captured pixels before they leave the browser lane.
//!
//! Go receives small visual facts and a content-addressed URL, never a PNG.
//! The sampling and photo-cell definitions match web-regression's live gate.

use crate::shm::Frame;
use serde_json::{json, Value};

fn number(spec: &Value, name: &str, default: f64) -> f64 {
    spec.get(name).and_then(Value::as_f64).unwrap_or(default)
}

fn rgb(frame: &Frame, x: usize, y: usize) -> (u8, u8, u8) {
    let offset = y * frame.stride + x * 4;
    let px = &frame.seg.as_slice()[offset..offset + 4];
    (px[2], px[1], px[0])
}

fn luma(r: u8, g: u8, b: u8) -> f64 {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
}

// A cell is photo-like only when it has varied color and luminance; dark
// text on white or a flat placeholder cannot satisfy all three signals.
fn photo_cell(frame: &Frame, x0: usize, y0: usize, x1: usize, y1: usize, step: usize, spec: &Value) -> bool {
    let min_colors = number(spec, "min_colors", 10.0) as u32;
    let min_colored = number(spec, "min_colored_frac", 0.12);
    let min_var = number(spec, "min_cell_variance", 40.0);
    let min_sat = number(spec, "min_saturation", 24.0);
    let mut colors = [0u64; 8];
    let (mut count, mut colored) = (0usize, 0usize);
    let (mut sum, mut sum_sq) = (0.0, 0.0);
    for y in (y0..y1).step_by(step) {
        for x in (x0..x1).step_by(step) {
            let (r, g, b) = rgb(frame, x, y);
            let key = ((usize::from(r) >> 5) << 6) | ((usize::from(g) >> 5) << 3) | (usize::from(b) >> 5);
            colors[key >> 6] |= 1u64 << (key & 63);
            let max = r.max(g).max(b);
            let min = r.min(g).min(b);
            if f64::from(max - min) > min_sat { colored += 1; }
            let value = luma(r, g, b);
            sum += value;
            sum_sq += value * value;
            count += 1;
        }
    }
    if count == 0 { return false; }
    let mean = sum / count as f64;
    let variance = sum_sq / count as f64 - mean * mean;
    let distinct = colors.iter().map(|v| v.count_ones()).sum::<u32>();
    distinct >= min_colors && colored as f64 / count as f64 >= min_colored && variance >= min_var
}

// Return the 512-bit gallery signature from the same box-averaged luma grids
// the Go report used; the report can group captures without decoding PNGs.
fn gallery_hash(frame: &Frame) -> String {
    let (w, h) = (frame.width, frame.height);
    let mut dh = [0u64; 4];
    let mut ah = [0u64; 4];
    let mut dg = [0.0f64; 17 * 16];
    let mut ag = [0.0f64; 16 * 16];
    for row in 0..16 {
        let y0 = row * h / 16;
        let y1 = ((row + 1) * h / 16).max(y0 + 1).min(h);
        let mut sums17 = [0u64; 17];
        let mut sums16 = [0u64; 16];
        let mut counts17 = [0u64; 17];
        let mut counts16 = [0u64; 16];
        for y in y0..y1 {
            for x in 0..w {
                let (r, g, b) = rgb(frame, x, y);
                let lum = 299u64 * r as u64 + 587u64 * g as u64 + 114u64 * b as u64;
                let c17 = ((x + 1) * 17 - 1) / w;
                let c16 = ((x + 1) * 16 - 1) / w;
                sums17[c17] += lum;
                sums16[c16] += lum;
                counts17[c17] += 1;
                counts16[c16] += 1;
            }
        }
        for x in 0..17 { dg[row * 17 + x] = sums17[x] as f64 / counts17[x].max(1) as f64; }
        for x in 0..16 { ag[row * 16 + x] = sums16[x] as f64 / counts16[x].max(1) as f64; }
    }
    for row in 0..16 {
        for x in 0..16 {
            let bit = row * 16 + x;
            if dg[row * 17 + x] > dg[row * 17 + x + 1] { dh[bit >> 6] |= 1 << (bit & 63); }
        }
    }
    let mean = ag.iter().sum::<f64>() / ag.len() as f64;
    for (bit, value) in ag.iter().enumerate() {
        if *value > mean { ah[bit >> 6] |= 1 << (bit & 63); }
    }
    let mut bytes = [0u8; 64];
    for i in 0..4 {
        bytes[i * 8..i * 8 + 8].copy_from_slice(&dh[i].to_be_bytes());
        bytes[(i + 4) * 8..(i + 5) * 8].copy_from_slice(&ah[i].to_be_bytes());
    }
    hex::encode(bytes)
}

// A region carries the same scalar paint evidence as the old Go crop path.
// Its box is clamped to the screenshot before sampling.
fn region_stats(frame: &Frame, x0: usize, y0: usize, x1: usize, y1: usize, spec: &Value) -> Value {
    let (w, h) = (x1 - x0, y1 - y0);
    let mut step = 1usize;
    while (w / step) * (h / step) > 1_500_000 { step += 1; }
    let mut bands = [0usize; 32];
    let (mut count, mut sum, mut sum_sq) = (0usize, 0.0, 0.0);
    for y in (y0..y1).step_by(step) {
        for x in (x0..x1).step_by(step) {
            let (r, g, b) = rgb(frame, x, y);
            let v = luma(r, g, b);
            bands[(v as usize) >> 3] += 1;
            sum += v;
            sum_sq += v * v;
            count += 1;
        }
    }
    let mean = sum / count as f64;
    let variance = (sum_sq / count as f64 - mean * mean).max(0.0);
    let dominant = bands.iter().copied().max().unwrap_or(0) as f64 / count as f64;
    let distinct = bands.iter().filter(|&&n| n as f64 > 0.001 * count as f64).count();
    let cell_px = 32usize;
    let mut photos = 0usize;
    for y in (y0..y1).step_by(cell_px) {
        for x in (x0..x1).step_by(cell_px) {
            if photo_cell(frame, x, y, (x + cell_px).min(x1), (y + cell_px).min(y1), step.max(2), spec) {
                photos += 1;
            }
        }
    }
    json!({"width": w, "height": h, "luma_variance": variance,
        "dominant_luma_frac": dominant, "distinct_luma_bands": distinct,
        "photo_like_cells": photos,
        "painted": variance >= 8.0 && distinct >= 4 && dominant < 0.98})
}

// Return 25 short, lossy glyph signatures under ±2px translation. A signature
// cannot reconstruct the marker crop, but lets sibling brands compare paint.
fn marker_hashes(frame: &Frame, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<String> {
    let (w, h) = (x1 - x0, y1 - y0);
    let mut hashes = Vec::with_capacity(25);
    for dy in -2isize..=2 {
        for dx in -2isize..=2 {
            let mut values = [255.0f64; 9 * 8];
            for row in 0..8 {
                for col in 0..9 {
                    let sx = x0 as isize + (((col * 2 + 1) * w) / 18) as isize + dx;
                    let sy = y0 as isize + (((row * 2 + 1) * h) / 16) as isize + dy;
                    if sx >= 0 && sy >= 0 && sx < frame.width as isize && sy < frame.height as isize {
                        let (r, g, b) = rgb(frame, sx as usize, sy as usize);
                        values[row * 9 + col] = luma(r, g, b);
                    }
                }
            }
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let (mut difference, mut darkness) = (0u64, 0u64);
            for row in 0..8 {
                for col in 0..8 {
                    let bit = row * 8 + col;
                    if values[row * 9 + col] > values[row * 9 + col + 1] { difference |= 1 << bit; }
                    if values[row * 9 + col] < mean { darkness |= 1 << bit; }
                }
            }
            hashes.push(format!("{difference:016x}{darkness:016x}"));
        }
    }
    hashes
}

/// The request's inspection spec controls sampling; output is scalar evidence
/// plus a compact photo-cell mask, never image bytes or a thumbnail.
pub fn analyze(frame: &Frame, spec: &Value) -> Value {
    let (w, h) = (frame.width, frame.height);
    if w == 0 || h == 0 { return json!({"width": w, "height": h}); }
    let mut step = 1usize;
    while (w / step) * (h / step) > 1_500_000 { step += 1; }
    let mut bands = [0usize; 32];
    let (mut count, mut sum, mut sum_sq) = (0usize, 0.0, 0.0);
    for y in (0..h).step_by(step) {
        for x in (0..w).step_by(step) {
            let (r, g, b) = rgb(frame, x, y);
            let value = luma(r, g, b);
            bands[(value as usize) >> 3] += 1;
            sum += value;
            sum_sq += value * value;
            count += 1;
        }
    }
    let mean = sum / count as f64;
    let variance = (sum_sq / count as f64 - mean * mean).max(0.0);
    let dominant = bands.iter().copied().max().unwrap_or(0) as f64 / count as f64;
    let distinct = bands.iter().filter(|&&n| n as f64 > 0.001 * count as f64).count();
    let cell_px = (number(spec, "photo_cell_px", 48.0) as usize).max(8);
    let cell_step = step.max(2);
    let mut mask = Vec::new();
    for y in (0..h).step_by(cell_px) {
        for x in (0..w).step_by(cell_px) {
            mask.push(photo_cell(frame, x, y, (x + cell_px).min(w), (y + cell_px).min(h), cell_step, spec));
        }
    }
    let photo_cells = mask.iter().filter(|&&b| b).count();
    let mut regions = serde_json::Map::new();
    if let Some(requested) = spec.get("resolved_regions").and_then(Value::as_array) {
        for requested in requested {
            let Some(name) = requested.get("name").and_then(Value::as_str) else { continue };
            let Some(rect) = requested.get("rect") else { continue };
            let get = |field: &str| rect.get(field).and_then(Value::as_f64).unwrap_or(0.0);
            let x0 = get("x").floor().max(0.0).min(w as f64) as usize;
            let y0 = get("y").floor().max(0.0).min(h as f64) as usize;
            let x1 = (get("x") + get("width")).ceil().max(0.0).min(w as f64) as usize;
            let y1 = (get("y") + get("height")).ceil().max(0.0).min(h as f64) as usize;
            if x1 <= x0 || y1 <= y0 {
                regions.insert(name.to_string(), json!({"error": "region has no painted box"}));
                continue;
            }
            let region = if requested.get("kind").and_then(Value::as_str) == Some("marker") {
                json!({"width": x1 - x0, "height": y1 - y0,
                    "hashes": marker_hashes(frame, x0, y0, x1, y1)})
            } else {
                region_stats(frame, x0, y0, x1, y1, spec)
            };
            regions.insert(name.to_string(), region);
        }
    }
    json!({"width": w, "height": h, "luma_mean": mean, "luma_variance": variance,
        "dominant_luma_frac": dominant, "distinct_luma_bands": distinct,
        "photo_like_cells": photo_cells, "grid_cells": mask.len(),
        "photo_cell_px": cell_px, "photo_cell_grid": mask,
        "gallery_hash": gallery_hash(frame), "regions": regions})
}
