//! browse -- ONE complete browsing request per Lambda invocation.
//!
//! Owner directives 2026-09-26: "snapbot daemon should host the playwright
//! browser sessions exclusively", "snapbot receives a complete browsing request
//! as a lambda call" and "delegate to snapbot which can stay up as a permanent
//! pool along with kumo". The browser is launched once per process lifetime and
//! a fresh context+page is PRE-CREATED while the process is idle, so a request
//! pays only its own navigation and steps.
//!
//! ONE REQUEST AT A TIME PER PROCESS (the Lambda contract). A spare context is
//! built only after the previous request's context is closed, so no context is
//! driving pages while the next one is created (route66 GH #4040: doing so
//! hangs the browser).
//!
//! Snapbot captures; the caller judges. The contract (request and result
//! shapes, the op set, if_ok/if_failed, clocks) is the former browse.js's.

use crate::actions::nav_json;
use crate::browser::{self, Browser, Fulfill, Intercept, LaunchOptions, Page};
use crate::{js, ocr, store};
use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const MAX_STEPS: usize = 200;
const MAX_STEP_TIMEOUT_MS: f64 = 120000.0;
const SCREENSHOT_TIMEOUT_MS: u64 = 30000;

const OPS: [&str; 21] = [
    "goto", "wait_for_function", "evaluate", "click", "fill", "check", "press", "type", "wait_for_selector", "wait_for_url",
    "wait_for_navigation", "text", "attribute", "count", "content", "url", "cookies", "set_cookies", "screenshot", "sleep",
    "start_clock",
];

struct Spare {
    context: String,
    page: Page,
}

#[derive(Default)]
struct PoolState {
    browser: Option<Browser>,
    args_key: Option<String>,
    spare: Option<Spare>,
    building: Option<JoinHandle<Result<Spare>>>,
    launches: u64,
    served: u64,
}

fn pool() -> &'static Mutex<PoolState> {
    static POOL: OnceLock<Mutex<PoolState>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(PoolState::default()))
}

static BUSY: AtomicBool = AtomicBool::new(false);

/// browser_args: only "--flag" strings.
fn parse_browser_args(v: Option<&Value>) -> Result<Vec<String>> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) if a.iter().all(|x| x.as_str().is_some_and(|s| s.starts_with("--"))) => {
            Ok(a.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect())
        }
        _ => bail!("browser_args must be an array of --flag strings"),
    }
}

/// Keep exactly one browser per process; a request carrying different launch
/// args relaunches once and keeps that browser.
async fn ensure_browser(st: &mut PoolState, extra: &[String]) -> Result<()> {
    let key = serde_json::to_string(extra)?;
    if let Some(b) = &st.browser {
        if st.args_key.as_deref() == Some(key.as_str()) && b.connected() {
            return Ok(());
        }
        if b.connected() {
            println!("[browse] relaunching browser: launch args changed ({} -> {key})", st.args_key.clone().unwrap_or_default());
        } else {
            eprintln!("[browse] browser disconnected; relaunching it");
        }
        if let Some(t) = st.building.take() {
            t.abort();
        }
        st.spare = None;
        if let Some(old) = st.browser.take() {
            old.close().await;
        }
    }
    let started = Instant::now();
    let b = Browser::launch(LaunchOptions { single_process: false, ignore_https_errors: true, extra_args: extra.to_vec() }).await?;
    st.browser = Some(b);
    st.args_key = Some(key.clone());
    st.launches += 1;
    println!("[browse] browser launched in {}ms args={key}", started.elapsed().as_millis());
    Ok(())
}

async fn build_spare(cdp: Arc<crate::cdp::Cdp>) -> Result<Spare> {
    let context = browser::create_context(&cdp).await?;
    let page = browser::new_page(cdp, Some(&context)).await?;
    Ok(Spare { context, page })
}

fn schedule_spare(st: &mut PoolState) {
    if let Some(b) = &st.browser {
        let cdp = b.cdp.clone();
        st.building = Some(tokio::spawn(build_spare(cdp)));
    }
}

async fn take_spare(st: &mut PoolState) -> Result<Spare> {
    if st.spare.is_none() {
        if let Some(t) = st.building.take() {
            match t.await {
                Ok(Ok(s)) => st.spare = Some(s),
                Ok(Err(e)) => eprintln!("[browse] spare context build failed: {e:#}"),
                Err(e) => eprintln!("[browse] spare context build failed: {e}"),
            }
        }
    }
    if let Some(s) = st.spare.take() {
        return Ok(s);
    }
    let cdp = st.browser.as_ref().ok_or_else(|| anyhow!("no browser"))?.cdp.clone();
    build_spare(cdp).await.map_err(|e| anyhow!("no browser context could be built for this request: {e:#}"))
}

