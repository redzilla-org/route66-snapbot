//! browse -- ONE complete browsing request per Lambda invocation.
//!
//! Owner directives 2026-09-26: "snapbot daemon should host the playwright
//! browser sessions exclusively", "snapbot receives a complete browsing request
//! as a lambda call" and "delegate to snapbot which can stay up as a permanent
//! pool along with kumo". The browser and ONE browsing context are set up once,
//! during init (route66 GH #4471, owner: "one context built in init", "two clear
//! calls, keep HTTP cache"). The context keeps DNS, connections, TLS sessions,
//! HSTS and the HTTP cache warm across requests. Each request gets its own page,
//! so sessionStorage, init scripts, interception and the document start empty.
//! After the request, cookies and every visited origin's storage are cleared
//! and the page is closed.
//!
//! ONE REQUEST AT A TIME PER PROCESS (the Lambda contract). A page is created
//! only after the previous request's page is closed, so no page is driving
//! while the next one is created (route66 GH #4040: doing so hangs the browser).
//!
//! Snapbot captures; the caller judges. The contract (request and result
//! shapes, the op set, if_ok/if_failed, clocks) is the former browse.js's.

use crate::actions::nav_json;
use crate::browser::{self, Browser, Fulfill, Intercept, LaunchOptions, Page};
use crate::{js, ocr};
use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MAX_STEPS: usize = 200;
const MAX_STEP_TIMEOUT_MS: f64 = 120000.0;
const SCREENSHOT_TIMEOUT_MS: u64 = 30000;

const OPS: [&str; 22] = [
    "goto", "wait_for_function", "evaluate", "click", "fill", "check", "press", "type", "wait_for_selector", "wait_for_url",
    "wait_for_navigation", "text", "attribute", "count", "content", "url", "cookies", "set_cookies", "screenshot", "sleep",
    "start_clock", "wait_for_render",
];

