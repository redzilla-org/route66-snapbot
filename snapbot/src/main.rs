//! route66-snapbot: one binary for the Lambda handler, the local Kumo pool, its
//! PNG writer and its self-test (GH #4115; the Node coordinator is burned).
//!
//!   snapbot                  Lambda bootstrap: Runtime API loop, handler = $_HANDLER
//!   snapbot kumo-runtime     local pool dispatcher (SNAPBOT_POOL_PROCESSES lanes)
//!   snapbot kumo-lane        one pool lane (started by kumo-runtime)
//!   snapbot png-writer <s>   the background PNG encoder/storer (started by the
//!                            pool or a Lambda process; see writer.rs)
//!   snapbot probe            image build check: embedded Chromium renders a page,
//!                            the in-process engine OCRs the painted frame
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
        "probe" => probe().await,
        // Test-image only: the OCR cost measurements on a tall page.
        "bench" => {
            let a: Vec<String> = std::env::args().skip(2).collect();
            anyhow::ensure!(a.len() == 2, "usage: snapbot bench <url> <best-tessdata-dir>");
            bench::run(&a[0], &a[1]).await
        }
        "" | "index.handler" | "fetch-hop.handler" => {
            let h = if arg.is_empty() { handler } else { arg };
            init(&h)?;
            runtime::run(h).await
        }
        other => anyhow::bail!("unknown command {other:?}"),
    }
}

/// The image's own proof: the embedded Chromium renders, a viewport and a tiled
/// full-page capture paint, and the linked engine reads the text back from the
/// raw frame. The page is taller than one paint tile, with text at its top and
/// at its bottom, so a full-page capture that lost a tile cannot pass.
async fn probe() -> Result<()> {
    const TOP: &str = "SNAPBOT PROBE 4115";
    const BOTTOM: &str = "SNAPBOT TILE TAIL";
    let b = browser::Browser::launch(browser::LaunchOptions { single_process: false, ignore_https_errors: true, extra_args: cefhost::launch_args().to_vec() }).await?;
    let page = b.new_page(None).await?;
    page.set_viewport(1366, 900).await?;
    let html = format!(
        "data:text/html,<body style=\"margin:0\"><h1 style=\"font:64px sans-serif;margin:0\">{TOP}</h1><div style=\"height:9800px\"></div><h1 style=\"font:64px sans-serif;margin:0\">{BOTTOM}</h1></body>"
    );
    let r = async {
        page.goto(&html.replace(' ', "%20"), "load", 30000).await?;
        // The viewport first: the plain paint path, no emulation change.
        let (spec, _) = ocr::parse(&serde_json::json!({"passes": [{"psm": 3}]}))?;
        let view = page.screenshot(false).await?;
        anyhow::ensure!((view.frame.width, view.frame.height) == (1366, 900), "viewport shot is {}x{}", view.frame.width, view.frame.height);
        let bands = ink_bands(&view.frame);
        let out = ocr::read(std::sync::Arc::new(view.frame), spec, None, serde_json::json!({"name": "smoke-probe-view"})).await?;
        anyhow::ensure!(out["text"].as_str().unwrap_or("").contains(TOP), "viewport probe OCR read {} (dark pixels per 1000 rows {bands:?})", out["text"]);
        let (spec, _) = ocr::parse(&serde_json::json!({"passes": [{"psm": 3}]}))?;
        let shot = page.screenshot(true).await?;
        anyhow::ensure!(shot.tiles >= 2, "probe page did not need tiling ({}x{})", shot.frame.width, shot.frame.height);
        let bands = ink_bands(&shot.frame);
        let frame = std::sync::Arc::new(shot.frame);
        let out = ocr::read(frame.clone(), spec, None, serde_json::json!({"name": "smoke-probe"})).await?;
        let text = out["text"].as_str().unwrap_or("").to_string();
        anyhow::ensure!(
            text.contains(TOP) && text.contains(BOTTOM),
            "probe OCR read {text:?}, expected {TOP:?} and {BOTTOM:?} (dark pixels per 1000 rows {bands:?}): {out}"
        );
        // WHY (route66 GH #4082): lv 20261008T005048Z full-page shots painted only
        // the first ~900 rows (search-result, compass privacy: white bands where
        // tiles had not rastered yet). A page of solid colored blocks proves every
        // row of every tile is painted, not just its text.
        let blocks = 140;
        let mut body = String::from("<body style=\"margin:0\">");
        for i in 0..blocks {
            // Software raster of blurred gradients is slow, as photo-heavy
            // listing pages are: the frame races raster unless draws wait for it.
            let c = format!("rgb({},{},{})", 40 + (i * 7) % 150, 30 + (i * 13) % 150, 20 + (i * 29) % 150);
            body.push_str(&format!(
                "<div style=\"height:100px;background:repeating-radial-gradient(circle,{c} 0 3px,rgb(20,20,20) 3px 5px);filter:blur(1px) saturate(2);box-shadow:0 0 40px {c}\"></div>"
            ));
        }
        page.goto(&format!("data:text/html,{}</body>", body.replace(' ', "%20").replace('#', "%23")), "load", 30000).await?;
        // The lv defect showed under 40-lane load, not idle: oversubscribe the
        // CPUs while the shot paints so raster is as late as it was there.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let burners: Vec<_> = (0..4 * std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
            .map(|_| {
                let s = stop.clone();
                std::thread::spawn(move || {
                    while !s.load(std::sync::atomic::Ordering::Relaxed) {
                        std::hint::spin_loop();
                    }
                })
            })
            .collect();
        let shot = page.screenshot(true).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        burners.into_iter().for_each(|b| b.join().unwrap());
        let shot = shot?;
        let f = &shot.frame;
        let px = f.seg.as_slice();
        let white: Vec<usize> = (0..blocks)
            .filter(|i| {
                let o = (i * 100 + 50) * f.stride + 683 * 4;
                px[o] as u32 + px[o + 1] as u32 + px[o + 2] as u32 > 700
            })
            .collect();
        anyhow::ensure!(white.is_empty(), "full-page {}x{} in {} tiles left {} of {blocks} blocks unpainted (white): {:?}", f.width, f.height, shot.tiles, white.len(), white);
        // The PNG the writer and the evidence path store: same pixels, encoded once.
        let png = pngenc::encode(&frame)?;
        println!(
            "snapbot probe: CEF painted {}x{} in {} tiles, OCR read it back, png {} bytes; timings {}",
            frame.width,
            frame.height,
            shot.tiles,
            png.len(),
            out["timings"]
        );
        Ok(())
    }
    .await;
    b.close().await;
    r
}

