//! Mandatory GitHub publication of a ticket attestation (#3767, compact #3933).
//! The token lives in this invocation only; a retry of the same artifact
//! reuses its already posted, signature-verified receipt.

use crate::{attest, config, js};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

pub struct Target {
    pub issue: i64,
    token: String,
}

pub fn target(event: &Value) -> Result<Target> {
    let g = event.get("github").cloned().unwrap_or(json!({}));
    let issue = js::js_number(g.get("issue"));
    let token = g.get("token").and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
    let safe = issue.is_finite() && issue.fract() == 0.0 && issue.abs() <= 9007199254740991.0;
    if !safe || issue <= 0.0 || token.is_empty() || token.contains(['\r', '\n']) {
        bail!("GitHub publication requires a positive issue/PR number and invocation-only token");
    }
    let issue = issue as i64;
    if let Some(i) = event.get("issue").filter(|v| !v.is_null()) {
        if js::js_number(Some(i)) != issue as f64 {
            bail!("GitHub publication target disagrees with capture issue");
        }
    }
    if let Some(k) = event.get("key").and_then(Value::as_str) {
        if let Some(c) = regex::Regex::new(r"/issue-evidence/issue-(\d+)/").unwrap().captures(k) {
            if c[1].parse::<f64>().unwrap_or(-1.0) != issue as f64 {
                bail!("GitHub publication target disagrees with evidence key");
            }
        }
    }
    Ok(Target { issue, token })
}

