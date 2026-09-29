//! capture-web-ui-screenshot: the handler drives Chromium itself, so every
//! capture fact in `observed` is first-hand, including every PERTURBATION it
//! applied (blocked subresources, clicks, pauses) -- an induced page state is
//! declared evidence or it is fabrication.
//!
//! The navigating-click handling, the phase markers and every bound are the
//! Node handler's, measured against the 2026-09-05 timeout incidents (GH #3177,
//! RequestIds 9656ca79 and 3fef470d): a click is issued, never awaited bare;
//! an unsettled click promise at the end of the 400ms probe IS the navigation
//! signal; the shutter is bounded at 30s.

use crate::attest;
use crate::browser::{Intercept, Page};
use crate::{ci, handler::phase, js};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const MAX_CLICK_SELECTORS: usize = 8;
const MAX_STEPS: usize = 12;
const MAX_STEP_WAIT_MS: i64 = 5000;

pub fn normalize_cookies(raw: Option<&Value>, url: &str) -> Result<Vec<Map<String, Value>>> {
    match raw {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => Ok(a
            .iter()
            .filter_map(|c| c.as_object())
            .map(|c| {
                let mut m = Map::new();
                m.insert("url".into(), json!(url));
                for (k, v) in c {
                    m.insert(k.clone(), v.clone());
                }
                m
            })
            .filter(|c| js::truthy(c.get("name")) && c.get("value").is_some_and(|v| !v.is_null()))
            .collect()),
        Some(Value::Object(o)) => Ok(o
            .iter()
            .map(|(k, v)| {
                let mut m = Map::new();
                m.insert("name".into(), json!(k));
                m.insert("value".into(), json!(js::js_string(v)));
                m.insert("url".into(), json!(url));
                m
            })
            .collect()),
        Some(Value::String(s)) if s.is_empty() => Ok(Vec::new()),
        Some(Value::Bool(false)) => Ok(Vec::new()),
        _ => bail!("cookies must be an array of cookie objects or a name/value object"),
    }
}

/// sha256 over the redacted cookie shape (names and attributes, never values).
pub fn cookie_fingerprint(cookies: &[Map<String, Value>]) -> String {
    let s = |c: &Map<String, Value>, k: &str| match c.get(k) {
        Some(v) if js::truthy(Some(v)) => v.clone(),
        _ => json!(""),
    };
    let mut redacted: Vec<Value> = cookies
        .iter()
        .map(|c| {
            json!({"name": c.get("name").cloned().unwrap_or(Value::Null), "domain": s(c, "domain"), "path": s(c, "path"),
                   "secure": js::truthy(c.get("secure")), "httpOnly": js::truthy(c.get("httpOnly")), "sameSite": s(c, "sameSite")})
        })
        .collect();
    // JS sorted with String.localeCompare; case-insensitive order with a
    // byte-order tie-break matches it for the ASCII cookie names in use.
    redacted.sort_by(|a, b| {
        let (sa, sb) = (a.to_string(), b.to_string());
        sa.to_lowercase().cmp(&sb.to_lowercase()).then(sa.cmp(&sb))
    });
    attest::sha256_hex(serde_json::to_string(&redacted).unwrap_or_default().as_bytes())
}

fn parse_click_selectors(raw: Option<&Value>) -> Result<Vec<String>> {
    let arr = match raw {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(a)) => a,
        _ => bail!("click_selectors must be an array of CSS selector strings"),
    };
    if arr.len() > MAX_CLICK_SELECTORS {
        bail!("click_selectors accepts at most {MAX_CLICK_SELECTORS} selectors; got {}", arr.len());
    }
    arr.iter()
        .enumerate()
        .map(|(i, s)| match s.as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(t) => Ok(js::metadata_str(t, 256)),
            None => bail!("click_selectors[{i}] must be a non-empty CSS selector string"),
        })
        .collect()
}

#[derive(Clone)]
enum Interaction {
    Click(String),
    Wait(i64),
}