/// Dark pixels per 1000-row band: what a failed probe reports, so a blank,
/// black or half-painted frame is told apart at a glance.
fn ink_bands(f: &shm::Frame) -> Vec<u64> {
    let px = f.seg.as_slice();
    let mut bands = vec![0u64; f.height.div_ceil(1000)];
    for y in 0..f.height {
        for p in px[y * f.stride..][..f.width * 4].chunks_exact(4) {
            if (p[0] as u32 + p[1] as u32 + p[2] as u32) < 384 {
                bands[y / 1000] += 1;
            }
        }
    }
    bands
}

/// Commands that drive pages initialize CEF on the main thread; the rest never
/// load Chromium at all.
fn needs_chromium() -> bool {
    match std::env::args().nth(1).unwrap_or_default().as_str() {
        "kumo-lane" | "probe" | "bench" | "index.handler" => true,
        "" => !handler_name().starts_with("fetch-hop"),
        _ => false,
    }
}

fn main() {
    // A CEF child re-executes this binary with --type=...; it runs nothing else.
    if let Some(code) = cefhost::run_subprocess_if_child() {
        std::process::exit(code);
    }
    // The PNG writer is its own process with its own runtime (writer.rs).
    if std::env::args().nth(1).as_deref() == Some("png-writer") {
        let path = std::env::args().nth(2).unwrap_or_default();
        if let Err(e) = writer::serve(&path) {
            eprintln!("snapbot png-writer fatal: {e:#}");
            std::process::exit(1);
        }
        return;
    }
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
