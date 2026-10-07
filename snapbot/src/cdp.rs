//! CDP commands to an in-process page (cefhost), with the GH #4082 diagnostics.
//!
//! WHY RAW CDP STILL (GH #4115). The handler drives Chromium the way puppeteer did,
//! command for command, so the browse contract route66 depends on keeps its
//! semantics. Since the CEF rework the commands no longer cross a websocket:
//! cefhost hands them to CefBrowserHost::SendDevToolsMessage in this process.

use anyhow::{anyhow, Result};
use serde_json::Value;

// An enclosing action can cancel its waiter before the existing slow-call
// trace fires. Observe that drop without cancelling or replaying the command.
struct CancelledCommandDiagnostic<'a> {
    method: &'a str,
    page: i32,
    identity: String,
    started: std::time::Instant,
    completed: bool,
}

impl Drop for CancelledCommandDiagnostic<'_> {
    fn drop(&mut self) {
        if !self.completed {
            eprintln!(
                "[snapbot diagnostic] {} method={} page={} cancelled_ms={}",
                self.identity,
                self.method,
                self.page,
                self.started.elapsed().as_millis()
            );
        }
    }
}

/// Send one command to page `page` and await its result.
pub async fn send(page: i32, method: &str, params: Value) -> Result<Value> {
    if !crate::cefhost::session_open(page) {
        return Err(anyhow!("Protocol error ({method}): Target closed"));
    }
    let reply = crate::cefhost::send(page, method, params);
    let reply = if crate::runtime::diagnostic_enabled() {
        let mut cancellation = CancelledCommandDiagnostic {
            method,
            page,
            identity: crate::runtime::diagnostic_identity(),
            started: std::time::Instant::now(),
            completed: false,
        };
        // GH #4082: keep the same pending reply after one diagnostic. This
        // observes stalls without cancelling or retrying any command.
        tokio::pin!(reply);
        let r = match tokio::time::timeout(std::time::Duration::from_secs(5), &mut reply).await {
            Ok(r) => r,
            Err(_) => {
                eprintln!("[snapbot diagnostic] {} method={method} page={page} pending_ms={}", cancellation.identity, cancellation.started.elapsed().as_millis());
                let r = reply.await;
                eprintln!("[snapbot diagnostic] {} method={method} returned_ms={}", cancellation.identity, cancellation.started.elapsed().as_millis());
                r
            }
        };
        cancellation.completed = true;
        r
    } else {
        reply.await
    };
    reply.map_err(|e| anyhow!("Protocol error ({method}): {e}"))
}