fn parse_steps(raw: Option<&Value>) -> Result<Option<Vec<Interaction>>> {
    let arr = match raw {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => bail!("steps must be a non-empty array of {{click: selector}} / {{wait_ms: N}} objects"),
    };
    if arr.len() > MAX_STEPS {
        bail!("steps accepts at most {MAX_STEPS} entries; got {}", arr.len());
    }
    let mut out = Vec::new();
    for (i, s) in arr.iter().enumerate() {
        let o = s.as_object().filter(|o| o.len() == 1);
        let Some(o) = o else { bail!("steps[{i}] must be an object with exactly one of click, wait_ms") };
        let (k, v) = o.iter().next().expect("one key");
        match k.as_str() {
            "click" => {
                let sel = v.as_str().map(str::trim).filter(|s| !s.is_empty());
                let Some(sel) = sel else { bail!("steps[{i}].click must be a non-empty CSS selector string") };
                if sel.contains('|') {
                    bail!("steps[{i}].click must not contain '|' (capture.steps separator)");
                }
                out.push(Interaction::Click(js::metadata_str(sel, 256)));
            }
            "wait_ms" => {
                let n = v.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= MAX_STEP_WAIT_MS as f64);
                let Some(n) = n else { bail!("steps[{i}].wait_ms must be an integer 0..{MAX_STEP_WAIT_MS}") };
                out.push(Interaction::Wait(n as i64));
            }
            other => bail!("steps[{i}] has unsupported key {}; allowed: click, wait_ms", js::json_stringify(Some(&json!(other)))),
        }
    }
    Ok(Some(out))
}

/// Walk the document in viewport steps so lazy content renders, then return
/// to the top. Bounded at 40 steps; a failed walk still gets its screenshot.
async fn scroll_through(page: &Page) {
    let _ = page
        .evaluate(
            "(async () => { const step = window.innerHeight; const settle = () => new Promise((r) => setTimeout(r, 120));
               for (let y = 0, steps = 0; steps < 40; steps++) { y += step; if (y > document.body.scrollHeight) break; window.scrollTo(0, y); await settle(); }
               window.scrollTo(0, 0); await settle(); })()",
        )
        .await;
}

/// The full PerformanceNavigationTiming breakdown; -1 marks an unavailable mark.
async fn navigation_timing(page: &Page) -> Value {
    let unavailable = json!({"redirect": -1, "dns": -1, "tcp": -1, "tls": -1, "request": -1, "response": -1, "ttfb": -1,
        "domInteractive": -1, "domContentLoaded": -1, "domComplete": -1, "load": -1, "transferBytes": -1, "encodedBytes": -1,
        "decodedBytes": -1, "protocol": "", "redirectCount": -1});
    let v = page
        .evaluate(
            "(() => { const nav = performance.getEntriesByType('navigation')[0]; if (!nav) return null;
               const span = (a, b) => (a > 0 && b > 0 ? Math.round(b - a) : -1); const at = (t) => (t > 0 ? Math.round(t) : -1);
               return { redirect: span(nav.redirectStart, nav.redirectEnd), dns: span(nav.domainLookupStart, nav.domainLookupEnd),
                 tcp: span(nav.connectStart, nav.connectEnd), tls: span(nav.secureConnectionStart, nav.connectEnd),
                 request: span(nav.requestStart, nav.responseStart), response: span(nav.responseStart, nav.responseEnd),
                 ttfb: span(nav.requestStart, nav.responseStart), domInteractive: at(nav.domInteractive),
                 domContentLoaded: at(nav.domContentLoadedEventEnd), domComplete: at(nav.domComplete), load: at(nav.loadEventEnd),
                 transferBytes: Number.isFinite(nav.transferSize) ? nav.transferSize : -1,
                 encodedBytes: Number.isFinite(nav.encodedBodySize) ? nav.encodedBodySize : -1,
                 decodedBytes: Number.isFinite(nav.decodedBodySize) ? nav.decodedBodySize : -1,
                 protocol: String(nav.nextHopProtocol || ''), redirectCount: Number.isFinite(nav.redirectCount) ? nav.redirectCount : -1 }; })()",
        )
        .await;
    match v {
        Ok(Some(v)) if v.is_object() => v,
        _ => unavailable,
    }
}

fn s(v: &Value) -> String {
    js::js_string(v)
}

pub fn timing_observed(t: &Value, out: &mut Map<String, Value>) {
    for (k, f) in [
        ("capture.timing.redirect-ms", "redirect"),
        ("capture.timing.dns-ms", "dns"),
        ("capture.timing.tcp-ms", "tcp"),
        ("capture.timing.tls-ms", "tls"),
        ("capture.timing.request-ms", "request"),
        ("capture.timing.response-ms", "response"),
        ("capture.timing.dom-interactive-ms", "domInteractive"),
        ("capture.timing.dom-complete-ms", "domComplete"),
        ("capture.timing.redirect-count", "redirectCount"),
        ("capture.timing.protocol", "protocol"),
        ("capture.timing.transfer-bytes", "transferBytes"),
        ("capture.timing.encoded-body-bytes", "encodedBytes"),
        ("capture.timing.decoded-body-bytes", "decodedBytes"),
    ] {
        out.insert(k.into(), json!(s(&t[f])));
    }
}

