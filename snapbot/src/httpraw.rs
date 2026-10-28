//! capture-http-raw (evidence-type:http-raw) and the IPv6 fetch-hop function.
//!
//! The evidence object IS the response body as served, and the redirect chain
//! is not collapsed: each hop is its own signed line. A 200 that is really a
//! login redirect, or a CDN HIT still serving pre-fix bytes, both read as "the
//! fix shipped" from a status code alone; requested-vs-final URL and
//! x-cache/age separate them.
//!
//! An http:// hop goes to the stack's IPv6 fetch function (route66#3659): the
//! prod rendezvous health origins are AAAA-only plain http and this function's
//! egress is IPv4-only. The fetch function runs this same binary with
//! _HANDLER=fetch-hop.handler, holds no key and writes nothing.

use crate::attest;
use crate::capture::{cookie_fingerprint, header_observed, normalize_cookies};
use crate::{ci, config, js};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::time::Duration;

const RAW_MAX_REDIRECTS: i64 = 10;

fn raw_body_extension(ct: &str) -> &'static str {
    let ct = ct.to_lowercase();
    if ct.starts_with("text/html") {
        "html"
    } else if ct.starts_with("application/json") || (ct.starts_with("application/") && ct.contains("+json")) {
        "json"
    } else if ct.starts_with("application/xml") || ct.starts_with("text/xml") || ct.contains("+xml") {
        "xml"
    } else if ct.starts_with("text/plain") {
        "txt"
    } else if ct.starts_with("text/css") {
        "css"
    } else if ct.starts_with("application/javascript") || ct.starts_with("text/javascript") {
        "js"
    } else {
        "bin"
    }
}

/// Node's res.headers: lowercased names, duplicates joined with ", ",
/// set-cookie kept as an array.
fn node_headers(h: &reqwest::header::HeaderMap) -> Value {
    let mut out = Map::new();
    for name in h.keys() {
        let vals: Vec<String> = h.get_all(name).iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect();
        let k = name.as_str().to_lowercase();
        if k == "set-cookie" {
            out.insert(k, json!(vals));
        } else {
            out.insert(k, json!(vals.join(", ")));
        }
    }
    Value::Object(out)
}

struct Hop {
    status: i64,
    headers: Value,
    body: Vec<u8>,
    fetched_by: String,
    remote_address: String,
}

fn client(timeout_ms: u64) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(timeout_ms))
        .build()?)
}

/// The request snapbot itself makes. A POST carries its exact body; every byte
/// of it is signed, so the capture proves what was sent, not only what came back.
struct Request<'a> {
    method: &'a str,
    body: Option<&'a str>,
}

async fn request_one_hop(target: &str, headers: &Map<String, Value>, timeout_ms: u64, request: &Request<'_>) -> Result<Hop> {
    let u = url::Url::parse(target)?;
    if u.scheme() == "http" {
        // The IPv6 fetch hop is GET-only by design (it holds no key, writes nothing).
        if request.method != "GET" {
            bail!("capture-http-raw sends {} over https only: {target}", request.method);
        }
        return request_via_ipv6_fetch(target, headers, timeout_ms).await;
    }
    if u.scheme() != "https" {
        bail!("capture-http-raw refuses non-http(s) hop {target}");
    }
    let mut req = match (request.method, request.body) {
        ("POST", Some(body)) => client(timeout_ms)?.post(target).body(body.to_string()),
        ("GET", None) => client(timeout_ms)?.get(target),
        (m, _) => bail!("capture-http-raw: unsupported request {m}"),
    };
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str().unwrap_or(""));
    }
    let res = req.send().await.map_err(|e| if e.is_timeout() { anyhow!("timeout after {timeout_ms}ms requesting {target}") } else { anyhow!("{e}") })?;
    let status = res.status().as_u16() as i64;
    let headers = node_headers(res.headers());
    let body = res.bytes().await?.to_vec();
    Ok(Hop { status, headers, body, fetched_by: String::new(), remote_address: String::new() })
}