/// Launch the browser and build the first spare before the lane polls, so no
/// request ever lands on a cold process.
pub async fn warm(extra: &Value) -> Result<()> {
    let extra = parse_browser_args(Some(extra))?;
    let mut st = pool().lock().await;
    ensure_browser(&mut st, &extra).await?;
    let spare = take_spare(&mut st).await.map_err(|e| anyhow!("warm: first spare context could not be built: {e:#}"))?;
    st.spare = Some(spare);
    Ok(())
}

struct Ctx {
    viewport: (i64, i64),
    intercept: Option<Intercept>,
    init_scripts: Vec<String>,
    cookies: Vec<Value>,
    default_timeout: Option<u64>,
    navigation_timeout: Option<u64>,
}

fn compile_regex(v: Option<&Value>, what: &str) -> Result<Regex> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Regex::new(s).map_err(|e| anyhow!("{what}: invalid regular expression: {e}")),
        _ => bail!("{what} must be a non-empty regex string"),
    }
}

fn finite_ms(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_f64).filter(|f| f.is_finite()).map(|f| f.max(0.0) as u64)
}

fn parse_context(raw: Option<&Value>) -> Result<Ctx> {
    let empty = json!({});
    let c = match raw {
        None | Some(Value::Null) => &empty,
        Some(v) => v,
    };
    let viewport = match c.get("viewport").filter(|v| js::truthy(Some(v))) {
        None => (1366, 900),
        Some(v) => {
            let int = |k: &str| v.get(k).and_then(Value::as_f64).filter(|f| f.fract() == 0.0).map(|f| f as i64);
            match (int("width"), int("height")) {
                (Some(w), Some(h)) => (w, h),
                _ => bail!("context.viewport needs integer width/height"),
            }
        }
    };
    if c.get("ignore_https_errors") == Some(&Value::Bool(false)) {
        bail!("context.ignore_https_errors=false is unsupported: the pool browser accepts insecure certs");
    }
    let arr = |k: &str| c.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut header_rules = Vec::new();
    for (i, r) in arr("extra_headers").iter().enumerate() {
        let host = r.get("host").and_then(Value::as_str).filter(|h| !h.is_empty());
        let headers = r.get("headers").and_then(Value::as_object);
        match (host, headers) {
            (Some(h), Some(hd)) => header_rules.push((h.to_lowercase(), hd.clone())),
            _ => bail!("context.extra_headers[{i}] needs host and headers"),
        }
    }
    let mut fulfill = Vec::new();
    for (i, r) in arr("fulfill").iter().enumerate() {
        fulfill.push(Fulfill {
            re: compile_regex(r.get("url_regex"), &format!("context.fulfill[{i}].url_regex"))?,
            status: r.get("status").and_then(Value::as_f64).filter(|f| *f != 0.0).map(|f| f as i64).unwrap_or(200),
            content_type: r.get("content_type").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("application/octet-stream").to_string(),
            body: base64::engine::general_purpose::STANDARD.decode(r.get("body_b64").and_then(Value::as_str).unwrap_or("")).unwrap_or_default(),
        });
    }
    let mut abort = Vec::new();
    for (i, s) in arr("abort").iter().enumerate() {
        abort.push(compile_regex(Some(s), &format!("context.abort[{i}]"))?);
    }
    let mut init_scripts = Vec::new();
    for (i, s) in arr("init_scripts").iter().enumerate() {
        init_scripts.push(s.as_str().ok_or_else(|| anyhow!("context.init_scripts[{i}] must be a string"))?.to_string());
    }
    let intercept =
        (!abort.is_empty() || !fulfill.is_empty() || !header_rules.is_empty()).then_some(Intercept::Browse { abort, fulfill, header_rules });
    Ok(Ctx {
        viewport,
        intercept,
        init_scripts,
        cookies: arr("cookies"),
        default_timeout: finite_ms(c.get("default_timeout_ms")),
        navigation_timeout: finite_ms(c.get("navigation_timeout_ms")),
    })
}

struct Step {
    id: String,
    op: String,
    raw: Map<String, Value>,
}

