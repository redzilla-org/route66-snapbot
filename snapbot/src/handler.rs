//! Action dispatch: the Lambda event contract of the former index.js.
//!
//! browse (no publication; the caller judges), attest-run-manifest (the one
//! non-GitHub attestation, GH #3822), and the ticket attestations, each of
//! which joins mandatory GitHub publication before returning. The IPv6
//! fetch-hop function runs the same binary under _HANDLER=fetch-hop.handler.

use crate::{attest, awsres, browse, capture, ci, github, httpraw, js};
use anyhow::{bail, Result};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

static STARTED: AtomicI64 = AtomicI64::new(0);

/// "[phase] <name> +<ms>ms <detail>": which phase spent the budget (GH #3177).
/// Nothing from the page is ever logged here.
pub fn phase(name: &str, detail: &str) {
    let ms = js::now_ms() - STARTED.load(Ordering::SeqCst);
    if detail.is_empty() {
        println!("[phase] {name} +{ms}ms");
    } else {
        println!("[phase] {name} +{ms}ms {detail}");
    }
}

/// The run-manifest key grammar is security-load-bearing (GH #3822): the
/// signing exemption covers only the durable web-regression run namespace.
fn require_run_manifest_key(key: &str) -> Result<()> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^(california-prod|california-dev|chicago-prod|chicago-dev|local-test)/([0-9a-f]{40})/(\d{8}T\d{6}Z)/manifest\.json$").unwrap()
    });
    let Some(m) = re.captures(key) else {
        bail!("attest-run-manifest key must be <env>/<40-lowercase-hex-sha>/<YYYYMMDDTHHMMSSZ>/manifest.json");
    };
    // Round-trip through a real clock so impossible values (month 99) fail.
    if chrono::NaiveDateTime::parse_from_str(&m[3], "%Y%m%dT%H%M%SZ").is_err() {
        bail!("attest-run-manifest key contains an invalid UTC run timestamp");
    }
    Ok(())
}

/// The capture action's own target identity, never a second free-form field.
fn attestation_target(event: &Value) -> String {
    let action = event.get("action").and_then(Value::as_str).unwrap_or("");
    match action {
        "capture-web-ui-screenshot" | "capture-http-raw" => js::metadata_value(event.get("url"), 2000).trim().to_string(),
        "attest-ci-verdict" => {
            let env = js::coalesce(&[event.get("env"), event.get("environment")]).map(js::js_string).unwrap_or_default();
            let sha = js::coalesce(&[event.get("target_sha"), event.get("targetSHA")]).map(js::js_string).unwrap_or_default();
            js::metadata_str(&format!("{env}@{sha}"), 2000).trim().to_string()
        }
        "attest-aws-resource" => {
            let s = event.get("service").filter(|v| !v.is_null()).map(js::js_string).unwrap_or_default();
            let o = event.get("operation").filter(|v| !v.is_null()).map(js::js_string).unwrap_or_default();
            let p = serde_json::to_string(event.get("params").filter(|v| js::truthy(Some(v))).unwrap_or(&json!({}))).unwrap_or_default();
            js::metadata_str(&format!("{s}.{o} {p}"), 2000).trim().to_string()
        }
        _ => {
            let ci = event.get("ci").filter(|v| v.is_object()).cloned().unwrap_or(json!({}));
            if let Some(sha) = js::coalesce(&[ci.get("target_sha"), ci.get("targetSHA")]).filter(|v| js::truthy(Some(v))) {
                let env = ci.get("env").filter(|v| !v.is_null()).map(js::js_string).unwrap_or_default();
                return js::metadata_str(&format!("{env}@{}", js::js_string(sha)), 2000).trim().to_string();
            }
            let bucket = crate::config::cfg().map(|c| c.bucket.clone()).unwrap_or_default();
            let key = event.get("key").filter(|v| !v.is_null()).map(js::js_string).unwrap_or_default();
            let vid = match event.get("version_id") {
                Some(v) if js::truthy(Some(v)) => format!("?versionId={}", js::js_string(v)),
                _ => String::new(),
            };
            js::metadata_str(&format!("s3://{bucket}/{key}{vid}"), 2000).trim().to_string()
        }
    }
}

/// The signed claim (intent, category, target) every published attestation carries.
fn required_attestation_context(event: &Value) -> Result<Map<String, Value>> {
    let intent = js::metadata_value(event.get("intent"), 1000).trim().to_string();
    let category = js::metadata_value(event.get("category"), 16).trim().to_uppercase();
    let target = attestation_target(event);
    if intent.is_empty() {
        bail!("published attestation requires intent");
    }
    if category != "BEFORE" && category != "AFTER" {
        bail!("published attestation category must be BEFORE or AFTER");
    }
    if target.is_empty() {
        bail!("published attestation action has no target identity");
    }
    let mut m = Map::new();
    m.insert("claim.intent".into(), json!(intent));
    m.insert("claim.category".into(), json!(category));
    m.insert("claim.target".into(), json!(target));
    Ok(m)
}

pub fn add_attestation_context(observed: &mut Map<String, Value>, event: &Value) {
    if let Some(ctx) = event.get("_attestation_context").and_then(Value::as_object) {
        for (k, v) in ctx {
            observed.insert(k.clone(), v.clone());
        }
    }
}