/// The allowlist a reader adjudicates evidence with. set-cookie and
/// authorization are absent BY CONSTRUCTION: the bucket is world-readable.
const RESPONSE_HEADER_ALLOWLIST: [&str; 18] = [
    "age", "cache-control", "content-encoding", "content-length", "content-type", "date", "etag", "expires", "last-modified",
    "location", "server", "vary", "via", "x-amz-cf-id", "x-amz-cf-pop", "x-cache", "x-content-type-options", "x-frame-options",
];

pub fn header_observed(headers: &Value, out: &mut Map<String, Value>) {
    let mut lower = Map::new();
    if let Some(o) = headers.as_object() {
        for (k, v) in o {
            lower.insert(k.to_lowercase(), v.clone());
        }
    }
    for name in RESPONSE_HEADER_ALLOWLIST {
        if let Some(v) = lower.get(name) {
            out.insert(format!("capture.header.{name}"), json!(js::metadata_value(Some(v), 512)));
        }
    }
    out.insert("capture.header-count".into(), json!(lower.len().to_string()));
}

struct Captured {
    bytes: Vec<u8>,
    full_page: bool,
    width: i64,
    height: i64,
    requested_url: String,
    final_url: String,
    status: i64,
    viewport: String,
    viewport_height: i64,
    fingerprint: String,
    cookie_count: usize,
    blocked: Vec<String>,
    clicks: Vec<String>,
    steps: Vec<Interaction>,
    headers: Value,
    body_sha256: String,
    body_bytes: i64,
    timing: Value,
}

fn clamp_num(v: Option<&Value>, default: f64, lo: f64, hi: f64) -> Result<i64> {
    let n = if js::truthy(v) { js::js_number(v) } else { default };
    if n.is_nan() {
        bail!("invalid numeric capture option");
    }
    Ok(n.min(hi).max(lo) as i64)
}

async fn capture_png(event: &Value) -> Result<Captured> {
    let url = js::metadata_value(event.get("url"), 2048);
    if !url.starts_with("https://") {
        bail!("capture-web-ui-screenshot requires an https URL");
    }
    let or = |a: &str, b: &str| event.get(a).filter(|v| js::truthy(Some(v))).or_else(|| event.get(b));
    let width = clamp_num(or("viewport_width", "width"), 1365.0, 320.0, 3840.0)?;
    let height = clamp_num(or("viewport_height", "height"), 900.0, 320.0, 3000.0)?;
    let timeout_ms = clamp_num(event.get("timeout_ms"), 20000.0, 5000.0, 45000.0)? as u64;
    let wait_ms = clamp_num(event.get("wait_ms"), 1000.0, 0.0, 10000.0)? as u64;
    let cookies = normalize_cookies(or("cookies", "session_cookies"), &url)?;
    // FULL PAGE BY DEFAULT (owner 2026-09-03).
    let full_page = match event.get("full_page") {
        None | Some(Value::Null) => true,
        v => js::truthy(v),
    };
    let blocked: Vec<String> = event
        .get("block_url_patterns")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|p| js::metadata_str(&s(p), 256)).filter(|p| !p.is_empty()).take(16).collect())
        .unwrap_or_default();
    let clicks = parse_click_selectors(event.get("click_selectors"))?;
    let parsed = parse_steps(event.get("steps"))?;
    if parsed.is_some() && !clicks.is_empty() {
        bail!("steps and click_selectors are mutually exclusive; put the clicks inside steps");
    }
    let interactions = parsed.unwrap_or_else(|| clicks.iter().cloned().map(Interaction::Click).collect());

    // The same pooled page every browse request uses: one browser path.
    let (mut c, _) = crate::browse::with_page(&[], |page, _| {
        let (url, cookies, blocked, interactions) = (url.clone(), cookies.clone(), blocked.clone(), interactions.clone());
        async move { capture_with(page, &url, width, height, timeout_ms, wait_ms, &cookies, full_page, &blocked, &interactions, event).await }
    })
    .await?;
    c.clicks = interactions.iter().filter_map(|i| if let Interaction::Click(s) = i { Some(s.clone()) } else { None }).collect();
    c.fingerprint = cookie_fingerprint(&cookies);
    c.cookie_count = cookies.len();
    c.blocked = blocked;
    c.steps = interactions;
    c.requested_url = url;
    Ok(c)
}

