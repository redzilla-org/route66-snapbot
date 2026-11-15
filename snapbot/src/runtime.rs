//! The Lambda Runtime API client loop: one invocation at a time, the same loop
//! in the managed Lambda and in each local pool lane (Kumo exposes the same API).

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

// GH #4082: opt-in request attribution diagnoses the observed stalled local
// browser call without logging its payload or adding logs to normal captures.
pub fn diagnostic_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("SNAPBOT_DIAGNOSTICS").as_deref() == Ok("1"))
}

fn diagnostic_request() -> &'static Mutex<String> {
    static REQUEST: OnceLock<Mutex<String>> = OnceLock::new();
    REQUEST.get_or_init(|| Mutex::new(String::new()))
}

pub fn diagnostic_identity() -> String {
    format!("pid={} lane={} request={}", std::process::id(), std::env::var("SNAPBOT_LANE").unwrap_or_default(), diagnostic_request().lock().unwrap())
}

pub fn diagnostic_phase(phase: &str) {
    if diagnostic_enabled() {
        eprintln!("[snapbot diagnostic] {} phase={phase}", diagnostic_identity());
    }
}

pub fn runtime_base() -> Result<String> {
    let rt = std::env::var("AWS_LAMBDA_RUNTIME_API").unwrap_or_default();
    if rt.is_empty() || rt.starts_with("http://") || rt.starts_with("https://") || rt.contains(['\r', '\n']) {
        bail!("AWS_LAMBDA_RUNTIME_API must be a host/path without scheme or newlines");
    }
    Ok(format!("http://{rt}/2018-06-01/runtime/invocation"))
}

/// Poll forever. /next is retried on transport errors and non-2xx: a retained
/// pool outlives Kumo restarts (route66 run306), and both are "no work yet".
pub async fn run(handler_name: String) -> Result<()> {
    let base = runtime_base()?;
    let client = reqwest::Client::builder().build()?;
    let lane = std::env::var("SNAPBOT_LANE").unwrap_or_default();
    loop {
        let mut delay = 100u64;
        let next = loop {
            match client.get(format!("{base}/next")).send().await {
                Ok(r) if r.status().is_success() => break r,
                Ok(r) => eprintln!("snapbot lane {lane}: Runtime API next returned HTTP {}; retrying in {delay}ms", r.status().as_u16()),
                Err(e) => eprintln!("snapbot lane {lane}: Runtime API transport error, retrying in {delay}ms: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(delay)).await;
            delay = (delay * 2).min(5000);
        };
        let id = next
            .headers()
            .get("lambda-runtime-aws-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("Runtime API next omitted request id on lane {lane}"))?;
        // A durable browse reply must finish inside this invocation's existing budget.
        let deadline_ms = next.headers().get("lambda-runtime-deadline-ms")
            .and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<i64>().ok());
        let event: Value = next.json().await.unwrap_or(Value::Null);
        // Only opt-in browse calls get a private recovery object; other actions stay unchanged.
        let result_put_url = if !handler_name.starts_with("fetch-hop") && event.get("action").and_then(Value::as_str) == Some("browse") {
            match event.get("result_put_url") {
                None | Some(Value::Null) => None,
                Some(Value::String(url)) if !url.is_empty() => Some(url.clone()),
                Some(_) => bail!("browse result_put_url must be a nonempty string"),
            }
        } else {
            None
        };
        if diagnostic_enabled() {
            *diagnostic_request().lock().unwrap() = id.clone();
            diagnostic_phase("dispatch-start");
        }
        let (mut suffix, mut body) = match crate::handler::dispatch(&handler_name, event).await {
            Ok(v) => ("response", v),
            Err(e) => ("error", json!({"errorMessage": format!("{e:#}"), "errorType": "Error", "stackTrace": []})),
        };
        diagnostic_phase("dispatch-return");
        // Persist this single execution's exact outcome before the response can be lost in transit.
        if let Some(url) = result_put_url {
            let delivery = crate::store::put_browse_result(&url, &body, suffix == "error", deadline_ms).await;
            if let Err(e) = delivery {
                suffix = "error";
                body = json!({"errorMessage": format!("{e:#}"), "errorType": "Error", "stackTrace": []});
            }
        }
        // Browse joined its own writes before dispatch returned; another
        // process-wide drain here would repeat the same wait.
        let r = client.post(format!("{base}/{id}/{suffix}")).header("content-type", "application/json").body(body.to_string()).send().await?;
        if !r.status().is_success() {
            bail!("Runtime API {suffix} returned HTTP {}", r.status().as_u16());
        }
        if diagnostic_enabled() {
            diagnostic_phase("response-posted");
            diagnostic_request().lock().unwrap().clear();
        }
    }
}
