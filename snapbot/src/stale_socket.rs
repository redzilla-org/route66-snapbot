//! `snapbot stale-socket`: the one reproduction of route66 GH #4487's CI DEV
//! failure (Dockerfile.test only).
//!
//! Observed: arn:aws:states:us-east-2:686255971907:execution:chicago-dev-ci-orchestrator:premerge-97b320de45dd2235b601ba75713852fa116faf53,
//! snapbot RequestId 5a77a505-c349-47a2-a0c2-424ce49e367b. A sandbox frozen
//! 4.95 s after five calls sent its next document request on a pooled
//! connection that died while frozen; no response came within the 4000 ms
//! goto and CloudFront never saw the request.
//!
//! WHY this shape: a local keep-alive server serves one browse, then black-holes
//! every connection accepted before the "freeze" (reads, never answers, never
//! closes: a dead NAT/QUIC path looks exactly like this to Chromium), the
//! process idles as long as the sandbox was frozen, and the next browse must
//! complete its goto inside the same 4000 ms budget.

use crate::browse;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The observed freeze and the CI journey's goto budget.
const FREEZE: Duration = Duration::from_millis(4950);
const GOTO_MS: u64 = 4000;

/// Serve HTML over keep-alive; a connection accepted in an earlier epoch than
/// the current one never gets another response.
async fn serve(listener: TcpListener, epoch: Arc<AtomicU64>) {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else { return };
        let epoch = epoch.clone();
        let born = epoch.load(Ordering::SeqCst);
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let Ok(n) = sock.read(&mut chunk).await else { return };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    buf.drain(..end + 4);
                    if epoch.load(Ordering::SeqCst) != born {
                        // The pre-freeze path is dead: hold the socket, answer nothing.
                        continue;
                    }
                    let body = "<!doctype html><title>ok</title><p>ok</p>";
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}", body.len());
                    if sock.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            }
        });
    }
}

pub async fn run() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
    let epoch = Arc::new(AtomicU64::new(0));
    tokio::spawn(serve(listener, epoch.clone()));
    // The CI shape: Fetch interception on (a host header rule) and a 4000 ms goto.
    let event = json!({
        "context": {"navigation_timeout_ms": GOTO_MS, "extra_headers": [{"host": "127.0.0.1", "headers": {"x-snapbot-test": "stale-socket"}}]},
        "steps": [{"id": "nav", "op": "goto", "url": url, "wait_until": "domcontentloaded", "timeout_ms": GOTO_MS}],
    });
    browse::warm(&json!([])).await?;
    let first = browse::browse(&event).await?;
    println!("STALE-SOCKET before freeze: {}", first["steps"]);
    if first["ok"] != Value::Bool(true) {
        bail!("the pre-freeze browse failed: {first}");
    }
    epoch.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(FREEZE).await;
    let second = browse::browse(&event).await?;
    println!("STALE-SOCKET after {}ms freeze: {}", FREEZE.as_millis(), second["steps"]);
    if second["ok"] != Value::Bool(true) {
        bail!("STALE-SOCKET FAIL: the post-freeze goto rode a pre-freeze socket; network={}", second["timings"]["network"]);
    }
    println!("STALE-SOCKET PASS");
    Ok(())
}