async fn request_via_ipv6_fetch(target: &str, headers: &Map<String, Value>, timeout_ms: u64) -> Result<Hop> {
    // A session cookie on plain http would cross the internet in cleartext.
    if headers.contains_key("cookie") {
        bail!("capture-http-raw refuses to send cookies over http: {target}");
    }
    let func = std::env::var("EVIDENCE_ATTESTOR_HTTP_FETCH_FUNCTION")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("missing required environment variable EVIDENCE_ATTESTOR_HTTP_FETCH_FUNCTION"))?;
    let payload = json!({"url": target, "headers": headers, "timeout_ms": timeout_ms});
    let out = config::lambda()
        .await
        .invoke()
        .function_name(&func)
        .payload(aws_smithy_types::Blob::new(serde_json::to_vec(&payload)?))
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_lambda::error::DisplayErrorContext(&e)))?;
    let raw = out.payload().map(|b| b.as_ref().to_vec()).unwrap_or_default();
    let reply: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    if out.function_error().is_some() {
        bail!("IPv6 fetch of {target} failed: {}", js::metadata_value(reply.get("errorMessage"), 512));
    }
    use base64::Engine as _;
    Ok(Hop {
        status: reply["status"].as_i64().unwrap_or(0),
        headers: reply.get("headers").cloned().unwrap_or(json!({})),
        body: base64::engine::general_purpose::STANDARD.decode(reply["body_b64"].as_str().unwrap_or("")).unwrap_or_default(),
        fetched_by: func,
        remote_address: format!("[{}]:{}", js::js_string(&reply["remote_address"]), js::js_string(&reply["remote_port"])),
    })
}

