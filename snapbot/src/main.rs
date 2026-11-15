//! route66-snapbot: one binary for the Lambda handler, the local Kumo pool, its
//! in-process image writer (GH #4115; the Node coordinator is burned).
//!
//!   snapbot                  Lambda bootstrap: Runtime API loop, handler = $_HANDLER
//!   snapbot kumo-runtime     local pool dispatcher (SNAPBOT_POOL_PROCESSES lanes)
//!   snapbot kumo-lane        one pool lane (started by kumo-runtime)
//!   snapbot bench <url> <dir> OCR cost measurements (Dockerfile.test only)
//!   snapbot --version
//!   snapbot --type=...       a CEF child process (renderer, GPU, utility)

mod actions;
mod attest;
mod awsres;
mod bench;
mod browse;
mod browser;
mod capture;
mod cdp;
mod cefhost;
mod ci;
mod config;
mod github;
mod handler;
mod httpraw;
mod js;
mod keys;
mod kumo;
mod ocr;
mod pngenc;
mod runtime;
mod shm;
mod inspect;
mod store;
mod writer;

use anyhow::Result;

fn handler_name() -> String {
    std::env::var("_HANDLER").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "index.handler".to_string())
}

/// Startup validation, as the Node handler failed at module load: the handler
/// configuration, except for fetch-hop which needs none.
fn init(handler: &str) -> Result<()> {
    if !handler.starts_with("fetch-hop") {
        config::cfg()?;
    }
    Ok(())
}

async fn run() -> Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let handler = handler_name();
    match arg.as_str() {
        "--version" => {
            println!("snapbot {} ({})", env!("CARGO_PKG_VERSION"), ocr::WORKER_VERSION);
            Ok(())
        }
        "kumo-runtime" => {
            init(&handler)?;
            kumo::run_pool().await
        }
        "kumo-lane" => {
            init(&handler)?;
            kumo::run_lane(handler).await
        }
        // Test-image only: the OCR cost measurements on a tall page.
        "bench" => {
            let a: Vec<String> = std::env::args().skip(2).collect();
            anyhow::ensure!(a.len() == 2, "usage: snapbot bench <url> <best-tessdata-dir>");
            bench::run(&a[0], &a[1]).await
        }
        "" | "index.handler" | "fetch-hop.handler" => {
            let h = if arg.is_empty() { handler } else { arg };
            init(&h)?;
            // WHY (route66 GH #4471): the browser launch and first spare context
            // happen in init, so a fresh sandbox's first request starts warm.
            if !h.starts_with("fetch-hop") {
                spawn_sampler();
                let args: serde_json::Value = match std::env::var("SNAPBOT_BROWSER_ARGS") {
                    Ok(v) if !v.is_empty() => serde_json::from_str(&v)?,
                    _ => serde_json::json!([]),
                };
                browse::warm(&args).await?;
            }
            runtime::run(h).await
        }
        other => anyhow::bail!("unknown command {other:?}"),
    }
}

/// WHY (route66 GH #4471, diagnostic): a fresh Lambda sandbox's browser freezes
/// 2.5-6 s about 1 s into each of its first two pages, all threads silent in
/// the log; pre-touching 2 GB of memory (e626f26, 0.6-1 s) did not change it.
/// It showed one browser thread at 100% for 4-6 s: SwiftShader's pipeline JIT
/// (see cefhost base_switches). Opt-in now (SNAPBOT_SAMPLE_SECS, default 0).
/// For SNAPBOT_SAMPLE_SECS after start this logs, every 500 ms,
/// the sandbox's iowait/steal ticks and its busiest threads (comm, state, CPU
/// ticks in the window) across every process, so the frozen window shows what
/// is running, blocked in D state, or descheduled.
fn spawn_sampler() {
    let secs: u64 = std::env::var("SNAPBOT_SAMPLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if secs == 0 {
        return;
    }
    std::thread::spawn(move || {
        use std::collections::HashMap;
        let started = std::time::Instant::now();
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        // /proc/stat cpu line: user nice system idle iowait irq softirq steal
        let cpu = || -> Vec<u64> { read("/proc/stat").lines().next().unwrap_or("").split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect() };
        let threads = || -> HashMap<String, (String, String, u64)> {
            let mut out = HashMap::new();
            for p in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
                let pid = p.file_name().to_string_lossy().to_string();
                if !pid.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                for t in std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten() {
                    let tid = t.file_name().to_string_lossy().to_string();
                    let stat = read(&format!("/proc/{pid}/task/{tid}/stat"));
                    // comm is parenthesised and may hold spaces; fields resume after ')'.
                    let (Some(l), Some(r)) = (stat.find('('), stat.rfind(')')) else { continue };
                    let comm = stat[l + 1..r].to_string();
                    let f: Vec<&str> = stat.get(r + 2..).unwrap_or("").split_whitespace().collect();
                    let ticks = f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                    out.insert(format!("{pid}/{tid}"), (comm, f.first().unwrap_or(&"?").to_string(), ticks));
                }
            }
            out
        };
        let mut last_cpu = cpu();
        let mut last = threads();
        while started.elapsed().as_secs() < secs {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let c = cpu();
            let d = |i: usize| c.get(i).unwrap_or(&0).saturating_sub(*last_cpu.get(i).unwrap_or(&0));
            let now = threads();
            let mut busy: Vec<(u64, String)> = now
                .iter()
                .map(|(k, (comm, st, t))| (t.saturating_sub(last.get(k).map(|x| x.2).unwrap_or(0)), format!("{comm}[{k}]{st}")))
                .filter(|(dt, s)| *dt > 0 || s.ends_with('D'))
                .collect();
            busy.sort_by(|a, b| b.0.cmp(&a.0));
            let top: Vec<String> = busy.iter().take(5).map(|(dt, s)| format!("{s}:{dt}")).collect();
            println!(
                "[sample +{}ms] user={} sys={} idle={} iowait={} steal={} top={}",
                started.elapsed().as_millis(), d(0) + d(1), d(2), d(3), d(4), d(7), top.join(" ")
            );
            last_cpu = c;
            last = now;
        }
    });
}

/// Commands that drive pages initialize CEF on the main thread; the rest never
/// load Chromium at all.
fn needs_chromium() -> bool {
    match std::env::args().nth(1).unwrap_or_default().as_str() {
        "kumo-lane" | "bench" | "index.handler" => true,
        "" => !handler_name().starts_with("fetch-hop"),
        _ => false,
    }
}

fn main() {
    // A CEF child re-executes this binary with --type=...; it runs nothing else.
    if let Some(code) = cefhost::run_subprocess_if_child() {
        std::process::exit(code);
    }
    // Screenshot writes run inside each lane; there is no writer subprocess.
    let tokio_main = || -> i32 {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
        match rt.block_on(run()) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("snapbot fatal: {e:#}");
                1
            }
        }
    };
    if !needs_chromium() {
        std::process::exit(tokio_main());
    }
    // CEF's UI loop owns the main thread; the handler runs on a tokio thread and
    // ends the loop when it is done.
    cefhost::initialize_or_exit();
    let code = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
    let c = code.clone();
    std::thread::Builder::new()
        .name("snapbot-tokio".into())
        .spawn(move || {
            c.store(tokio_main(), std::sync::atomic::Ordering::SeqCst);
            cefhost::quit();
        })
        .expect("spawn tokio thread");
    cefhost::run_until_quit();
    std::process::exit(code.load(std::sync::atomic::Ordering::SeqCst));
}