async fn attest_existing_object(event: &Value) -> Result<Value> {
    let cfg = crate::config::cfg()?;
    if let Some(b) = event.get("bucket").filter(|v| js::truthy(Some(v))) {
        if js::js_string(b) != cfg.bucket {
            bail!("bucket {} is not the evidence bucket", js::js_string(b));
        }
    }
    let mut observed = Map::new();
    let c = event.get("ci").cloned().unwrap_or(json!({}));
    if js::truthy(c.get("env")) || js::truthy(c.get("target_sha")) {
        let env = js::metadata_value(c.get("env"), 64);
        let sha = js::metadata_value(c.get("target_sha"), 80);
        if env.is_empty() || sha.is_empty() {
            bail!("ci requires both env and target_sha");
        }
        observed.extend(ci::capture_dev_ci(&env, &sha).await);
    }
    add_attestation_context(&mut observed, event);
    let key = event.get("key").and_then(Value::as_str).unwrap_or("");
    let vid = event.get("version_id").filter(|v| js::truthy(Some(v))).map(js::js_string).unwrap_or_default();
    attest::attest(key, &vid, observed).await
}

async fn attest_ci_verdict(event: &Value) -> Result<Value> {
    let issue = js::metadata_value(js::coalesce(&[event.get("issue"), event.get("issue_number")]).or(Some(&json!("unknown"))), 64);
    let env = js::metadata_value(js::coalesce(&[event.get("env"), event.get("environment")]), 64);
    let sha = js::metadata_value(js::coalesce(&[event.get("target_sha"), event.get("targetSHA")]), 80);
    if env.is_empty() {
        bail!("attest-ci-verdict requires env");
    }
    if sha.is_empty() {
        bail!("attest-ci-verdict requires target_sha");
    }
    let mut ci = ci::capture_dev_ci(&env, &sha).await;
    let checked = ci.get("ci.checked-at-utc").and_then(Value::as_str).unwrap_or("").to_string();
    if checked.is_empty() {
        bail!("CI capture produced no ci.checked-at-utc");
    }
    ci.entry("ci.executed-sha").or_insert(json!(""));
    let key = attest::evidence_key(&env, &issue, &js::compact_stamp(&checked), "ci-verdict.json");
    let doc = json!({"v": 1, "evidence_type": "ci-verdict", "env": env, "issue": issue, "target_sha": sha, "ci": ci});
    let metadata = [
        ("environment", env.clone()),
        ("issue-number", issue.clone()),
        ("target-sha", sha.clone()),
        ("evidence-type", "ci-verdict".to_string()),
        ("captured-by", "command-center evidence-attestor".to_string()),
    ];
    let vid = attest::put_evidence(&key, serde_json::to_vec_pretty(&doc)?, "application/json", &metadata).await?;
    let mut observed = Map::new();
    observed.insert("ci.evidence-type".into(), json!("ci-verdict"));
    observed.insert("ci.issue".into(), json!(issue));
    observed.extend(ci);
    add_attestation_context(&mut observed, event);
    attest::attest(&key, &vid, observed).await
}

async fn capture_attestation(event: &Value) -> Result<Value> {
    match event.get("action").and_then(Value::as_str).unwrap_or("") {
        "capture-web-ui-screenshot" => capture::capture_web_ui_screenshot(event).await,
        "attest-aws-resource" => awsres::attest_aws_resource(event).await,
        "attest-ci-verdict" => attest_ci_verdict(event).await,
        "capture-http-raw" => httpraw::capture_http_raw(event).await,
        _ => attest_existing_object(event).await,
    }
}

/// The main Lambda handler (index.handler).
pub async fn handle(event: Value) -> Result<Value> {
    STARTED.store(js::now_ms(), Ordering::SeqCst);
    let action = event.get("action").and_then(Value::as_str).unwrap_or("");
    if action == "browse" {
        return browse::browse(&event).await;
    }
    // Legacy clients may still invoke this action. A browse now joins its own
    // writes before replying, so there is no pool-wide queue left to drain.
    if action == "drain-writer" {
        return Ok(json!({"drained": true, "wait_ms": 0}));
    }
    crate::config::cfg()?;
    if action == "attest-run-manifest" {
        let key = event.get("key").and_then(Value::as_str).unwrap_or("");
        let vid = event.get("version_id").and_then(Value::as_str).map(str::trim).unwrap_or("");
        require_run_manifest_key(key)?;
        if vid.is_empty() {
            bail!("attest-run-manifest requires the manifest object's VersionId");
        }
        return attest_existing_object(&event).await;
    }
    let target = github::target(&event)?;
    let ctx = required_attestation_context(&event)?;
    // The capture never sees the GitHub credential.
    let mut capture = event.clone();
    if let Some(o) = capture.as_object_mut() {
        o.remove("github");
        o.insert("_attestation_context".into(), Value::Object(ctx));
    }
    let result = capture_attestation(&capture).await?;
    github::post(&target, result).await
}

/// Dispatch on the Lambda handler name (ImageConfig.Command / _HANDLER).
pub async fn dispatch(handler: &str, event: Value) -> Result<Value> {
    if handler.starts_with("fetch-hop") {
        return httpraw::fetch_hop(&event).await;
    }
    handle(event).await
}
