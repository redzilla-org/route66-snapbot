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
            runtime::run(h).await
        }
        other => anyhow::bail!("unknown command {other:?}"),
    }
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