#[allow(clippy::too_many_arguments)]
async fn capture_with(
    page: Arc<Page>,
    url: &str,
    width: i64,
    height: i64,
    timeout_ms: u64,
    wait_ms: u64,
    cookies: &[Map<String, Value>],
    full_page: bool,
    blocked: &[String],
    interactions: &[Interaction],
    event: &Value,
) -> Result<Captured> {
    page.set_viewport(width, height).await?;
    if let Some(h) = event.get("headers").and_then(Value::as_object) {
        let headers: Map<String, Value> =
            h.iter().filter(|(k, _)| k.to_lowercase() != "cookie").map(|(k, v)| (k.clone(), json!(s(v)))).collect();
        page.send("Network.setExtraHTTPHeaders", json!({"headers": headers})).await?;
    }
    if !cookies.is_empty() {
        let list: Vec<Value> = cookies.iter().cloned().map(Value::Object).collect();
        page.set_cookies(&list).await?;
    }
    if !blocked.is_empty() {
        page.set_intercept(Intercept::Block { patterns: blocked.to_vec() }).await?;
    }
    let response = page.goto(url, "networkidle2", timeout_ms).await?;
    phase("goto", &format!("status={}", response.as_ref().map(|r| r.resp.status).unwrap_or(0)));
    // Body and headers describe the served document, read before any click.
    let (body_sha256, body_bytes) = match &response {
        Some(r) => match page.response_text(&r.resp.request_id).await {
            Ok(body) => (attest::sha256_hex(body.as_bytes()), body.len() as i64),
            Err(e) => (format!("unavailable: {e}"), -1),
        },
        None => (attest::sha256_hex(b""), 0),
    };
    let headers = response.as_ref().map(|r| r.headers.clone()).unwrap_or(json!({}));
    let timing = navigation_timing(&page).await;
    phase("body+headers+timing", &format!("body-bytes={body_bytes}"));
    if full_page {
        scroll_through(&page).await;
        phase("scroll", "full-page=true");
    }
    for step in interactions {
        let sel = match step {
            Interaction::Wait(ms) => {
                tokio::time::sleep(Duration::from_millis(*ms as u64)).await;
                phase("step-wait", &format!("wait-ms={ms}"));
                continue;
            }
            Interaction::Click(sel) => sel.clone(),
        };
        page.wait_for_selector(&sel, "visible", 5000).await?;
        phase("click-selector-ready", &sel);
        let url_before = page.url();
        // Armed BEFORE the click; "did not navigate" is the common outcome.
        let nav_page = page.clone();
        let nav = tokio::spawn(async move { nav_page.wait_for_navigation("networkidle2", 8000).await.ok() });
        let settled = Arc::new(AtomicBool::new(false));
        let (click_page, click_sel, flag) = (page.clone(), sel.clone(), settled.clone());
        let mut click = tokio::spawn(async move {
            let _ = click_page.click_selector(&click_sel).await;
            flag.store(true, Ordering::SeqCst);
        });
        phase("click-issued", &sel);
        let mut nav = nav;
        // Each handle is polled to completion at most once.
        let (mut click_done, mut nav_done) = (false, false);
        tokio::select! {
            _ = &mut click => { click_done = true; }
            _ = &mut nav => { nav_done = true; }
            _ = tokio::time::sleep(Duration::from_millis(400)) => {}
        }
        let navigated = page.url() != url_before || !settled.load(Ordering::SeqCst);
        if navigated {
            if !nav_done {
                let _ = (&mut nav).await;
            }
            let _ = page.wait_for_network_idle(500, 5000).await;
        } else if !nav_done {
            nav.abort();
        }
        if !click_done {
            let _ = tokio::time::timeout(Duration::from_millis(1000), &mut click).await;
        }
        let landed = page.url();
        let logged = if landed.contains('?') { url::Url::parse(&landed).map(|u| u.path().to_string()).unwrap_or(landed.clone()) } else { landed };
        phase("click", &format!("{sel} navigated={navigated} click-settled={} url={logged}", settled.load(Ordering::SeqCst)));
    }
    if wait_ms > 0 {
        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
    }
    phase("settle", &format!("wait-ms={wait_ms}"));
    phase("screenshot-start", &format!("full-page={full_page}"));
    let bytes = tokio::time::timeout(Duration::from_secs(30), page.screenshot(full_page))
        .await
        .map_err(|_| anyhow!("screenshot exceeded 30s (phase=screenshot)"))??;
    phase("screenshot", &format!("png-bytes={} full-page={full_page}", bytes.len()));
    let (w, h) = if bytes.len() > 24 {
        (u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as i64, u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as i64)
    } else {
        (-1, -1)
    };
    Ok(Captured {
        bytes,
        full_page,
        width: w,
        height: h,
        requested_url: String::new(),
        final_url: page.url(),
        status: response.as_ref().map(|r| r.resp.status).unwrap_or(0),
        viewport: format!("{width}x{height}"),
        viewport_height: height,
        fingerprint: String::new(),
        cookie_count: 0,
        blocked: Vec::new(),
        clicks: Vec::new(),
        steps: Vec::new(),
        headers,
        body_sha256,
        body_bytes,
        timing,
    })
}