impl Step {
    fn get(&self, k: &str) -> Option<&Value> {
        self.raw.get(k).filter(|v| !v.is_null())
    }
    fn str(&self, k: &str) -> Option<&str> {
        self.raw.get(k).and_then(Value::as_str)
    }
    fn optional(&self) -> bool {
        js::truthy(self.raw.get("optional"))
    }
}

fn parse_steps(raw: Option<&Value>) -> Result<Vec<Step>> {
    let arr = match raw {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => bail!("steps must be a non-empty array"),
    };
    if arr.len() > MAX_STEPS {
        bail!("steps accepts at most {MAX_STEPS}; got {}", arr.len());
    }
    let mut ids = HashSet::new();
    let mut out = Vec::new();
    for (i, s) in arr.iter().enumerate() {
        let op = s.get("op").and_then(Value::as_str).filter(|o| OPS.contains(o));
        let Some(op) = op else {
            bail!("steps[{i}].op {} is not one of {}", js::json_stringify(s.get("op")), OPS.join(","));
        };
        let raw = s.as_object().cloned().unwrap_or_default();
        let id = match raw.get("id") {
            Some(v) if js::truthy(Some(v)) => js::js_string(v),
            _ => format!("{i}:{op}"),
        };
        if ids.contains(&id) {
            bail!("steps[{i}].id {id} is duplicated");
        }
        ids.insert(id.clone());
        for r in ["if_ok", "if_failed"] {
            if let Some(v) = raw.get(r) {
                if !ids.contains(&js::js_string(v)) {
                    bail!("steps[{i}].{r} names {}, which is not an EARLIER step id", js::js_string(v));
                }
            }
        }
        if let Some(t) = raw.get("timeout_ms") {
            let ok = t.as_f64().is_some_and(|f| f.is_finite() && (0.0..=MAX_STEP_TIMEOUT_MS).contains(&f));
            if !ok {
                bail!("steps[{i}].timeout_ms must be 0..{MAX_STEP_TIMEOUT_MS}");
            }
        }
        out.push(Step { id, op: op.to_string(), raw });
    }
    Ok(out)
}

/// `(${fn})(${JSON.stringify(arg)})` for a JavaScript function source.
fn call_expr(f: Option<&Value>, arg: Option<&Value>) -> Result<String> {
    static FN: OnceLock<Regex> = OnceLock::new();
    let re = FN.get_or_init(|| Regex::new(r"^\s*(async\s*)?(\(|function\b|[A-Za-z_$][\w$]*\s*=>)").unwrap());
    let src = f.and_then(Value::as_str).filter(|s| re.is_match(s)).ok_or_else(|| anyhow!("fn must be a JavaScript function source"))?;
    let a = match arg {
        None => String::new(),
        Some(v) => serde_json::to_string(v)?,
    };
    Ok(format!("({src})({a})"))
}

/// Value assertions evaluated here so the verdict travels with the value.
fn check_expect(expect: Option<&Value>, value: Option<&Value>) -> Option<String> {
    let e = expect?.as_object()?;
    let vs = js::json_stringify(value);
    if let Some(eq) = e.get("equals") {
        let es = serde_json::to_string(eq).unwrap_or_default();
        if vs != es {
            return Some(format!("expected {es}, got {vs}"));
        }
    }
    let num = js::js_number(value);
    if let Some(g) = e.get("gte") {
        if !(num >= js::js_number(Some(g))) {
            return Some(format!("expected >= {}, got {vs}", js::js_string(g)));
        }
    }
    if let Some(l) = e.get("lte") {
        if !(num <= js::js_number(Some(l))) {
            return Some(format!("expected <= {}, got {vs}", js::js_string(l)));
        }
    }
    if let Some(c) = e.get("contains") {
        let needle = c.as_str().map(str::to_string).unwrap_or_else(|| js::js_string(c));
        if !value.and_then(Value::as_str).is_some_and(|s| s.contains(&needle)) {
            return Some(format!("expected to contain {}", serde_json::to_string(c).unwrap_or_default()));
        }
    }
    if let Some(t) = e.get("truthy") {
        if js::truthy(value) != js::truthy(Some(t)) {
            return Some(format!("expected truthy={}, got {vs}", js::js_string(t)));
        }
    }
    None
}