pub async fn capture_http_raw(event: &Value) -> Result<Value> {
    let issue = js::metadata_value(js::coalesce(&[event.get("issue"), event.get("issue_number")]).or(Some(&json!("unknown"))), 64);
    let env_name = js::metadata_value(js::coalesce(&[event.get("env"), event.get("environment")]), 64);
    let target_sha = js::metadata_value(js::coalesce(&[event.get("target_sha"), event.get("targetSHA")]), 80);
    let requested = js::metadata_value(event.get("url"), 2048);
    if env_name.is_empty() {
        bail!("capture-http-raw requires env");
    }
    if !(requested.starts_with("https://") || requested.starts_with("http://")) {
        bail!("capture-http-raw requires an http(s) URL");
    }
    let num = |v: Option<&Value>, d: f64| if js::truthy(v) { js::js_number(v) } else { d };
    let timeout_ms = num(event.get("timeout_ms"), 15000.0).min(30000.0).max(1000.0) as u64;
    let max_redirects = match event.get("max_redirects") {
        None | Some(Value::Null) => RAW_MAX_REDIRECTS,
        v => (js::js_number(v).min(RAW_MAX_REDIRECTS as f64).max(0.0)) as i64,
    };
    let cookies = normalize_cookies(event.get("cookies").filter(|v| js::truthy(Some(v))).or_else(|| event.get("session_cookies")), &requested)?;
    let mut req_headers = Map::new();
    req_headers.insert("accept".into(), json!("*/*"));
    req_headers.insert("user-agent".into(), json!("r66-evidence-attestor"));
    if let Some(h) = event.get("headers").and_then(Value::as_object) {
        for (k, v) in h {
            if k.to_lowercase() != "cookie" {
                req_headers.insert(k.clone(), json!(js::js_string(v)));
            }
        }
    }
    if !cookies.is_empty() {
        let c: Vec<String> = cookies
            .iter()
            .map(|c| format!("{}={}", js::js_string(c.get("name").unwrap_or(&Value::Null)), js::js_string(c.get("value").unwrap_or(&Value::Null))))
            .collect();
        req_headers.insert("cookie".into(), json!(c.join("; ")));
    }

    // route66#4344: a POST with a form body (application/x-www-form-urlencoded),
    // made and signed here like every GET. GET stays the default and unchanged.
    let method = match event.get("method").map(js::js_string).unwrap_or_default().to_uppercase().as_str() {
        "" | "GET" => "GET",
        "POST" => "POST",
        other => bail!("capture-http-raw supports GET and POST, got {other}"),
    };
    let form: Vec<(String, String)> = match event.get("form") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(pairs)) => pairs
            .iter()
            .map(|p| match (p.get("name").and_then(Value::as_str), p.get("value").and_then(Value::as_str)) {
                (Some(n), Some(v)) if !n.is_empty() => Ok((n.to_string(), v.to_string())),
                _ => Err(anyhow!("capture-http-raw form entries must be {{\"name\": non-empty string, \"value\": string}}")),
            })
            .collect::<Result<_>>()?,
        Some(_) => bail!("capture-http-raw form must be an array of {{name, value}}"),
    };
    let request_body = if method == "POST" {
        if form.is_empty() {
            bail!("capture-http-raw POST needs a non-empty form");
        }
        // A POST is answered once: following a redirect would re-send or drop
        // the body, and either way the signed request would not be the one made.
        if max_redirects != 0 {
            bail!("capture-http-raw POST records the first answer only; send max_redirects 0");
        }
        if req_headers.keys().any(|k| k.eq_ignore_ascii_case("content-type")) {
            bail!("capture-http-raw POST sets its own content-type; remove the content-type header");
        }
        let mut ser = url::form_urlencoded::Serializer::new(String::new());
        for (n, v) in &form {
            ser.append_pair(n, v);
        }
        let encoded = ser.finish();
        // Signed in full below, so it is bounded rather than truncated.
        if encoded.len() > 2048 {
            bail!("capture-http-raw POST body is {} bytes; at most 2048 are signed", encoded.len());
        }
        req_headers.insert("content-type".into(), json!("application/x-www-form-urlencoded"));
        Some(encoded)
    } else {
        if !form.is_empty() {
            bail!("capture-http-raw form requires method POST");
        }
        None
    };
    let request = Request { method, body: request_body.as_deref() };

    let captured_at = js::now_iso();
    let mut hops: Vec<(String, i64, String, String, String)> = Vec::new();
    let mut current = requested.clone();
    let mut last: Option<(String, Hop)> = None;
    let mut i = 0;
    loop {
        let res = request_one_hop(&current, &req_headers, timeout_ms, &request).await?;
        let location = res.headers.get("location").map(js::js_string).unwrap_or_default();
        let is_redirect = (300..400).contains(&res.status) && !location.is_empty();
        hops.push((current.clone(), res.status, if is_redirect { location.clone() } else { String::new() }, res.fetched_by.clone(), res.remote_address.clone()));
        let next = if is_redirect { Some(url::Url::parse(&current)?.join(&location)?.to_string()) } else { None };
        last = Some((current.clone(), res));
        if !is_redirect || i >= max_redirects {
            break;
        }
        current = next.expect("redirect target");
        i += 1;
    }
    let (final_url, final_res) = last.expect("at least one hop");
    let content_type = js::metadata_str(
        &final_res.headers.get("content-type").map(js::js_string).filter(|s| !s.is_empty()).unwrap_or_else(|| "application/octet-stream".to_string()),
        256,
    );
    let body = final_res.body.clone();
    let key = attest::evidence_key(&env_name, &issue, &js::compact_stamp(&captured_at), &format!("http-raw.{}", raw_body_extension(&content_type)));
    let metadata = [
        ("environment", env_name.clone()),
        ("issue-number", issue.clone()),
        ("evidence-type", "http-raw".to_string()),
        ("captured-by", "command-center evidence-attestor".to_string()),
        ("requested-url", js::metadata_str(&requested, 512)),
        ("final-url", js::metadata_str(&final_url, 512)),
    ];
    let vid = attest::put_evidence(&key, body.clone(), &content_type, &metadata).await?;
    let mut o = Map::new();
    let redirect_count = hops.len() as i64 - 1;
    for (k, v) in [
        ("capture.evidence-type", "http-raw".to_string()),
        ("capture.issue", issue.clone()),
        ("capture.captured-at-utc", captured_at.clone()),
        ("capture.requested-url", requested.clone()),
        ("capture.final-url", final_url.clone()),
        ("capture.http-status", final_res.status.to_string()),
        ("capture.redirect-count", redirect_count.to_string()),
        ("capture.redirect-truncated", (redirect_count >= max_redirects && !hops.last().map(|h| h.2.is_empty()).unwrap_or(true)).to_string()),
        ("capture.body-bytes", body.len().to_string()),
        ("capture.body-sha256", attest::sha256_hex(&body)),
        ("capture.request-timeout-ms", timeout_ms.to_string()),
        ("capture.cookie-fingerprint-sha256", cookie_fingerprint(&cookies)),
        ("capture.cookie-count", cookies.len().to_string()),
        ("capture.request-method", method.to_string()),
    ] {
        o.insert(k.to_string(), json!(v));
    }
    // The exact bytes sent (bounded to 2048 above, never truncated) and their
    // digest, so a verifier sees which form produced the signed answer.
    if let Some(sent) = &request_body {
        o.insert("capture.request-content-type".into(), json!("application/x-www-form-urlencoded"));
        o.insert("capture.request-body".into(), json!(sent));
        o.insert("capture.request-body-sha256".into(), json!(attest::sha256_hex(sent.as_bytes())));
    }
    // Zero-padded so the manifest's key sort is also hop order.
    for (i, (u, status, location, fetched_by, remote)) in hops.iter().enumerate() {
        let p = format!("capture.redirect.{i:02}.");
        o.insert(format!("{p}url"), json!(js::metadata_str(u, 512)));
        o.insert(format!("{p}status"), json!(status.to_string()));
        o.insert(format!("{p}location"), json!(js::metadata_str(location, 512)));
        if !fetched_by.is_empty() {
            o.insert(format!("{p}fetched-by"), json!(js::metadata_str(fetched_by, 256)));
            o.insert(format!("{p}remote-address"), json!(js::metadata_str(remote, 128)));
        }
    }
    header_observed(&final_res.headers, &mut o);
    if !target_sha.is_empty() {
        for (k, v) in ci::capture_dev_ci(&env_name, &target_sha).await {
            o.insert(k, v);
        }
    }
    crate::handler::add_attestation_context(&mut o, event);
    attest::attest(&key, &vid, o).await
}

