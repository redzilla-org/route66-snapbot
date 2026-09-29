//! route66-snapbot: one static binary for the Lambda handler, the local
//! Kumo pool and its self-test (GH #4115; the Node coordinator is burned).
//!
//!   snapbot                  Lambda bootstrap: Runtime API loop, handler = $_HANDLER
//!   snapbot kumo-runtime     local pool dispatcher (SNAPBOT_POOL_PROCESSES lanes)
//!   snapbot kumo-lane        one pool lane (started by kumo-runtime)
//!   snapbot probe            image build check: Chromium renders a page, the
//!                            in-process engine OCRs the captured PNG
//!   snapbot --version

mod actions;
mod attest;
mod awsres;
mod browse;
mod browser;
mod capture;
mod cdp;
mod ci;
mod config;
mod github;
mod handler;
mod httpraw;
mod js;
mod keys;
mod kumo;
mod ocr;
mod runtime;
mod store;

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
        "" | "index.handler" | "fetch-hop.handler" => {
            let h = if arg.is_empty() { handler } else { arg };
            init(&h)?;
            runtime::run(h).await
        }
        other => anyhow::bail!("unknown command {other:?}"),
    }
}

/// The image's own proof: the baked Chromium renders, the capture path yields
/// a PNG, and the linked engine reads its text back in memory.
async fn probe() -> Result<()> {
    const TEXT: &str = "SNAPBOT PROBE 4115";
    let b = browser::Browser::launch(browser::LaunchOptions { single_process: false, ignore_https_errors: true, extra_args: Vec::new() }).await?;
    let page = b.new_page(None).await?;
    page.set_viewport(1366, 900).await?;
    let html = format!("data:text/html,<h1 style=\"font:64px sans-serif\">{TEXT}</h1>");
    let r = async {
        page.goto(&html.replace(' ', "%20"), "load", 30000).await?;
        let png = page.screenshot(true).await?;
        let (spec, _) = ocr::parse(&serde_json::json!({"passes": [{"psm": 3}]}))?;
        let out = ocr::read(std::sync::Arc::new(png), spec, None).await?;
        let text = out["text"].as_str().unwrap_or("").to_string();
        anyhow::ensure!(text.contains(TEXT), "probe OCR read {text:?}, expected {TEXT:?}: {out}");
        println!("snapbot probe: chromium rendered and OCR read it back; timings {}", out["timings"]);
        Ok(())
    }
    .await;
    b.close().await;
    r
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("snapbot fatal: {e:#}");
        std::process::exit(1);
    }
}