fn safe_name(v: Option<&Value>) -> Result<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]{1,200}$").unwrap());
    match v.and_then(Value::as_str) {
        Some(s) if re.is_match(s) => Ok(s.to_string()),
        _ => bail!("screenshot name {} must match [A-Za-z0-9._-]{{1,200}}", js::json_stringify(v)),
    }
}

struct RunCtx<'a> {
    event: &'a Value,
    clocks: HashMap<String, i64>,
    default_timeout: u64,
    navigation_timeout: u64,
}

enum Timeout {
    Exhausted,
    Ms(Option<u64>),
}

/// min(remaining clock, timeout_ms); an exhausted clock fails the step unrun.
fn step_timeout(step: &Step, clocks: &HashMap<String, i64>) -> Result<Timeout> {
    let mut t = step.get("timeout_ms").and_then(Value::as_f64).map(|f| f as i64);
    if let Some(c) = step.get("clock") {
        let name = js::js_string(c);
        let deadline = clocks.get(&name).ok_or_else(|| anyhow!("clock {name} was never started"))?;
        let remaining = deadline - js::now_ms();
        if remaining <= 0 {
            return Ok(Timeout::Exhausted);
        }
        t = Some(t.map_or(remaining, |t| t.min(remaining)));
    }
    Ok(Timeout::Ms(t.map(|t| t.max(0) as u64)))
}

/// A JS string argument: String(value) of the step field (undefined -> "undefined").
fn js_arg(step: &Step, k: &str) -> String {
    match step.raw.get(k) {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(v) => js::js_string(v),
    }
}

/// Evaluate a caller-supplied page function at the capture instant.
async fn page_fn(page: &Page, src: &str, what: &str) -> Result<Value> {
    let expr = call_expr(Some(&Value::String(src.to_string())), None).map_err(|_| anyhow!("{what} must be a JavaScript function source"))?;
    page.evaluate(&expr).await?.ok_or_else(|| anyhow!("{what} returned undefined"))
}

async fn screenshot_step(step: &Step, page: &Page, ctx: &RunCtx<'_>) -> Result<Option<Value>> {
    let name = safe_name(step.raw.get("name"))?;
    let full_page = step.raw.get("full_page") != Some(&Value::Bool(false));
    let ocr_spec = match step.get("ocr") {
        Some(v) => Some(ocr::parse(v)?),
        None => None,
    };
    // The caller's keyword and region functions read the page at the capture instant.
    let t = Instant::now();
    let mut keywords: Option<Vec<String>> = None;
    let mut spec = None;
    if let Some((mut s, fns)) = ocr_spec {
        if let Some(src) = &fns.keywords_fn {
            let v = page_fn(page, src, "ocr.stop_when.keywords_fn").await?;
            let arr = v.as_array().filter(|a| a.iter().all(Value::is_string));
            let arr = arr.ok_or_else(|| anyhow!("ocr.stop_when.keywords_fn must return an array of strings"))?;
            keywords = Some(arr.iter().map(|x| x.as_str().unwrap_or("").to_string()).collect());
        }
        if let Some(src) = &fns.regions_fn {
            let v = page_fn(page, src, "ocr.regions").await?;
            s.regions = ocr::parse_rects(&v, "ocr.regions result")?;
        }
        spec = Some(s);
    }
    let fns_ms = t.elapsed().as_millis() as u64;

    let t = Instant::now();
    let png = tokio::time::timeout(Duration::from_millis(SCREENSHOT_TIMEOUT_MS), page.screenshot(full_page))
        .await
        .map_err(|_| anyhow!("screenshot exceeded {SCREENSHOT_TIMEOUT_MS}ms"))??;
    let capture_ms = t.elapsed().as_millis() as u64;
    let t = Instant::now();
    let mut stored = store::put(ctx.event, &name, &png, "image/png").await?;
    let store_ms = t.elapsed().as_millis() as u64;
    let (w, h) = if png.len() > 24 {
        (u32::from_be_bytes([png[16], png[17], png[18], png[19]]) as i64, u32::from_be_bytes([png[20], png[21], png[22], png[23]]) as i64)
    } else {
        (-1, -1)
    };
    stored["width"] = json!(w);
    stored["height"] = json!(h);
    let mut timings = json!({"fns_ms": fns_ms, "capture_ms": capture_ms, "store_ms": store_ms});
    if let Some(spec) = spec {
        let regions: Vec<Value> = spec.regions.iter().map(|r| json!({"x": r.x, "y": r.y, "width": r.width, "height": r.height})).collect();
        // In the lane, on the bytes just captured: decoded once in memory, no
        // re-encode, no temp file. The text rides back in the response.
        let mut out = ocr::read(Arc::new(png), spec, keywords).await?;
        for k in ["decode_ms", "resample_ms", "ocr_ms"] {
            timings[k] = out["timings"][k].clone();
        }
        out["regions"] = json!(regions);
        stored["ocr"] = out;
    }
    stored["timings"] = timings;
    Ok(Some(stored))
}

