//! In-lane OCR of a captured screenshot (GH #4115).
//!
//! Owner 2026-09-29: "so keep OCR in the Chromium lane then", "also avoid image
//! transmission; do the OCR at capture point in snapbot", and "can you keep
//! domain knowledge out of snapbot". The screenshot step hands the PNG bytes
//! it just captured -- in memory, never re-encoded, never written to a temp
//! file -- to this process's one resident Tesseract engine. The spec is
//! generic: `passes` run first; `fallback` runs only when fewer than
//! `min_fraction` of the caller's keywords appear in the text so far. Text is
//! unioned in submission order, one read per line block, as route66's former
//! two-wave reader assembled it.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use snapbot_ocr::{Engine, Image, Pass, Rect};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::oneshot;

/// Reported in every result so a caller can prove which engine read the image.
pub const WORKER_VERSION: &str = concat!("snapbot-ocr/", env!("CARGO_PKG_VERSION"), " in-process");

pub struct Spec {
    pub passes: Vec<Pass>,
    pub fallback: Vec<Pass>,
    pub min_fraction: f64,
    /// Image rects to read (CSS px at device scale 1, i.e. image px); empty
    /// means the whole image.
    pub regions: Vec<Rect>,
}

/// The page-side inputs of a spec, evaluated at the capture instant.
pub struct PageFns {
    pub keywords_fn: Option<String>,
    pub regions_fn: Option<String>,
}

/// Parse a rect list: [{x, y, width, height}, ...] with finite numbers.
pub fn parse_rects(v: &Value, what: &str) -> Result<Vec<Rect>> {
    let arr = v.as_array().ok_or_else(|| anyhow!("{what} must be an array of {{x, y, width, height}}"))?;
    arr.iter()
        .enumerate()
        .map(|(i, r)| {
            let n = |k: &str| {
                r.get(k).and_then(Value::as_f64).filter(|f| f.is_finite()).ok_or_else(|| anyhow!("{what}[{i}].{k} must be a finite number"))
            };
            // Fractional CSS pixels widen to the pixels they touch.
            let (x, y, w, h) = (n("x")?, n("y")?, n("width")?, n("height")?);
            let (x0, y0) = (x.floor(), y.floor());
            Ok(Rect { x: x0 as i64, y: y0 as i64, width: ((x + w).ceil() - x0) as i64, height: ((y + h).ceil() - y0) as i64 })
        })
        .collect()
}

fn parse_pass(v: &Value, what: &str) -> Result<Pass> {
    let o = v.as_object().ok_or_else(|| anyhow!("{what} must be an object"))?;
    let num = |k: &str| -> Result<u64> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(0),
            Some(x) => x
                .as_f64()
                .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                .map(|f| f as u64)
                .ok_or_else(|| anyhow!("{what}.{k} must be a non-negative integer")),
        }
    };
    let lang = match o.get("lang") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => bail!("{what}.lang must be a string"),
    };
    Ok(Pass { psm: num("psm")? as u32, lang, dpi: num("dpi")? as u32, upscale: num("upscale")? as u32, pixel_budget: num("pixel_budget")? }
        .normalized())
}

fn parse_passes(v: Option<&Value>, what: &str, required: bool) -> Result<Vec<Pass>> {
    match v {
        None | Some(Value::Null) if !required => Ok(Vec::new()),
        Some(Value::Array(a)) if !a.is_empty() || !required => {
            a.iter().enumerate().map(|(i, p)| parse_pass(p, &format!("{what}[{i}]"))).collect()
        }
        _ => bail!("{what} must be a non-empty array of passes"),
    }
}

/// Parse a step's `ocr` field into the spec and its page-side functions.
pub fn parse(v: &Value) -> Result<(Spec, PageFns)> {
    let o = v.as_object().ok_or_else(|| anyhow!("ocr must be an object"))?;
    let passes = parse_passes(o.get("passes"), "ocr.passes", true)?;
    let fallback = parse_passes(o.get("fallback"), "ocr.fallback", false)?;
    let mut min_fraction = 1.0;
    let mut keywords_fn = None;
    let mut regions = Vec::new();
    let mut regions_fn = None;
    match o.get("regions") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => regions_fn = Some(s.clone()),
        Some(v) => regions = parse_rects(v, "ocr.regions")?,
    }
    if let Some(sw) = o.get("stop_when").filter(|v| !v.is_null()) {
        let sw = sw.as_object().ok_or_else(|| anyhow!("ocr.stop_when must be an object"))?;
        match sw.get("keywords_fn") {
            Some(Value::String(s)) => keywords_fn = Some(s.clone()),
            _ => bail!("ocr.stop_when.keywords_fn must be a JavaScript function source"),
        }
        if let Some(m) = sw.get("min_fraction").filter(|v| !v.is_null()) {
            min_fraction = m
                .as_f64()
                .filter(|f| (0.0..=1.0).contains(f))
                .ok_or_else(|| anyhow!("ocr.stop_when.min_fraction must be a number in [0, 1]"))?;
        }
    }
    Ok((Spec { passes, fallback, min_fraction, regions }, PageFns { keywords_fn, regions_fn }))
}