pub async fn capture_web_ui_screenshot(event: &Value) -> Result<Value> {
    let issue = js::metadata_value(js::coalesce(&[event.get("issue"), event.get("issue_number")]).or(Some(&json!("unknown"))), 64);
    let env_name = js::metadata_value(js::coalesce(&[event.get("env"), event.get("environment")]), 64);
    let target_sha = js::metadata_value(js::coalesce(&[event.get("target_sha"), event.get("targetSHA")]), 80);
    if env_name.is_empty() {
        bail!("capture-web-ui-screenshot requires env");
    }
    if target_sha.is_empty() {
        bail!("capture-web-ui-screenshot requires target_sha");
    }
    let ci = ci::capture_dev_ci(&env_name, &target_sha).await;
    let c = capture_png(event).await?;
    let captured_at = js::now_iso();
    let key = attest::evidence_key(&env_name, &issue, &js::compact_stamp(&captured_at), "web-ui-screenshot.png");
    let metadata = [
        ("environment", env_name.clone()),
        ("target-sha", target_sha.clone()),
        ("issue-number", issue.clone()),
        ("evidence-type", "web-ui-screenshot".to_string()),
        ("captured-by", "command-center evidence-attestor".to_string()),
        ("requested-url", js::metadata_str(&c.requested_url, 512)),
        ("final-url", js::metadata_str(&c.final_url, 512)),
    ];
    phase("s3-put", &format!("png-bytes={}", c.bytes.len()));
    let vid = attest::put_evidence(&key, c.bytes.clone(), "image/png", &metadata).await?;
    let mut o = Map::new();
    let mut put = |k: &str, v: String| {
        o.insert(k.to_string(), json!(v));
    };
    put("capture.evidence-type", "web-ui-screenshot".into());
    put("capture.issue", issue.clone());
    put("capture.captured-at-utc", captured_at.clone());
    put("capture.requested-url", c.requested_url.clone());
    put("capture.final-url", c.final_url.clone());
    put("capture.http-status", c.status.to_string());
    put("capture.viewport", c.viewport.clone());
    put("capture.full-page", c.full_page.to_string());
    put("capture.image-width-px", c.width.to_string());
    put("capture.image-height-px", c.height.to_string());
    put(
        "capture.image-screens-tall",
        if c.height > 0 && c.viewport_height > 0 { format!("{:.2}", c.height as f64 / c.viewport_height as f64) } else { "-1".into() },
    );
    put("capture.cookie-fingerprint-sha256", c.fingerprint.clone());
    put("capture.cookie-count", c.cookie_count.to_string());
    put("capture.blocked-url-patterns", if c.blocked.is_empty() { "none".into() } else { c.blocked.join(",") });
    put("capture.click-selectors", if c.clicks.is_empty() { "none".into() } else { c.clicks.join("|") });
    let steps: Vec<String> = c
        .steps
        .iter()
        .map(|s| match s {
            Interaction::Wait(ms) => format!("wait_ms:{ms}"),
            Interaction::Click(sel) => format!("click:{sel}"),
        })
        .collect();
    put("capture.steps", if steps.is_empty() { "none".into() } else { steps.join("|") });
    put("capture.ttfb-ms", s(&c.timing["ttfb"]));
    put("capture.dom-content-loaded-ms", s(&c.timing["domContentLoaded"]));
    put("capture.load-event-ms", s(&c.timing["load"]));
    put("capture.body-sha256", c.body_sha256.clone());
    put("capture.body-bytes", c.body_bytes.to_string());
    timing_observed(&c.timing, &mut o);
    header_observed(&c.headers, &mut o);
    for (k, v) in ci {
        o.insert(k, v);
    }
    crate::handler::add_attestation_context(&mut o, event);
    phase("attest-and-sign", &format!("key={key}"));
    attest::attest(&key, &vid, o).await
}