/// One step against the page. Ok(None) is a JS `undefined` value.
async fn run_step(step: &Step, page: &Page, ctx: &mut RunCtx<'_>) -> Result<Option<Value>> {
    let t = match step_timeout(step, &ctx.clocks)? {
        Timeout::Exhausted => bail!("clock {} exhausted before {} began", js_arg(step, "clock"), step.op),
        Timeout::Ms(t) => t,
    };
    let wait_default = t.unwrap_or(ctx.default_timeout);
    let nav_default = t.unwrap_or(ctx.navigation_timeout);
    let sel = || js_arg(step, "selector");
    match step.op.as_str() {
        "goto" => {
            let url = js_arg(step, "url");
            let wu = step.str("wait_until").filter(|s| !s.is_empty()).unwrap_or("domcontentloaded");
            let resp = page.goto(&url, wu, nav_default).await?;
            let mut out = nav_json(&resp, &page.url());
            if let Some(r) = &resp {
                if js::truthy(step.raw.get("body")) {
                    out["body"] = json!(page.response_text(&r.resp.request_id).await?);
                }
            }
            Ok(Some(out))
        }
        "wait_for_navigation" => {
            let wu = step.str("wait_until").filter(|s| !s.is_empty()).unwrap_or("domcontentloaded");
            let resp = page.wait_for_navigation(wu, nav_default).await?;
            let mut out = nav_json(&resp, &page.url());
            if let Some(o) = out.as_object_mut() {
                o.remove("headers");
            }
            Ok(Some(out))
        }
        "wait_for_function" => {
            let expr = call_expr(step.raw.get("fn"), step.raw.get("arg"))?;
            let polling = step.raw.get("polling").cloned().unwrap_or(Value::Null);
            let msg = format!("Waiting failed: {wait_default}ms exceeded");
            let v = page.poll(&expr, &polling, wait_default, &msg).await?;
            Ok(Some(v.unwrap_or(Value::Null)))
        }
        "evaluate" => page.evaluate(&call_expr(step.raw.get("fn"), step.raw.get("arg"))?).await,
        "click" => {
            page.locator_click(&sel(), t.unwrap_or(30000)).await?;
            Ok(Some(Value::Null))
        }
        "fill" => {
            page.locator_fill(&sel(), &js_arg(step, "value"), t.unwrap_or(30000)).await?;
            Ok(Some(Value::Null))
        }
        "check" => {
            // Click only when unchecked: a check step states the END state.
            let s = sel();
            page.wait_for_selector(&s, "attached", wait_default).await?;
            let checked = page
                .evaluate(&format!(
                    "(() => {{ const el = document.querySelector({0}); if (!el) throw new Error('failed to find element matching selector \"' + {0} + '\"'); return !!el.checked; }})()",
                    serde_json::to_string(&s)?
                ))
                .await?;
            if checked != Some(Value::Bool(true)) {
                page.click_selector(&s).await?;
            }
            Ok(Some(Value::Bool(true)))
        }
        "press" => {
            if let Some(s) = step.get("selector").filter(|v| js::truthy(Some(v))) {
                page.focus(&js::js_string(s)).await?;
            }
            page.key_press(&js_arg(step, "key"), 0).await?;
            Ok(Some(Value::Null))
        }
        "type" => {
            let delay = step.get("delay_ms").and_then(Value::as_f64).unwrap_or(0.0).max(0.0) as u64;
            page.type_into(&sel(), &js_arg(step, "text"), delay).await?;
            Ok(Some(Value::Null))
        }
        "wait_for_selector" => {
            let state = step.str("state").filter(|s| !s.is_empty()).unwrap_or("attached");
            page.wait_for_selector(&sel(), state, wait_default).await?;
            Ok(Some(Value::Bool(true)))
        }
        "wait_for_url" => {
            let re = match step.raw.get("url_regex") {
                Some(Value::String(s)) if !s.is_empty() => s.clone(),
                _ => bail!("wait_for_url.url_regex must be a non-empty regex string"),
            };
            let pred = format!("new RegExp({}).test(location.href)", serde_json::to_string(&re)?);
            let msg = format!("Waiting failed: {wait_default}ms exceeded");
            page.poll(&pred, &Value::Null, wait_default, &msg).await?;
            Ok(Some(json!(page.url())))
        }
        "text" => {
            let target = match step.get("selector").filter(|v| js::truthy(Some(v))) {
                Some(s) => format!("document.querySelector({})", serde_json::to_string(&js::js_string(s))?),
                None => "document.body".to_string(),
            };
            page.evaluate(&format!("(() => {{ const el = {target}; return el ? el.innerText : null; }})()")).await
        }
        "attribute" => {
            let s = serde_json::to_string(&sel())?;
            let n = serde_json::to_string(&js_arg(step, "name"))?;
            page.evaluate(&format!("(() => {{ const el = document.querySelector({s}); return el ? el.getAttribute({n}) : null; }})()")).await
        }
        "count" => page.evaluate(&format!("document.querySelectorAll({}).length", serde_json::to_string(&sel())?)).await,
        "content" => Ok(Some(json!(page.content().await?))),
        "url" => Ok(Some(json!(page.url()))),
        "cookies" => Ok(Some(page.all_cookies().await?)),
        "set_cookies" => {
            let cookies = step.get("cookies").and_then(Value::as_array).cloned().unwrap_or_default();
            page.set_cookies(&cookies).await?;
            Ok(Some(Value::Null))
        }
        "sleep" => {
            let ms = step.get("ms").and_then(Value::as_f64).unwrap_or(0.0).max(0.0) as u64;
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(Some(Value::Null))
        }
        "start_clock" => {
            let name = step.str("name");
            let ms = step.get("ms").and_then(Value::as_f64).filter(|f| f.is_finite());
            let (Some(name), Some(ms)) = (name, ms) else { bail!("start_clock needs name and ms") };
            ctx.clocks.insert(name.to_string(), js::now_ms() + ms as i64);
            Ok(Some(Value::Null))
        }
        "screenshot" => screenshot_step(step, page, ctx).await,
        other => bail!("unreachable op {other}"),
    }
}

