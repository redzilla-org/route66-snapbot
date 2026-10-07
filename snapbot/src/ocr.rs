//! In-lane OCR of a captured screenshot (GH #4115).
//!
//! Owner 2026-09-29: "so keep OCR in the Chromium lane then", "also avoid image
//! transmission; do the OCR at capture point in snapbot", and "can you keep
//! domain knowledge out of snapbot". The screenshot step hands the PNG bytes
//! CDP just returned to this process's one resident Tesseract engine, which
//! decodes them once in memory. The spec is generic: `passes` run in order and
//! stop as soon as `stop_when` is met; `fallback` runs only when it is still
//! unmet. Keywords and regions come from the caller's page-side JavaScript.
//!
//! REQUEST (a screenshot step's `ocr` field):
//!   passes:    [{psm, lang, dpi, upscale, pixel_budget}]  required, non-empty
//!   fallback:  [{...same...}]                             optional
//!   stop_when: {keywords_fn: "<JS fn source -> string[]>", min_fraction: 0..1}
//!   regions:   [{x, y, width, height}] | "<JS fn source -> rects>"   optional
//! `upscale` is the pass scale: (0, 1) downscales, 1 (default) reads the
//! captured pixels, a whole 2..4 upscales (bounded by pixel_budget).

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use snapbot_ocr::{Engine, Gray, Pass, Rect};
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
    let num = |k: &str| -> Result<f64> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(0.0),
            Some(x) => x.as_f64().filter(|f| f.is_finite() && *f >= 0.0).ok_or_else(|| anyhow!("{what}.{k} must be a non-negative number")),
        }
    };
    let int = |k: &str| -> Result<u64> {
        let f = num(k)?;
        if f.fract() != 0.0 {
            bail!("{what}.{k} must be an integer");
        }
        Ok(f as u64)
    };
    let lang = match o.get("lang") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => bail!("{what}.lang must be a string"),
    };
    let p = Pass { psm: int("psm")? as u32, lang, dpi: int("dpi")? as u32, scale: num("upscale")?, pixel_budget: int("pixel_budget")? }
        .normalized();
    p.validate().map_err(|e| anyhow!("{what}.upscale: {e}"))?;
    Ok(p)
}

fn parse_passes(v: Option<&Value>, what: &str, required: bool) -> Result<Vec<Pass>> {
    match v {
        None | Some(Value::Null) if !required => Ok(Vec::new()),
        Some(Value::Array(a)) if !a.is_empty() || !required => a.iter().enumerate().map(|(i, p)| parse_pass(p, &format!("{what}[{i}]"))).collect(),
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
            min_fraction =
                m.as_f64().filter(|f| (0.0..=1.0).contains(f)).ok_or_else(|| anyhow!("ocr.stop_when.min_fraction must be a number in [0, 1]"))?;
        }
    }
    if !fallback.is_empty() && keywords_fn.is_none() {
        bail!("ocr.fallback needs ocr.stop_when: without keywords it could never run");
    }
    Ok((Spec { passes, fallback, min_fraction, regions }, PageFns { keywords_fn, regions_fn }))
}

/// Lowercase and collapse whitespace runs: the generic match normalization.
pub fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// The keywords present in `text` (case-insensitive, whitespace-collapsed
/// substring) and their fraction of the keywords that normalize non-empty.
pub fn keyword_match(text: &str, keywords: &[String]) -> (Vec<String>, Option<f64>) {
    let hay = normalize(text);
    let kws: Vec<&String> = keywords.iter().filter(|k| !normalize(k).is_empty()).collect();
    if kws.is_empty() {
        return (Vec::new(), None);
    }
    let hit: Vec<String> = kws.iter().filter(|k| hay.contains(normalize(k).as_str())).map(|k| k.to_string()).collect();
    let f = hit.len() as f64 / kws.len() as f64;
    (hit, Some(f))
}

struct Job {
    png: Arc<Vec<u8>>,
    spec: Spec,
    keywords: Option<Vec<String>>,
    /// Who asked (screenshot name, page URL); only logged, never read by the engine.
    label: Value,
    reply: oneshot::Sender<Result<Value, String>>,
}