#[derive(Default)]
struct PoolState {
    browser: Option<Browser>,
    args_key: Option<String>,
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

/// The one browsing context every request's page opens in. cefhost creates a
/// CEF request context the first time a page names it; dispose_context forgets
/// it, so the next page opens a new one under the same name.
const REQUEST_CONTEXT: &str = "request";

/// Launch the browser and open the one context during init, so no request
/// pays either.
pub async fn warm(extra: &Value) -> Result<()> {
    let extra = parse_browser_args(Some(extra))?;
    let mut st = pool().lock().await;
    ensure_browser(&mut st, &extra).await?;
    // WHY: opening and closing one page creates the CEF request context now, in init.
    let page = browser::new_page(Some(REQUEST_CONTEXT)).await.map_err(|e| anyhow!("warm: the browsing context could not be opened: {e:#}"))?;
    browser::close_page(&page).await?;
    Ok(())
}

/// Clear what one request could leave for the next: the context's cookie jar,
/// and every visited origin's storage (localStorage, IndexedDB, Cache Storage,
/// service workers). The HTTP cache stays warm, by owner order.
async fn clear_request_state(page: &Page) -> Result<()> {
    page.send("Network.clearBrowserCookies", json!({})).await?;
    for origin in page.visited_origins() {
        page.send("Storage.clearDataForOrigin", json!({"origin": origin, "storageTypes": "all"})).await?;
    }
    Ok(())
}

/// A throwaway page that touches the rendering paths a real journey hits:
/// fixed/3D-composited layers, transitions, shadows, gradients, radii, blur and
/// opacity, form controls, text in several faces with fallback glyphs, SVG
/// dashed paths, canvas 2D readback and a decoded image.
const PRIME_HTML: &str = r##"<!doctype html><html><head><meta charset="utf-8"><style>
body{margin:0;font:14px/1.4 sans-serif;color:#222}
header{position:fixed;top:0;left:0;right:0;height:56px;z-index:10;color:#fff;background:linear-gradient(90deg,#123,#468);box-shadow:0 2px 8px rgba(0,0,0,.4)}
main{padding:64px 16px 16px}
.card{border-radius:12px;box-shadow:0 4px 16px rgba(0,0,0,.25);background:radial-gradient(circle,#fff,#eef);padding:16px;margin:8px 0}
.layer{will-change:transform;transform:translate3d(0,0,0);transition:transform .2s,opacity .2s}
.moved{transform:translate3d(40px,8px,0) scale(1.05);opacity:.7}
.blur{filter:blur(2px) drop-shadow(2px 2px 2px #000)}
.dd{position:absolute;top:100%;left:0;right:0;max-height:160px;overflow:auto;background:#fff;border:1px solid #ccc;border-radius:4px;box-shadow:0 6px 12px rgba(0,0,0,.175)}
.dd div:hover,.dd .on{background:#e8f0fe}
#map{position:relative;width:640px;height:360px;overflow:hidden;background:#ddd}
#map>*{position:absolute;left:0;top:0}
.stripes{height:1600px;background:repeating-linear-gradient(45deg,#eee 0 10px,#fff 10px 20px)}
</style></head><body>
<header><b style="font-size:22px">Prime</b> <span style="font-family:serif">serif</span> <i>italic</i></header>
<main>
<div style="position:relative;width:420px"><input id="q" type="text" placeholder="City, ZIP or neighborhood" style="width:100%;padding:8px;border:1px solid #888;border-radius:6px"><div class="dd" id="dd"></div></div>
<p><label><input id="cb" type="checkbox"> check</label> <label><input type="radio" checked> radio</label>
<select><option>one</option><option>two</option></select> <button>Search</button> <textarea rows="2">text</textarea>
<input type="range"> <progress value="40" max="100"></progress></p>
<div class="card layer" id="mv"><b>Bold</b> <span style="font-weight:300">light</span> <span style="font-family:monospace">mono 0123456789</span>
<span style="font-size:36px">Big</span> <span style="font-size:9px">tiny</span> &#10003; &#9733; &#8230; &#8364; &#9662; &#26085;&#26412; &#128512;</div>
<div id="map" class="layer"><img id="img" width="256" height="256" style="transform:translate3d(16px,16px,0)">
<svg width="640" height="360"><path d="M20 20 L260 60 L200 320 Z" fill="#3388ff" fill-opacity=".2" stroke="#3388ff" stroke-width="3" stroke-dasharray="5,5" stroke-linejoin="round"/>
<circle cx="400" cy="180" r="60" fill="none" stroke="#e33" stroke-width="2"/><text x="300" y="40">svg text</text></svg>
<canvas id="c2d" width="320" height="180" style="left:320px;top:180px"></canvas></div>
<div class="card blur">blurred shadowed card</div>
<div class="stripes"></div>
</main>
<script>
const c = document.getElementById('c2d').getContext('2d', {willReadFrequently: true});
c.fillStyle = '#4a4'; c.fillRect(0, 0, 320, 180);
c.strokeStyle = '#00f'; c.lineWidth = 4; c.setLineDash([6, 4]); c.beginPath(); c.arc(160, 90, 60, 0, 6.283); c.stroke();
c.font = 'bold 20px sans-serif'; c.fillStyle = '#000'; c.fillText('canvas', 120, 96);
c.getImageData(0, 0, 320, 180);
const t = document.createElement('canvas'); t.width = t.height = 256;
const tc = t.getContext('2d'); tc.fillStyle = tc.createLinearGradient(0, 0, 256, 256); tc.fillRect(0, 0, 256, 256);
document.getElementById('img').src = t.toDataURL('image/png');
const dd = document.getElementById('dd');
document.getElementById('q').addEventListener('input', e => {
  dd.innerHTML = Array.from({length: 12}, (_, i) => '<div' + (i ? '' : ' class="on"') + '>' + e.target.value + ' Neighborhood ' + i + ' (9201' + i + ')</div>').join('');
  document.getElementById('mv').classList.toggle('moved');
});
let n = 0; const tick = () => { if (++n < 12) requestAnimationFrame(tick); else window.__primed = true; }; requestAnimationFrame(tick);
</script></body></html>"##;

/// Render PRIME_HTML in its own context at the desktop and a phone viewport,
/// type into its input, scroll and capture it, then dispose the context.
async fn prime_page(page: &Page) -> Result<()> {
    let url = format!("data:text/html;base64,{}", base64::engine::general_purpose::STANDARD.encode(PRIME_HTML));
    let mut phases = Map::new();
    let mut t = Instant::now();
    let mut lap = |name: &str, t: &mut Instant| {
        phases.insert(name.to_string(), json!(t.elapsed().as_millis() as u64));
        *t = Instant::now();
    };
    // A capture view taller than the viewport, as route66's height-declaring checks use.
    page.set_view(1366, 900, 3000).await?;
    page.goto(&url, "load", 60000).await?;
    lap("load", &mut t);
    page.poll("window.__primed === true", &Value::Null, 60000, "prime: frames never ran").await?;
    lap("frames", &mut t);
    page.locator_fill("#q", "92014", 60000).await?;
    lap("first_fill", &mut t);
    page.locator_fill("#q", "", 60000).await?;
    page.type_into("#q", "9213", 0).await?;
    page.key_press("ArrowDown", 0).await?;
    lap("type", &mut t);
    // The open dropdown covers #cb; the hit-target check (route66#4471) would retry
    // that click forever. It has rendered, so close it before clicking.
    page.evaluate("document.getElementById('dd').replaceChildren()").await?;
    page.locator_click("#cb", 60000).await?;
    lap("click", &mut t);
    page.evaluate("window.scrollTo(0, 1200)").await?;
    page.freeze(false).await?;
    lap("viewport_shot", &mut t);
    page.freeze(true).await?;
    lap("full_shot", &mut t);
    page.set_viewport(390, 844).await?;
    page.evaluate("window.scrollTo(0, 0)").await?;
    page.freeze(false).await?;
    lap("phone_shot", &mut t);
    println!("[browse] prime phases ms {}", Value::Object(phases));
    Ok(())
}

/// WHY (route66 GH #4471/#4483): a fresh CommandCenterSnapbot-app sandbox
/// stalled 5-8 s on its first browse's first input while the page's JS sat
/// idle; warm sandboxes never did (35 of 40 fresh over the 4000 ms step
/// budget, 0 of 120 warm). The measured cause was SwiftShader's per-pipeline JIT,
/// gone since cefhost runs with --disable-gpu; Chromium's first frames
/// still pay smaller one-time costs (renderer, font and decode setup) that
/// warm() alone, building an about:blank page, never triggers. Paying them here, during Lambda init and before the
/// first poll, leaves no request to pay them inside a step's budget. The
/// throwaway context is disposed and never counted as served.
pub async fn prime(extra: &Value) -> Result<()> {
    let started = Instant::now();
    let args = parse_browser_args(Some(extra))?;
    {
        let mut st = pool().lock().await;
        ensure_browser(&mut st, &args).await?;
    }
    let context = browser::create_context().await?;
    let page = browser::new_page(Some(&context)).await?;
    let primed = prime_page(&page).await;
    drop(page);
    browser::dispose_context(&context).await?;
    primed.map_err(|e| anyhow!("prime: {e:#}"))?;
    warm(extra).await?;
    println!("[browse] primed in {}ms", started.elapsed().as_millis());
    Ok(())
}

struct Ctx {
    viewport: (i64, i64),
    /// The OSR view (capture) height; the viewport height when not requested.
    capture_height: i64,
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
    // WHY (route66 owner 2026-11-15: capture size is a per-test-case
    // parameter): context.capture {width, height} sizes the painted view once
    // for the whole visit; width must be the viewport's (tiling is vertical).
    let capture_height = match c.get("capture").filter(|v| js::truthy(Some(v))) {
        None => viewport.1,
        Some(v) => {
            let int = |k: &str| v.get(k).and_then(Value::as_f64).filter(|f| f.fract() == 0.0).map(|f| f as i64);
            match (int("width"), int("height")) {
                (Some(w), Some(h)) if w == viewport.0 && h >= viewport.1 => h,
                _ => bail!("context.capture needs integer width (= viewport width {}) and height (>= viewport height {})", viewport.0, viewport.1),
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
        capture_height,
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
        for r in ["if_ok", "if_failed", "if_html_incomplete"] {
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

struct RunCtx {
    /// How long the last page-changing step waited for its result's paint.
    paint_wait_ms: Option<u64>,
    /// The capture armed for the image after the current step, and once that
    /// step completes, the frozen view (with what made it ready).
    armed: Option<tokio::sync::oneshot::Receiver<Result<crate::cefhost::Frozen>>>,
    frozen: Option<crate::cefhost::Frozen>,
    clocks: HashMap<String, i64>,
    default_timeout: u64,
    navigation_timeout: u64,
}

// Match the harness's captured-HTML oracle without changing the capture result:
// only an incomplete first DOM needs another wait and content read.
fn captured_html_complete(html: &str) -> bool {
    let html = html.trim();
    if html.is_empty() {
        return false;
    }
    let lower = html.to_lowercase();
    if !lower.contains("<html") || !lower.contains("<body") || !lower.contains("</body>") || !lower.contains("</html>") {
        return false;
    }
    if lower.find("<body").unwrap() > lower.rfind("</body>").unwrap()
        || lower.find("<html").unwrap() > lower.rfind("</html>").unwrap()
    {
        return false;
    }
    !lower.rfind('<').is_some_and(|left| lower.rfind('>').map_or(true, |right| right < left))
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

/// Start an image: evaluate the caller's capture-instant functions, freeze
/// the view, and return the readers' task (awaited by run_request).
async fn screenshot_step(step: &Step, page: &Page, armed: Option<crate::cefhost::Frozen>) -> Result<(tokio::task::JoinHandle<Result<Value>>, &'static str)> {
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

    // Element regions are located in the painted document at capture time.
    // Snapbot reports measurements for them; callers never fetch the PNG.
    let mut inspect_spec = step.get("inspect").cloned().unwrap_or_else(|| json!({}));
    let mut resolved = Vec::new();
    if let Some(regions) = inspect_spec.get("regions").and_then(Value::as_array) {
        for region in regions {
            let name = region.get("name").and_then(Value::as_str).ok_or_else(|| anyhow!("inspect region needs name"))?;
            let source = region.get("rect_fn").and_then(Value::as_str).ok_or_else(|| anyhow!("inspect region {name} needs rect_fn"))?;
            let expr = call_expr(Some(&Value::String(source.to_string())), region.get("arg"))?;
            let rect = page.evaluate(&expr).await?.ok_or_else(|| anyhow!("inspect region {name} has no rect"))?;
            resolved.push(json!({"name": name, "kind": region.get("kind"), "rect": rect}));
        }
    }
    inspect_spec["resolved_regions"] = json!(resolved);

    // The image is one CDP surface capture (route66 GH #4486). An armed image
    // was captured when its step completed. An unarmed one (first in a
    // request, after another image, or after a skipped step) is captured now.
    // Both replace OSR view copies that froze stale frames (local-verify
    // 20261116T070021Z california/CTH/workflow-saved-search-newsfeed) and
    // composites (20261116T072458Z california/DMV/
    // map-picker-unobscured-centering). No reply within the deadline is the
    // unarmed twin of the armed "surface capture never returned" error.
    let t = Instant::now();
    let frozen = match armed {
        Some(f) => f,
        None => match tokio::time::timeout(Duration::from_millis(SCREENSHOT_TIMEOUT_MS), page.freeze(full_page)).await {
            Ok(r) => r?,
            Err(_) => {
                page.disarm();
                bail!("screenshot not captured within its {SCREENSHOT_TIMEOUT_MS}ms deadline (unarmed surface capture never returned)");
            }
        },
    };
    let capture_ms = t.elapsed().as_millis() as u64;
    let frame = Arc::new(frozen.frame);
    let url = page.url();
    // WHY (route66 GH #4082, owner 2026-10-07: "log the OCR timings"; GH #4486):
    // only unsigned numbers, route66's harness decodes timings as map[string]uint64.
    let mut timings = json!({"fns_ms": fns_ms, "capture_ms": capture_ms, "tiles": 1, "frame_bytes": frame.height * frame.stride,
                             "ready_to_capture_ms": frozen.ready_to_capture_ms, "decode_ms": frozen.decode_ms,
                             "png_bytes": frozen.png_bytes});
    let timings_ready_by = frozen.ready_by;
    // The three readers (PNG encode+store, OCR, inspect) run in parallel on the
    // frozen frame while the page's next browser step runs (owner 2026-11-16:
    // "Allocate a new memfd and start the next browser step while PNG/OCR
    // run!"). The frame is freed when the last of them finishes.
    let readers = tokio::spawn(async move {
        let rest = Instant::now();
        let inspect_frame = frame.clone();
        let inspection = tokio::task::spawn_blocking(move || {
            let t = Instant::now();
            (crate::inspect::analyze(&inspect_frame, &inspect_spec), t.elapsed().as_millis() as u64)
        });
        let write = crate::writer::submit(&frame);
        let (w, h) = (frame.width as i64, frame.height as i64);
        let mut ocr_result = None;
        let mut ocr_error = None;
        if let Some(spec) = spec {
            let regions: Vec<Value> = spec.regions.iter().map(|r| json!({"x": r.x, "y": r.y, "width": r.width, "height": r.height})).collect();
            let label = json!({"name": name, "url": url, "full_page": full_page});
            let t = Instant::now();
            let read = ocr::read(frame.clone(), spec, keywords, label).await;
            timings["ocr_reader_ms"] = json!(t.elapsed().as_millis() as u64);
            match read {
                Ok(mut out) => {
                    for k in ["convert_ms", "resample_ms", "ocr_ms", "cpu_ms", "wall_ms", "engine_init_ms", "text_bytes"] {
                        timings[k] = out["timings"][k].clone();
                    }
                    out["regions"] = json!(regions);
                    ocr_result = Some(out);
                }
                Err(e) => ocr_error = Some(e),
            }
        }
        let mut stored = crate::writer::finish(write).await?;
        let (inspected, inspect_ms) = inspection.await.map_err(|e| anyhow!("screenshot inspector exited: {e}"))?;
        stored["inspect"] = inspected;
        for k in ["encode_ms", "write_ms"] {
            timings[k] = stored[k].take();
            stored.as_object_mut().unwrap().remove(k);
        }
        timings["inspect_ms"] = json!(inspect_ms);
        timings["rest_ms"] = json!(rest.elapsed().as_millis() as u64);
        if let Some(e) = ocr_error {
            return Err(e);
        }
        stored["width"] = json!(w);
        stored["height"] = json!(h);
        if let Some(out) = ocr_result {
            stored["ocr"] = out;
        }
        // The log and response use the final SHA-1 address, not a temporary name.
        eprintln!("SNAPBOT-SHOT {}", json!({"name": name, "url": url, "png": stored["url"], "full_page": full_page,
                                            "width": w, "height": h, "timings": timings}));
        stored["timings"] = timings;
        Ok(stored)
    });
    Ok((readers, timings_ready_by))
}

/// One step against the page. Ok(None) is a JS `undefined` value.
async fn run_step(step: &Step, page: &Page, ctx: &mut RunCtx) -> Result<Option<Value>> {
    let t = match step_timeout(step, &ctx.clocks)? {
        Timeout::Exhausted => bail!("clock {} exhausted before {} began", js_arg(step, "clock"), step.op),
        Timeout::Ms(t) => t,
    };
    let value = match run_step_op(step, page, ctx, t).await {
        Ok(v) => v,
        Err(e) => {
            if ctx.armed.take().is_some() {
                page.disarm();
            }
            return Err(e);
        }
    };
    let deadline = if step.op == "goto" { t.unwrap_or(ctx.navigation_timeout) } else { t.unwrap_or(ctx.default_timeout) };
    // A step whose image follows completes on its surface capture, sent at
    // its completing message (route66 GH #4486). A step whose completion is
    // not one CDP message makes the capture ready on return.
    if let Some(rx) = ctx.armed.take() {
        // A goto captures only on its lifecycle events; any other step not
        // completed by its matched reply captures now (no-op if done).
        if !matches!(step.op.as_str(), "goto" | "wait_for_function" | "wait_for_selector" | "wait_for_render") {
            page.ready_posted();
        }
        let w = Instant::now();
        let frozen = tokio::time::timeout(Duration::from_millis(deadline), rx)
            .await
            .map_err(|_| anyhow!("{} result not captured within its {deadline}ms deadline (armed surface capture never returned)", step.op))?
            .map_err(|_| anyhow!("{}'s armed capture was dropped", step.op))??;
        ctx.paint_wait_ms = Some(w.elapsed().as_millis() as u64);
        ctx.frozen = Some(frozen);
        return Ok(value);
    }
    // A page-changing step with no image finishes when its result is painted:
    // the generation recorded now, the first paint after it. An evaluate is a
    // read (location.href, DOM state): it damages nothing, so off-screen
    // rendering never paints after it and a paint wait only times out. An
    // evaluate whose image follows still completes on its reply (armed path).
    // A goto with no image finishes on its document's firstContentfulPaint:
    // DOMContentLoaded can precede it (a render-blocking stylesheet: route66
    // local-verify 2026-11-15 20:11 PT froze every detail-not-found blank), and
    // a document whose content painted before the step returned paints nothing
    // after it (CTC ctc-home-clear-filters-resets-fields timed out on gen 7).
    // A document with no contentful content never reaches FCP (the empty
    // /agent/_admin_check timed out at 2026-11-15 20:42 PT): its load event,
    // after every render-blocking resource, completes it instead.
    if step.op == "goto" {
        let w = Instant::now();
        page.wait_state(Some(Instant::now() + Duration::from_millis(deadline)), |st| {
            st.lifecycle.get(&st.loader).filter(|l| l.contains("firstContentfulPaint") || l.contains("load")).map(|_| ())
        })
        .await
        .ok_or_else(|| anyhow!("goto result not painted within its {deadline}ms deadline (no firstContentfulPaint or load)"))?;
        ctx.paint_wait_ms = Some(w.elapsed().as_millis() as u64);
        return Ok(value);
    }
    if matches!(step.op.as_str(), "click" | "fill" | "check" | "press" | "type") {
        let since = page.paint_generation();
        let w = Instant::now();
        tokio::time::timeout(Duration::from_millis(deadline), page.painted_after(since))
            .await
            .map_err(|_| anyhow!("{} result not painted within its {deadline}ms deadline (no paint after generation {since})", step.op))??;
        ctx.paint_wait_ms = Some(w.elapsed().as_millis() as u64);
    }
    Ok(value)
}

/// A goto step's wait_until as the CDP lifecycle event name.
fn goto_lifecycle(step: &Step) -> String {
    match step.str("wait_until").filter(|s| !s.is_empty()).unwrap_or("domcontentloaded") {
        "load" => "load",
        "networkidle0" | "networkidle" => "networkIdle",
        "networkidle2" => "networkAlmostIdle",
        _ => "DOMContentLoaded",
    }
    .to_string()
}

/// The CDP message that completes `op` when its image is armed (cefhost::Completes).
fn completes_for(step: &Step) -> Option<crate::cefhost::Completes> {
    use crate::cefhost::Completes;
    match step.op.as_str() {
        "evaluate" => Some(Completes::Reply),
        // A wait completes on a paint its condition holds for (checked per paint).
        "wait_for_function" => call_expr(step.raw.get("fn"), step.raw.get("arg")).ok().map(Completes::Condition),
        "wait_for_render" => Some(Completes::Rendered(js_arg(step, "name"))),
        "wait_for_selector" => {
            let state = step.str("state").filter(|s| !s.is_empty()).unwrap_or("attached");
            crate::actions::selector_predicate(&js_arg(step, "selector"), state).ok().map(|(p, _)| Completes::Condition(p))
        }
        _ => None,
    }
}

/// The step's own operation.
async fn run_step_op(step: &Step, page: &Page, ctx: &mut RunCtx, t: Option<u64>) -> Result<Option<Value>> {
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
        "evaluate" => {
            let expr = call_expr(step.raw.get("fn"), step.raw.get("arg"))?;
            // An evaluate whose image follows is captured on its reply, so
            // the reply waits two animation frames: every DOM change made
            // before it is committed (route66 local-verify 2026-11-15 20:42 PT
            // froze spinners on CTH search-result-page-2 after its card wait held).
            if ctx.armed.is_some() {
                page.evaluate(&format!("(async () => {{ const v = await ({expr}); await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))); return v; }})()")).await
            } else {
                page.evaluate(&expr).await
            }
        }
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
        // The page calls __r66Rendered(name) at the point its render callback
        // chooses (after DOM insert, image decode, fonts, a double rAF).
        "wait_for_render" => {
            let name = js_arg(step, "name");
            // Any call since this document committed, earlier steps included.
            let seen = page.state.lock().unwrap().rendered_from;
            let dl = Some(Instant::now() + Duration::from_millis(wait_default));
            page.wait_state(dl, |st| st.rendered[seen..].iter().any(|p| *p == name).then_some(()))
                .await
                .ok_or_else(|| anyhow!("wait_for_render {name}: __r66Rendered({name:?}) not called within {wait_default}ms"))?;
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
        other => bail!("unreachable op {other}"),
    }
}

/// Apply the request's context-level setup to the fresh page: viewport,
/// default timeouts, init scripts, cookies and request interception.
async fn install_context(page: &Page, c: &mut Ctx) -> Result<()> {
    page.set_view(c.viewport.0, c.viewport.1, c.capture_height).await?;
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
        run_request(&page, &mut c, &steps, started, acquire_ms).await
    })
    .await?;
    let mut out = out;
    out["worker"] = worker;
    Ok(out)
}

/// Run `f` on a new page in this process's one context, then clear the
/// request's cookies and origin storage and close the page.
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
    crate::runtime::diagnostic_phase("page-acquire");
    let page = {
        let mut st = pool().lock().await;
        ensure_browser(&mut st, browser_args).await?;
        browser::new_page(Some(REQUEST_CONTEXT)).await.map_err(|e| anyhow!("no page could be opened for this request: {e:#}"))?
    };
    let acquire_ms = acquire_start.elapsed().as_millis() as u64;
    // Shared so tasks f spawns on the page (a capture's click and navigation
    // waiters) may outlive f; the page close ends them.
    let page = Arc::new(page);
    crate::runtime::diagnostic_phase("page-acquired");
    let result = f(page.clone(), acquire_ms).await;

    crate::runtime::diagnostic_phase("page-cleanup");
    let mut st = pool().lock().await;
    // WHY: a context whose state could not be cleared is dropped whole, so
    // nothing from this request can reach the next; the next page opens a new one.
    let cleared = clear_request_state(&page).await.and(browser::close_page(&page).await);
    if let Err(e) = cleared {
        eprintln!("[browse] request state clear or page close failed, dropping the context: {e:#}");
        if let Err(e) = browser::dispose_context(REQUEST_CONTEXT).await {
            eprintln!("[browse] context close failed: {e:#}");
        }
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
    crate::runtime::diagnostic_phase("page-released");
    Ok((result?, worker))
}

async fn run_request(page: &Page, c: &mut Ctx, steps: &[Step], started: Instant, acquire_ms: u64) -> Result<Value> {
    let setup_start = Instant::now();
    crate::runtime::diagnostic_phase("context-setup");
    install_context(page, c).await?;
    let setup_ms = setup_start.elapsed().as_millis() as u64;
    let mut ctx = RunCtx {
        paint_wait_ms: None,
        armed: None,
        frozen: None,
        clocks: HashMap::new(),
        default_timeout: c.default_timeout.unwrap_or(30000),
        navigation_timeout: c.navigation_timeout.or(c.default_timeout).unwrap_or(30000),
    };
    let mut results: Vec<Value> = Vec::new();
    let mut by_id: HashMap<String, (bool, bool)> = HashMap::new(); // id -> (skipped, ok)
    let mut stopped = false;
    let mut all_ok = true;
    // Images whose readers are still running: (result index, readers, start).
    let mut images: Vec<(usize, tokio::task::JoinHandle<Result<Value>>, Instant)> = Vec::new();
    for step in steps {
        // A gate on an image waits for that image's readers.
        for g in ["if_ok", "if_failed", "if_html_incomplete"] {
            if let Some(id) = step.raw.get(g).map(js::js_string) {
                settle_images(&mut images, &mut results, &mut by_id, steps, &mut stopped, &mut all_ok, Some(&id)).await;
            }
        }
        let gate_ok = step.raw.get("if_ok").is_some_and(|g| !by_id.get(&js::js_string(g)).is_some_and(|r| !r.0 && r.1));
        let gate_failed = step.raw.get("if_failed").is_some_and(|g| !by_id.get(&js::js_string(g)).is_some_and(|r| !r.0 && !r.1));
        // Keep the first HTML bytes and transport outcome intact; a skipped
        // optional wait must not prevent an independently gated recapture.
        let gate_html_complete = step.raw.get("if_html_incomplete").is_some_and(|g| {
            let id = js::js_string(g);
            results.iter().find(|r| r.get("id").and_then(Value::as_str) == Some(id.as_str()))
                .and_then(|r| r.get("value")).and_then(Value::as_str).is_some_and(captured_html_complete)
        });
        if stopped || gate_ok || gate_failed || gate_html_complete {
            results.push(json!({"id": step.id, "op": step.op, "skipped": true}));
            by_id.insert(step.id.clone(), (true, false));
            continue;
        }
        let t0 = Instant::now();
        // Step identifiers and operations locate a stalled journey; arguments
        // can contain credentials and are deliberately absent from diagnostics.
        if crate::runtime::diagnostic_enabled() {
            crate::runtime::diagnostic_phase(&format!("step-start id={} op={}", step.id, step.op));
        }
        let mut r = Map::new();
        r.insert("id".into(), json!(step.id));
        r.insert("op".into(), json!(step.op));
        if step.op == "screenshot" {
            // The image's readers run while the next step runs; its result is
            // filled in when they finish (settle_images).
            match screenshot_step(step, page, ctx.frozen.take()).await {
                Ok((readers, ready_by)) => {
                    r.insert("freeze_to_next_step_ms".into(), json!(t0.elapsed().as_millis() as u64));
                    r.insert("ready_by".into(), json!(ready_by));
                    images.push((results.len(), readers, t0));
                    by_id.insert(step.id.clone(), (false, true));
                    results.push(Value::Object(r));
                    continue;
                }
                Err(e) => {
                    r.insert("ok".into(), json!(false));
                    r.insert("error".into(), json!(format!("{e:#}")));
                    r.insert("elapsed_ms".into(), json!(t0.elapsed().as_millis() as u64));
                    if !step.optional() {
                        stopped = true;
                        all_ok = false;
                    }
                    by_id.insert(step.id.clone(), (false, false));
                    results.push(Value::Object(r));
                    continue;
                }
            }
        }
        ctx.paint_wait_ms = None;
        let next_image = steps.get(results.len() + 1).filter(|n| n.op == "screenshot");
        if let Some(n) = next_image {
            let full = n.raw.get("full_page") != Some(&Value::Bool(false));
            ctx.armed = Some(page.arm(full, completes_for(step).or_else(|| (step.op == "goto").then(|| {
                crate::cefhost::Completes::Lifecycle(goto_lifecycle(step))
            }))).await);
        }
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
        if let Some(ms) = ctx.paint_wait_ms {
            r.insert("paint_wait_ms".into(), json!(ms));
        }
        if crate::runtime::diagnostic_enabled() {
            crate::runtime::diagnostic_phase(&format!("step-return id={} op={} elapsed_ms={} ok={ok}", step.id, step.op, t0.elapsed().as_millis()));
        }
        if !ok && !step.optional() {
            stopped = true;
            all_ok = false;
        }
        by_id.insert(step.id.clone(), (false, ok));
        results.push(Value::Object(r));
    }
    // Every image's readers (and its content-addressed write) finish before the response.
    settle_images(&mut images, &mut results, &mut by_id, steps, &mut stopped, &mut all_ok, None).await;
    let st = page.state.lock().unwrap();
    // WHY: retain native timing evidence, including pending resources, on both successful and failed journeys.
    let network = st.network.snapshot();
    Ok(json!({
        "ok": all_ok,
        "steps": results,
        "final_url": st.url,
        "console_errors": st.console_errors,
        "failed_requests": st.failed_requests,
        "http_errors": st.http_errors,
        "writer_drained": true,
        "timings": {"total_ms": started.elapsed().as_millis() as u64, "acquire_ms": acquire_ms, "setup_ms": setup_ms, "network": network},
    }))
}

/// Await image readers: the one whose step id is `only`, or all of them, and
/// fill their step results. A failed non-optional image fails the browse.
async fn settle_images(
    images: &mut Vec<(usize, tokio::task::JoinHandle<Result<Value>>, Instant)>,
    results: &mut [Value],
    by_id: &mut HashMap<String, (bool, bool)>,
    steps: &[Step],
    stopped: &mut bool,
    all_ok: &mut bool,
    only: Option<&str>,
) {
    let mut i = 0;
    while i < images.len() {
        let idx = images[i].0;
        let id = results[idx]["id"].as_str().unwrap_or("").to_string();
        if only.is_some_and(|o| o != id) {
            i += 1;
            continue;
        }
        let (_, readers, t0) = images.remove(i);
        let step = steps.iter().find(|s| s.id == id).expect("an image result names its step");
        let r = results[idx].as_object_mut().unwrap();
        let ok = match readers.await.map_err(|e| anyhow!("image readers exited: {e}")).and_then(|x| x) {
            Ok(v) => {
                let failure = check_expect(step.raw.get("expect"), Some(&v));
                r.insert("value".into(), v);
                if let Some(f) = &failure {
                    r.insert("error".into(), json!(format!("expect: {f}")));
                }
                failure.is_none()
            }
            Err(e) => {
                r.insert("error".into(), json!(format!("{e:#}")));
                false
            }
        };
        r.insert("ok".into(), json!(ok));
        r.insert("elapsed_ms".into(), json!(t0.elapsed().as_millis() as u64));
        if !ok && !step.optional() {
            *stopped = true;
            *all_ok = false;
        }
        by_id.insert(id, (false, ok));
    }
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