/// Apply the request's context-level setup to the fresh page: viewport,
/// default timeouts, init scripts, cookies and request interception.
async fn install_context(page: &Page, c: &mut Ctx) -> Result<()> {
    page.set_viewport(c.viewport.0, c.viewport.1).await?;
    for src in &c.init_scripts {
        page.send("Page.addScriptToEvaluateOnNewDocument", json!({"source": src})).await?;
    }
    if !c.cookies.is_empty() {
        page.set_cookies(&c.cookies).await?;
    }
    page.observe();
    if let Some(rules) = c.intercept.take() {
        page.set_intercept(rules).await?;
    }
    Ok(())
}

/// The browse Lambda action.
pub async fn browse(event: &Value) -> Result<Value> {
    let started = Instant::now();
    let browser_args = parse_browser_args(event.get("browser_args"))?;
    let mut c = parse_context(event.get("context"))?;
    let steps = parse_steps(event.get("steps"))?;
    let (out, worker) = with_page(&browser_args, |page, acquire_ms| async move {
        run_request(event, &page, &mut c, &steps, started, acquire_ms).await
    })
    .await?;
    let mut out = out;
    out["worker"] = worker;
    Ok(out)
}

/// Run `f` on this process's pooled page: acquire the pre-built context+page,
/// then release it (close the context, build the next spare in the background).
/// Every capture -- a browse request or an evidence screenshot -- goes through
/// here, so there is one browser path. Returns f's result and the worker facts.
pub async fn with_page<T, F, Fut>(browser_args: &[String], f: F) -> Result<(T, Value)>
where
    F: FnOnce(Arc<Page>, u64) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    if BUSY.swap(true, Ordering::SeqCst) {
        bail!("browse: a second concurrent request reached one pool process; each process serves one request at a time");
    }
    let r = with_page_inner(browser_args, f).await;
    BUSY.store(false, Ordering::SeqCst);
    r
}