async fn github_json(t: &Target, method: reqwest::Method, suffix: &str, body: Option<Value>) -> Result<Value> {
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(15)).build()?;
    let url = format!("https://api.github.com/repos/redzilla-org/route66/issues/{}/comments{suffix}", t.issue);
    let mut req = client
        .request(method.clone(), url)
        .header("Authorization", format!("Bearer {}", t.token))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "route66-evidence-attestor")
        .header("Content-Type", "application/json");
    if let Some(b) = body {
        req = req.body(b.to_string());
    }
    let res = req.send().await.map_err(|e| anyhow!("GitHub attestation {method} failed: {}", if e.is_timeout() { "timeout" } else { "transport" }))?;
    if !res.status().is_success() {
        bail!("GitHub attestation {method} failed: HTTP {}", res.status().as_u16());
    }
    let bytes = res.bytes().await?;
    if bytes.len() > 8 * 1024 * 1024 {
        bail!("GitHub comment response exceeds 8 MiB");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// GFM inline-active characters are escaped so signed text renders literally.
fn markdown_literal(v: &Value) -> String {
    let s = v.as_str().map(str::to_string).unwrap_or_else(|| if v.is_null() { String::new() } else { js::js_string(v) });
    let mut out = String::new();
    for c in s.chars() {
        if "\\`*_[]<>&|~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn armored(result: &Value, identity: &str) -> String {
    let o = &result["observed"];
    let s = |v: &Value| js::js_string(v);
    [
        "-----BEGIN ROUTE66 SIGNED ATTESTATION-----".to_string(),
        String::new(),
        format!("**{}: {}**", markdown_literal(&o["claim.category"]), markdown_literal(&o["claim.intent"])),
        String::new(),
        format!("Target: {}", markdown_literal(&o["claim.target"])),
        String::new(),
        format!("Attested: {}", s(&result["object"]["attested_at_utc"])),
        String::new(),
        format!("Evidence: <{}>", s(&result["url"])),
        String::new(),
        format!("Statement: <{}>", s(&result["attestation_url"])),
        String::new(),
        format!("{} \u{b7} Key-ID: {}", s(&result["canonicalization"]), s(&result["key_id"])),
        String::new(),
        format!("<!-- r66-attestation-identity: {identity} -->"),
        String::new(),
        "-----END ROUTE66 SIGNED ATTESTATION-----".to_string(),
    ]
    .join("\n")
}

fn obs(result: &Value, k: &str) -> Value {
    match result["observed"].get(k) {
        Some(v) if js::truthy(Some(v)) => v.clone(),
        _ => json!(""),
    }
}

pub async fn post(t: &Target, mut result: Value) -> Result<Value> {
    let ident_src = json!([
        "compact-v1", result["object"]["bucket"], result["object"]["key"], result["object"]["version_id"],
        obs(&result, "ci.env"), obs(&result, "ci.target-sha"),
        result["observed"]["claim.category"], result["observed"]["claim.intent"], result["observed"]["claim.target"],
    ]);
    let identity = attest::sha256_hex(serde_json::to_string(&ident_src)?.as_bytes());
    let marker = format!("\n<!-- r66-attestation-identity: {identity} -->\n");
    let cfg = config::cfg()?;
    for page in 1..=10 {
        let comments = github_json(t, reqwest::Method::GET, &format!("?per_page=100&page={page}"), None).await?;
        let list = comments.as_array().ok_or_else(|| anyhow!("GitHub comment listing is malformed"))?;
        for comment in list {
            let Some(body) = comment["body"].as_str().filter(|b| b.contains(&marker)) else { continue };
            // An editable comment: verify its original statement before reuse.
            let re = regex::Regex::new(r"(?m)^Statement: <(https://[^>\s]+)>$").unwrap();
            let loc = re.captures(body).map(|c| c[1].to_string()).ok_or_else(|| anyhow!("Existing attestation comment has no sidecar locator"))?;
            let version = url::Url::parse(&loc)?.query_pairs().find(|(k, _)| k == "versionId").map(|(_, v)| v.into_owned()).unwrap_or_default();
            let att_key = result["attestation_key"].as_str().unwrap_or("").to_string();
            if version.is_empty() || loc != attest::public_url(&cfg.bucket, &att_key, &version)? {
                bail!("Existing attestation comment has an invalid sidecar locator");
            }
            let saved = config::s3()
                .await
                .get_object()
                .bucket(&cfg.bucket)
                .key(&att_key)
                .version_id(&version)
                .send()
                .await
                .map_err(|e| anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))?;
            let len = saved.content_length().unwrap_or(0);
            if len <= 0 || len > 65536 {
                bail!("Attestation sidecar size is invalid");
            }
            let mut prior: Value = serde_json::from_slice(&saved.body.collect().await?.into_bytes())?;
            prior["url"] = json!(attest::public_url(&cfg.bucket, prior["object"]["key"].as_str().unwrap_or(""), prior["object"]["version_id"].as_str().unwrap_or(""))?);
            prior["attestation_key"] = json!(att_key);
            prior["attestation_url"] = json!(loc);
            let observed = prior["observed"].as_object().cloned().unwrap_or_default();
            let manifest = attest::canonical_manifest(&prior["object"], &observed);
            let valid = prior["manifest"].as_str() == Some(manifest.as_str())
                && prior["key_id"] == json!(attest::KEY_ID)
                && attest::verify(&manifest, prior["signature_b64"].as_str().unwrap_or(""))
                && prior["object"]["bucket"] == result["object"]["bucket"]
                && prior["object"]["key"] == result["object"]["key"]
                && prior["object"]["version_id"] == result["object"]["version_id"]
                && prior["url"] == result["url"]
                && prior["observed"].get("ci.env") == result["observed"].get("ci.env")
                && prior["observed"].get("ci.target-sha") == result["observed"].get("ci.target-sha")
                && armored(&prior, &identity) == body;
            if !valid {
                bail!("Existing attestation comment failed signed receipt validation");
            }
            let html = comment["html_url"].as_str().filter(|u| !u.is_empty()).ok_or_else(|| anyhow!("Existing attestation comment has no publication URL"))?;
            crate::handler::phase("github-publication", &format!("issue={} reused=true", t.issue));
            prior["evidence_text"] = json!(body);
            prior["comment_url"] = json!(html);
            prior["github_posted"] = json!(true);
            return Ok(prior);
        }
        if list.len() < 100 {
            break;
        }
        if page == 10 {
            bail!("GitHub attestation deduplication exceeds 1000 comments");
        }
    }
    let block = armored(&result, &identity);
    if block.encode_utf16().count() > 60000 {
        bail!("Armored attestation exceeds GitHub comment size budget");
    }
    let posted = github_json(t, reqwest::Method::POST, "", Some(json!({"body": block}))).await?;
    let html = posted["html_url"].as_str().unwrap_or("");
    if posted["body"].as_str() != Some(block.as_str()) || html.is_empty() {
        bail!("GitHub did not confirm the identical attestation comment");
    }
    crate::handler::phase("github-publication", &format!("issue={} reused=false", t.issue));
    result["evidence_text"] = json!(block);
    result["comment_url"] = json!(html);
    result["github_posted"] = json!(true);
    Ok(result)
}
