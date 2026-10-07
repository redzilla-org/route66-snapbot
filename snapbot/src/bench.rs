//! `snapbot bench <url> <best-tessdata-dir>`: per-read CPU cost of in-lane OCR
//! on a tall page (GH #4115).
//!
//! WHY: route66 run660 showed the pool is CPU-bound (40 -> 80 lanes made each
//! request ~50% slower), and its slowest request is a full-page screenshot + OCR
//! of a tall listing page. This measures, on one capture of such a page, what a
//! downscaled first read, region crops and tessdata fast-vs-best each cost, and
//! the text best reads that fast does not. Run by Dockerfile.test only; the
//! keywords and regions below stand in for the caller's page-side JavaScript.

use crate::browser::{Browser, LaunchOptions};
use crate::ocr;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use snapbot_ocr::{Engine, Gray, Pass, Rect};
use std::collections::HashMap;
use std::time::Instant;

/// The listing facts a caller would expect on the page (stop_when input).
const KEYWORDS_FN: &str = "() => ['Tiverton', 'Carmel Valley', '1,099,000', 'Townhouse', 'Cambria', 'Listing Source', \
    'Monthly Payment Calculator', 'Ocean Distance', 'HOA Fee', 'Year Built', 'Square Footage', 'Public Records']";

/// Where the text is on that layout: the header band, the facts column and
/// the footer (the caller's regions_fn).
const REGIONS_FN: &str = "() => { const h = document.documentElement.scrollHeight; \
    return [{x: 0, y: 0, width: 1366, height: 330}, {x: 630, y: 330, width: 650, height: h - 660}, {x: 0, y: h - 330, width: 1366, height: 330}]; }";

fn pass(scale: f64) -> Pass {
    Pass { psm: 3, lang: "eng".into(), dpi: 300, scale, pixel_budget: 40_000_000 }
}

fn words(text: &str) -> HashMap<String, i64> {
    let mut m = HashMap::new();
    for w in text.split(|c: char| !c.is_alphanumeric() && c != '$' && c != ',' && c != '.').map(|w| w.trim_matches(|c| c == ',' || c == '.')) {
        if w.chars().any(char::is_alphanumeric) {
            *m.entry(w.to_lowercase()).or_insert(0) += 1;
        }
    }
    m
}

/// Multiset word diff: (only in a, only in b, common) counts plus samples.
fn diff(a: &str, b: &str) -> Value {
    let (wa, wb) = (words(a), words(b));
    let mut only_a = Vec::new();
    let mut only_b = Vec::new();
    let mut common = 0i64;
    for (w, &n) in &wa {
        let m = *wb.get(w).unwrap_or(&0);
        common += n.min(m);
        if n > m {
            only_a.push((w.clone(), n - m));
        }
    }
    for (w, &n) in &wb {
        let m = *wa.get(w).unwrap_or(&0);
        if n > m {
            only_b.push((w.clone(), n - m));
        }
    }
    only_a.sort();
    only_b.sort();
    let total = |v: &Vec<(String, i64)>| v.iter().map(|x| x.1).sum::<i64>();
    let sample = |v: &Vec<(String, i64)>| v.iter().take(60).map(|x| x.0.clone()).collect::<Vec<_>>();
    json!({"common_words": common, "only_best_words": total(&only_a), "only_fast_words": total(&only_b),
           "only_best_sample": sample(&only_a), "only_fast_sample": sample(&only_b)})
}

fn read(engine: &mut Engine, g: &Gray, label: &str, p: &Pass, regions: &[Rect], keywords: &[String]) -> Result<String> {
    let (t, cpu) = (Instant::now(), thread_cpu_ms());
    let (text, timing) = engine.read(g, p, regions).map_err(|e| anyhow!("{label}: {e}"))?;
    let (wall, cpu_ms) = (t.elapsed().as_millis() as u64, thread_cpu_ms() - cpu);
    let (hit, f) = ocr::keyword_match(&text, keywords);
    println!(
        "BENCH {}",
        json!({"read": label, "upscale": p.scale, "regions": regions.len(), "pixels": timing.pixels, "resample_ms": timing.resample_ms,
               "ocr_ms": timing.ocr_ms, "wall_ms": wall, "cpu_ms": cpu_ms, "chars": text.len(), "keyword_fraction": f, "matched": hit.len()})
    );
    Ok(text)
}

/// This thread's CPU time in ms, so each row's cost is its own CPU.
fn thread_cpu_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes only the timespec it is handed.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

/// This whole process's CPU time in ms (CEF's UI/compositor threads included),
/// for the capture rows, whose work happens on threads other than the caller's.
fn process_cpu_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: as above.
    unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