/// The fetch-hop Lambda: one plain-http GET, no redirects, bytes as served,
/// plus the IPv6 peer it actually connected to.
pub async fn fetch_hop(event: &Value) -> Result<Value> {
    let raw = event.get("url").map(js::js_string).unwrap_or_default();
    let u = url::Url::parse(&raw).map_err(|_| anyhow!("Invalid URL"))?;
    if u.scheme() != "http" {
        bail!("fetch-hop serves http:// hops only, got {}:", u.scheme());
    }
    let timeout_ms = (if js::truthy(event.get("timeout_ms")) { js::js_number(event.get("timeout_ms")) } else { 15000.0 }).min(30000.0).max(1000.0) as u64;
    let mut req = client(timeout_ms)?.get(u.as_str());
    if let Some(h) = event.get("headers").and_then(Value::as_object) {
        for (k, v) in h {
            req = req.header(k.as_str(), js::js_string(v));
        }
    }
    let res = req.send().await.map_err(|e| if e.is_timeout() { anyhow!("timeout after {timeout_ms}ms requesting {u}") } else { anyhow!("{e}") })?;
    let remote = res.remote_addr();
    let status = res.status().as_u16();
    let headers = node_headers(res.headers());
    let body = res.bytes().await?;
    Ok(json!({
        "status": status,
        "headers": headers,
        "body_b64": attest::b64(&body),
        "remote_address": remote.map(|a| a.ip().to_string()).unwrap_or_default(),
        "remote_port": remote.map(|a| a.port()).unwrap_or(0),
    }))
}