/// This thread's CPU time in ms (CLOCK_THREAD_CPUTIME_ID), so a read's cost is its own
/// CPU, not wall time inflated by other lanes.
fn thread_cpu_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes only the timespec it is handed.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1e6
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
                    // WHY (route66 GH #4082, owner 2026-10-07: "log the OCR timings;
                    // investigate the ones cosuming the most CPU"): each read reports its own
                    // thread CPU and wall time, its text size, and the engine's one-time
                    // setup, so the heaviest reads can be ranked. Measurement only.
                    let t = Instant::now();
                    let mut engine = Engine::new();
                    let mut engine_init_ms = Some(t.elapsed().as_millis() as u64);
                    for job in rx {
                        let (wall, cpu) = (Instant::now(), thread_cpu_ms());
                        let mut out = run_job(&mut engine, &job.png, &job.spec, job.keywords.as_deref());
                        let (wall_ms, cpu_ms) = (wall.elapsed().as_millis() as u64, (thread_cpu_ms() - cpu).round() as u64);
                        let init = engine_init_ms.take().unwrap_or(0);
                        if let Ok(v) = out.as_mut() {
                            let text_bytes = v["text"].as_str().map_or(0, str::len);
                            v["timings"]["cpu_ms"] = json!(cpu_ms);
                            v["timings"]["wall_ms"] = json!(wall_ms);
                            v["timings"]["engine_init_ms"] = json!(init);
                            v["timings"]["text_bytes"] = json!(text_bytes);
                            eprintln!("SNAPBOT-OCR {}", json!({
                                "label": job.label, "image": v["image"], "regions": job.spec.regions.len(),
                                "passes": v["passes"].as_array().map_or(0, Vec::len), "engine_init_ms": init,
                                "wall_ms": wall_ms, "cpu_ms": cpu_ms, "text_bytes": text_bytes,
                            }));
                        }
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

/// Run a spec over an already decoded plane. Public so the bench can drive
/// the exact request path with its own engines.
pub fn run_spec(engine: &mut Engine, g: &Gray, spec: &Spec, keywords: Option<&[String]>) -> Result<Value, String> {
    let mut text = String::new();
    let mut passes = Vec::new();
    let (mut resample, mut ocr) = (0u64, 0u64);
    let mut won = Value::Null;
    let met = |t: &str| keywords.and_then(|k| keyword_match(t, k).1).map(|f| f >= spec.min_fraction);
    'waves: for (wave, list) in [("passes", &spec.passes), ("fallback", &spec.fallback)] {
        // The fallback wave runs only when keywords were supplied and are unmet.
        if wave == "fallback" && met(&text) != Some(false) {
            break;
        }
        for (i, p) in list.iter().enumerate() {
            let (t, timing) = engine.read(g, p, &spec.regions)?;
            resample += timing.resample_ms;
            ocr += timing.ocr_ms;
            text.push_str(&t);
            text.push('\n');
            let f = keywords.and_then(|k| keyword_match(&text, k).1);
            passes.push(json!({"wave": wave, "index": i, "psm": p.psm, "lang": p.lang, "dpi": p.dpi, "upscale": p.scale,
                               "pixel_budget": p.pixel_budget, "pixels": timing.pixels, "resample_ms": timing.resample_ms,
                               "ocr_ms": timing.ocr_ms, "fraction": f}));
            // Stop as soon as the keyword floor is met: the union is monotonic,
            // so the reads skipped could not have changed the answer.
            if met(&text) == Some(true) {
                won = json!({"wave": wave, "index": i});
                break 'waves;
            }
        }
    }
    let (matched, fraction) = match keywords {
        Some(k) => {
            let (m, f) = keyword_match(&text, k);
            (json!(m), f)
        }
        None => (Value::Null, None),
    };
    Ok(json!({
        "text": text,
        "keywords": keywords,
        "matched": matched,
        "fraction": fraction,
        "min_fraction": spec.min_fraction,
        "met": fraction.map(|f| f >= spec.min_fraction),
        "won": won,
        "passes": passes,
        "engine": WORKER_VERSION,
        "timings": {"resample_ms": resample, "ocr_ms": ocr},
    }))
}

fn run_job(engine: &mut Engine, png: &[u8], spec: &Spec, keywords: Option<&[String]>) -> Result<Value, String> {
    let t = Instant::now();
    let g = snapbot_ocr::decode(png)?;
    let decode_ms = t.elapsed().as_millis() as u64;
    let mut out = run_spec(engine, &g, spec, keywords)?;
    out["timings"]["decode_ms"] = json!(decode_ms);
    out["timings"]["total_ms"] = json!(t.elapsed().as_millis() as u64);
    out["image"] = json!({"width": g.w, "height": g.h});
    Ok(out)
}

/// OCR `png` in this process's engine. The bytes are shared, not copied.
pub async fn read(png: Arc<Vec<u8>>, spec: Spec, keywords: Option<Vec<String>>, label: Value) -> Result<Value> {
    let (reply, rx) = oneshot::channel();
    engine().send(Job { png, spec, keywords, label, reply }).map_err(|_| anyhow!("OCR engine thread is gone"))?;
    rx.await.map_err(|_| anyhow!("OCR engine thread died"))?.map_err(|e| anyhow!("ocr: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_match_is_case_and_whitespace_insensitive() {
        // WHY: the fallback wave keys on this fraction; a miss here re-reads
        // every screenshot at a higher scale.
        let kws = vec!["Save  and\nContinue".to_string(), "Missing".to_string(), "  ".to_string()];
        let (hit, f) = keyword_match("header\nSAVE AND   continue footer", &kws);
        assert_eq!(f, Some(0.5));
        assert_eq!(hit, vec!["Save  and\nContinue".to_string()]);
        assert_eq!(keyword_match("anything", &[]).1, None);
    }

    #[test]
    fn spec_requires_passes_bounded_fraction_and_valid_scale() {
        assert!(parse(&json!({"passes": []})).is_err());
        assert!(parse(&json!({"passes": [{}], "stop_when": {"keywords_fn": "() => []", "min_fraction": 2}})).is_err());
        assert!(parse(&json!({"passes": [{"upscale": 1.5}]})).is_err());
        assert!(parse(&json!({"passes": [{}], "fallback": [{"upscale": 2}]})).is_err());
        let (spec, f) = parse(&json!({"passes": [{"psm": 3, "upscale": 0.5}], "fallback": [{"psm": 6, "upscale": 2}],
                                      "stop_when": {"keywords_fn": "() => []", "min_fraction": 0.7}}))
        .unwrap();
        assert_eq!(spec.passes[0].scale, 0.5);
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