async fn with_page_inner<T, F, Fut>(browser_args: &[String], f: F) -> Result<(T, Value)>
where
    F: FnOnce(Arc<Page>, u64) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let acquire_start = Instant::now();
    let spare = {
        let mut st = pool().lock().await;
        ensure_browser(&mut st, browser_args).await?;
        take_spare(&mut st).await?
    };
    let acquire_ms = acquire_start.elapsed().as_millis() as u64;
    // Shared so tasks f spawns on the page (a capture's click and navigation
    // waiters) may outlive f; the context close ends them.
    let page = Arc::new(spare.page);
    let result = f(page.clone(), acquire_ms).await;

    let mut st = pool().lock().await;
    if let Some(b) = &st.browser {
        if let Err(e) = browser::dispose_context(&b.cdp, &spare.context).await {
            eprintln!("[browse] context close failed: {e:#}");
        }
        page.cdp.unsubscribe(&page.session);
    }
    if st.browser.as_ref().is_some_and(Browser::connected) {
        schedule_spare(&mut st);
    }
    if result.is_ok() {
        st.served += 1;
    }
    let worker = json!({
        "pid": std::process::id(),
        "lane": std::env::var("SNAPBOT_LANE").unwrap_or_default(),
        "browser_launches": st.launches,
        "served": st.served,
        "store": "s3",
    });
    drop(st);
    Ok((result?, worker))
}

async fn run_request(event: &Value, page: &Page, c: &mut Ctx, steps: &[Step], started: Instant, acquire_ms: u64) -> Result<Value> {
    let setup_start = Instant::now();
    install_context(page, c).await?;
    let setup_ms = setup_start.elapsed().as_millis() as u64;
    let mut ctx = RunCtx {
        event,
        clocks: HashMap::new(),
        default_timeout: c.default_timeout.unwrap_or(30000),
        navigation_timeout: c.navigation_timeout.or(c.default_timeout).unwrap_or(30000),
    };
    let mut results: Vec<Value> = Vec::new();
    let mut by_id: HashMap<String, (bool, bool)> = HashMap::new(); // id -> (skipped, ok)
    let mut stopped = false;
    let mut all_ok = true;
    for step in steps {
        let gate_ok = step.raw.get("if_ok").is_some_and(|g| !by_id.get(&js::js_string(g)).is_some_and(|r| !r.0 && r.1));
        let gate_failed = step.raw.get("if_failed").is_some_and(|g| !by_id.get(&js::js_string(g)).is_some_and(|r| !r.0 && !r.1));
        if stopped || gate_ok || gate_failed {
            results.push(json!({"id": step.id, "op": step.op, "skipped": true}));
            by_id.insert(step.id.clone(), (true, false));
            continue;
        }
        let t0 = Instant::now();
        let mut r = Map::new();
        r.insert("id".into(), json!(step.id));
        r.insert("op".into(), json!(step.op));
        let ok = match run_step(step, page, &mut ctx).await {
            Ok(value) => {
                let failure = check_expect(step.raw.get("expect"), value.as_ref());
                if let Some(v) = value {
                    r.insert("value".into(), v);
                }
                r.insert("ok".into(), json!(failure.is_none()));
                if let Some(f) = &failure {
                    r.insert("error".into(), json!(format!("expect: {f}")));
                }
                failure.is_none()
            }
            Err(e) => {
                r.insert("ok".into(), json!(false));
                r.insert("error".into(), json!(format!("{e:#}")));
                false
            }
        };
        r.insert("elapsed_ms".into(), json!(t0.elapsed().as_millis() as u64));
        if !ok && !step.optional() {
            stopped = true;
            all_ok = false;
        }
        by_id.insert(step.id.clone(), (false, ok));
        results.push(Value::Object(r));
    }
    let st = page.state.lock().unwrap();
    Ok(json!({
        "ok": all_ok,
        "steps": results,
        "final_url": st.url,
        "console_errors": st.console_errors,
        "failed_requests": st.failed_requests,
        "http_errors": st.http_errors,
        "timings": {"total_ms": started.elapsed().as_millis() as u64, "acquire_ms": acquire_ms, "setup_ms": setup_ms},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_ok_must_name_an_earlier_step() {
        // WHY: a forward reference would gate on a step that has not run and
        // silently skip the dependent step.
        let bad = json!([{"op": "url", "if_ok": "later"}, {"id": "later", "op": "url"}]);
        assert!(parse_steps(Some(&bad)).is_err());
        let good = json!([{"id": "a", "op": "url"}, {"op": "url", "if_ok": "a"}]);
        assert_eq!(parse_steps(Some(&good)).unwrap()[1].id, "1:url");
    }
}