/// Lowercase and collapse whitespace runs: the generic match normalization.
pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The fraction of keywords present in `text` (case-insensitive, whitespace-
/// collapsed substring). Keywords that normalize to empty are not counted.
pub fn keyword_fraction(text: &str, keywords: &[String]) -> Option<f64> {
    let hay = normalize(text);
    let kws: Vec<String> = keywords.iter().map(|k| normalize(k)).filter(|k| !k.is_empty()).collect();
    if kws.is_empty() {
        return None;
    }
    let hit = kws.iter().filter(|k| hay.contains(k.as_str())).count();
    Some(hit as f64 / kws.len() as f64)
}

struct Job {
    png: Arc<Vec<u8>>,
    spec: Spec,
    keywords: Option<Vec<String>>,
    reply: oneshot::Sender<Value>,
}

static ENGINE: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();

/// The process's one engine thread, started on first use. It owns the
/// Tesseract instance, so no C handle ever crosses a thread.
fn engine() -> mpsc::Sender<Job> {
    ENGINE
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("snapbot-ocr".to_string())
                .spawn(move || {
                    let mut engine = Engine::new();
                    for job in rx {
                        let out = run_job(&mut engine, &job.png, &job.spec, job.keywords.as_deref());
                        let _ = job.reply.send(out);
                    }
                })
                .expect("spawn OCR engine thread");
            Mutex::new(tx)
        })
        .lock()
        .unwrap()
        .clone()
}

fn pass_json(wave: u32, p: &Pass, t: &snapbot_ocr::PassTiming) -> Value {
    json!({"wave": wave, "psm": p.psm, "lang": p.lang, "dpi": p.dpi, "upscale": p.upscale, "pixel_budget": p.pixel_budget,
           "scale": t.scale, "decode_ms": t.decode_ms, "preprocess_ms": t.preprocess_ms, "ocr_ms": t.ocr_ms,
           "region_ms": t.region_ms})
}

fn run_job(engine: &mut Engine, png: &[u8], spec: &Spec, keywords: Option<&[String]>) -> Value {
    let started = Instant::now();
    let mut img = Image::new(png);
    let mut text = String::new();
    let mut passes = Vec::new();
    let (mut decode, mut pre, mut ocr) = (0u64, 0u64, 0u64);
    let mut waves_run = 0u32;
    let mut error = String::new();
    let met = |t: &str| keywords.and_then(|k| keyword_fraction(t, k)).map(|f| f >= spec.min_fraction);
    'waves: for (wave, list) in [(1u32, &spec.passes), (2u32, &spec.fallback)] {
        if list.is_empty() {
            continue;
        }
        // The fallback wave runs only when keywords were supplied and are unmet.
        if wave == 2 && met(&text) != Some(false) {
            break;
        }
        waves_run = wave;
        for p in list.iter() {
            match engine.read(&mut img, p, &spec.regions) {
                Ok((t, timing)) => {
                    decode += timing.decode_ms;
                    pre += timing.preprocess_ms;
                    ocr += timing.ocr_ms;
                    passes.push(pass_json(wave, p, &timing));
                    text.push_str(&t);
                    text.push('\n');
                }
                Err(e) => {
                    error = e;
                    break 'waves;
                }
            }
            // Stop as soon as the keyword floor is met: the union is monotonic,
            // so the reads skipped could not have changed the answer.
            if met(&text) == Some(true) {
                break 'waves;
            }
        }
    }
    let fraction = keywords.and_then(|k| keyword_fraction(&text, k));
    json!({
        "text": text,
        "error": error,
        "waves_run": waves_run,
        "keyword_fraction": fraction,
        "met": fraction.map(|f| f >= spec.min_fraction),
        "min_fraction": spec.min_fraction,
        "passes": passes,
        "timings": {"decode_ms": decode, "preprocess_ms": pre, "ocr_ms": ocr, "total_ms": started.elapsed().as_millis() as u64},
        "metadata": {"worker_version": WORKER_VERSION},
    })
}

/// OCR `png` in this process's engine. The bytes are shared, not copied.
pub async fn read(png: Arc<Vec<u8>>, spec: Spec, keywords: Option<Vec<String>>) -> Result<Value> {
    let (reply, rx) = oneshot::channel();
    engine().send(Job { png, spec, keywords, reply }).map_err(|_| anyhow!("OCR engine thread is gone"))?;
    rx.await.map_err(|_| anyhow!("OCR engine thread died"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_fraction_is_case_and_whitespace_insensitive() {
        // WHY: the fallback wave keys on this fraction; a miss here re-reads
        // every screenshot at three times the cost.
        let kws = vec!["Save  and\nContinue".to_string(), "Missing".to_string(), "  ".to_string()];
        assert_eq!(keyword_fraction("header\nSAVE AND   continue footer", &kws), Some(0.5));
        assert_eq!(keyword_fraction("anything", &[]), None);
    }

    #[test]
    fn spec_requires_passes_and_a_bounded_fraction() {
        assert!(parse(&json!({"passes": []})).is_err());
        assert!(parse(&json!({"passes": [{}], "stop_when": {"keywords_fn": "() => []", "min_fraction": 2}})).is_err());
        let (spec, f) = parse(&json!({"passes": [{"psm": 3}], "fallback": [{"psm": 6, "upscale": 3}],
                                      "stop_when": {"keywords_fn": "() => []", "min_fraction": 0.7}}))
        .unwrap();
        assert_eq!(spec.passes[0].upscale, 1);
        assert_eq!(spec.fallback[0].pixel_budget, 20_000_000);
        assert_eq!(f.keywords_fn.as_deref(), Some("() => []"));
        assert!(spec.regions.is_empty());
    }

    #[test]
    fn fractional_rects_widen_to_whole_pixels() {
        let r = parse_rects(&json!([{"x": 10.5, "y": 2.2, "width": 5.0, "height": 1.0}]), "r").unwrap();
        assert_eq!((r[0].x, r[0].y, r[0].width, r[0].height), (10, 2, 6, 2));
    }
}
