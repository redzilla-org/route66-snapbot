//! Raw Chrome DevTools Protocol client over the browser's websocket.
//!
//! WHY RAW CDP (GH #4115). The handler drives Chromium the way puppeteer did,
//! command for command, so the browse contract route66 depends on keeps its
//! semantics. A thin JSON-RPC multiplexer with flattened sessions gives that
//! control without a second abstraction's opinions about waits or events.

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

/// One CDP event, routed to the session that emitted it.
#[derive(Clone, Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
}

type Pending = HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>;

pub struct Cdp {
    out: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    pending: Mutex<Pending>,
    sessions: Mutex<HashMap<String, mpsc::UnboundedSender<Event>>>,
    closed: AtomicBool,
}

// An enclosing action can cancel its waiter before the existing slow-call
// trace fires. Observe that drop without cancelling or replaying the command.
struct CancelledCommandDiagnostic<'a> {
    id: u64,
    method: &'a str,
    session: Option<&'a str>,
    identity: String,
    started: std::time::Instant,
    completed: bool,
}

impl Drop for CancelledCommandDiagnostic<'_> {
    fn drop(&mut self) {
        if !self.completed {
            eprintln!(
                "[snapbot diagnostic] {} cdp_id={} method={} session={} cancelled_ms={}",
                self.identity,
                self.id,
                self.method,
                self.session.unwrap_or("browser"),
                self.started.elapsed().as_millis()
            );
        }
    }
}

impl Cdp {
    /// Connect to the browser endpoint. Frame and message caps are lifted: a
    /// full-page screenshot of a tall page arrives as one large base64 reply.
    pub async fn connect(ws_url: &str) -> Result<Arc<Cdp>> {
        let mut cfg = WebSocketConfig::default();
        cfg.max_message_size = None;
        cfg.max_frame_size = None;
        let (ws, _) = tokio_tungstenite::connect_async_with_config(ws_url, Some(cfg), false)
            .await
            .map_err(|e| anyhow!("connect DevTools websocket {ws_url}: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        let cdp = Arc::new(Cdp {
            out,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        });

        // Writer: one task owns the sink so commands are never interleaved.
        tokio::spawn(async move {
            while let Some(text) = out_rx.recv().await {
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
        });

        // Reader: replies resolve their pending command; events go to their
        // session. A closed socket fails every waiter instead of hanging it.
        let reader = cdp.clone();
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let text = match msg {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let tx = reader.pending.lock().unwrap().remove(&id);
                    if let Some(tx) = tx {
                        let res = match v.get("error") {
                            Some(err) => Err(err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("CDP error")
                                .to_string()),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(res);
                    }
                    continue;
                }
                let method = v.get("method").and_then(Value::as_str).unwrap_or("").to_string();
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                let session = v.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
                let tx = reader.sessions.lock().unwrap().get(&session).cloned();
                if let Some(tx) = tx {
                    let _ = tx.send(Event { method, params });
                }
            }
            reader.closed.store(true, Ordering::SeqCst);
            let pending: Vec<_> = reader.pending.lock().unwrap().drain().collect();
            for (_, tx) in pending {
                let _ = tx.send(Err("Target closed: the browser connection closed".to_string()));
            }
            // Dropping the senders ends every session's event loop.
            reader.sessions.lock().unwrap().clear();
        });
        Ok(cdp)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Send one command on the browser (session None) or a page session.
    pub async fn send(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        if self.is_closed() {
            return Err(anyhow!("Protocol error ({method}): Target closed"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = Value::String(s.to_string());
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if self.out.send(msg.to_string()).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(anyhow!("Protocol error ({method}): Target closed"));
        }
        let mut cancellation =
            crate::runtime::diagnostic_enabled().then(|| CancelledCommandDiagnostic {
                id,
                method,
                session,
                identity: crate::runtime::diagnostic_identity(),
                started: std::time::Instant::now(),
                completed: false,
            });
        // GH #4082: retain the same pending reply after one diagnostic. This
        // observes the 22–84s stalls without cancelling or retrying any command.
        let reply = if crate::runtime::diagnostic_enabled() {
            let identity = crate::runtime::diagnostic_identity();
            let started = std::time::Instant::now();
            let mut rx = rx;
            match tokio::time::timeout(std::time::Duration::from_secs(5), &mut rx).await {
                Ok(reply) => reply,
                Err(_) => {
                    eprintln!("[snapbot diagnostic] {identity} cdp_id={id} method={method} session={} pending_ms={} closed={}", session.unwrap_or("browser"), started.elapsed().as_millis(), self.is_closed());
                    let reply = rx.await;
                    eprintln!("[snapbot diagnostic] {identity} cdp_id={id} method={method} returned_ms={}", started.elapsed().as_millis());
                    reply
                }
            }
        } else {
            rx.await
        };
        if let Some(diagnostic) = cancellation.as_mut() {
            diagnostic.completed = true;
        }
        match reply {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("Protocol error ({method}): {e}")),
            Err(_) => Err(anyhow!("Protocol error ({method}): Target closed")),
        }
    }

    /// Route a session's events to a fresh receiver.
    pub fn subscribe(&self, session: &str) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.sessions.lock().unwrap().insert(session.to_string(), tx);
        rx
    }

    pub fn unsubscribe(&self, session: &str) {
        self.sessions.lock().unwrap().remove(session);
    }
}