pub async fn run(url: &str, best_dir: &str) -> Result<()> {
    let b = Browser::launch(LaunchOptions { single_process: false, ignore_https_errors: true, extra_args: crate::cefhost::launch_args().to_vec() }).await?;
    let r = async {
        let page = b.new_page(None).await?;
        page.set_viewport(1366, 900).await?;
        page.goto(url, "load", 30000).await?;
        let keywords: Vec<String> = serde_json::from_value(page.evaluate(&format!("({KEYWORDS_FN})()")).await?.unwrap_or_default())?;
        let regions = ocr::parse_rects(&page.evaluate(&format!("({REGIONS_FN})()")).await?.unwrap_or_default(), "regions")?;

        // Capture and convert, three times each: they are the fixed cost of every
        // read. The PNG encode is timed too, though it runs in the writer process.
        let mut frame = None;
        for i in 0..3 {
            let (t, cpu) = (Instant::now(), process_cpu_ms());
            let shot = page.screenshot(true).await?;
            let (capture_ms, capture_cpu_ms) = (t.elapsed().as_millis() as u64, process_cpu_ms() - cpu);
            let (t, cpu) = (Instant::now(), thread_cpu_ms());
            let g = snapbot_ocr::from_bgra(shot.frame.seg.as_slice(), shot.frame.width, shot.frame.height, shot.frame.stride).map_err(|e| anyhow!(e))?;
            let (convert_ms, convert_cpu_ms) = (t.elapsed().as_millis() as u64, thread_cpu_ms() - cpu);
            let t = Instant::now();
            let png = crate::pngenc::encode(&shot.frame)?;
            println!(
                "BENCH {}",
                json!({"run": i, "capture_ms": capture_ms, "capture_process_cpu_ms": capture_cpu_ms, "tiles": shot.tiles,
                       "convert_ms": convert_ms, "convert_cpu_ms": convert_cpu_ms, "writer_encode_ms": t.elapsed().as_millis() as u64,
                       "png_bytes": png.len(), "width": g.w, "height": g.h})
            );
            frame = Some(shot.frame);
        }
        let f = frame.ok_or_else(|| anyhow!("no capture"))?;
        let g = snapbot_ocr::from_bgra(f.seg.as_slice(), f.width, f.height, f.stride).map_err(|e| anyhow!(e))?;
        println!("BENCH {}", json!({"cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)}));

        let mut fast = Engine::new();
        let mut best = Engine::with_datapath(best_dir);
        // Warm both models so no row pays a model load.
        let tiny = Gray { w: 64, h: 32, px: vec![255; 64 * 32] };
        fast.read(&tiny, &pass(1.0), &[]).map_err(|e| anyhow!(e))?;
        best.read(&tiny, &pass(1.0), &[]).map_err(|e| anyhow!(e))?;

        let fast_full = read(&mut fast, &g, "fast full", &pass(1.0), &[], &keywords)?;
        read(&mut fast, &g, "fast full", &pass(1.0), &[], &keywords)?;
        read(&mut fast, &g, "fast full", &pass(0.75), &[], &keywords)?;
        read(&mut fast, &g, "fast full", &pass(0.5), &[], &keywords)?;
        read(&mut fast, &g, "fast regions", &pass(1.0), &regions, &keywords)?;
        read(&mut fast, &g, "fast regions", &pass(0.75), &regions, &keywords)?;
        read(&mut fast, &g, "fast regions", &pass(0.5), &regions, &keywords)?;
        read(&mut fast, &g, "fast full", &pass(2.0), &[], &keywords)?;
        let best_full = read(&mut best, &g, "best full", &pass(1.0), &[], &keywords)?;
        read(&mut best, &g, "best regions", &pass(1.0), &regions, &keywords)?;
        println!("BENCH {}", json!({"diff": "best full x1 vs fast full x1", "result": diff(&best_full, &fast_full)}));
        println!("BENCH-TEXT-FAST {}", json!(fast_full));
        println!("BENCH-TEXT-BEST {}", json!(best_full));

        // The request path: a downscaled first read, escalating only when unmet.
        for spec in [
            json!({"passes": [{"upscale": 0.5}], "fallback": [{"upscale": 1}], "stop_when": {"keywords_fn": KEYWORDS_FN, "min_fraction": 0.8}}),
            json!({"passes": [{"upscale": 0.75}], "fallback": [{"upscale": 1}], "stop_when": {"keywords_fn": KEYWORDS_FN, "min_fraction": 0.8}}),
            json!({"passes": [{"upscale": 0.5}], "fallback": [{"upscale": 1}], "stop_when": {"keywords_fn": KEYWORDS_FN, "min_fraction": 1.0}}),
            json!({"passes": [{"upscale": 0.75}], "fallback": [{"upscale": 1}], "regions": REGIONS_FN, "stop_when": {"keywords_fn": KEYWORDS_FN, "min_fraction": 0.8}}),
        ] {
            let (mut s, fns) = ocr::parse(&spec)?;
            if fns.regions_fn.is_some() {
                s.regions = regions.clone();
            }
            let t = Instant::now();
            let out = ocr::run_spec(&mut fast, &g, &s, Some(&keywords)).map_err(|e| anyhow!(e))?;
            let passes: Vec<Value> = out["passes"].as_array().cloned().unwrap_or_default().into_iter().map(|mut p| {
                p.as_object_mut().map(|o| o.retain(|k, _| ["wave", "index", "upscale", "ocr_ms", "fraction"].contains(&k.as_str())));
                p
            }).collect();
            println!(
                "BENCH {}",
                json!({"request": {"upscales": [spec["passes"][0]["upscale"], spec["fallback"][0]["upscale"]], "regions": s.regions.len(),
                        "min_fraction": s.min_fraction},
                       "won": out["won"], "met": out["met"], "fraction": out["fraction"], "passes": passes, "wall_ms": t.elapsed().as_millis() as u64})
            );
        }
        Ok(())
    }
    .await;
    b.close().await;
    r
}
