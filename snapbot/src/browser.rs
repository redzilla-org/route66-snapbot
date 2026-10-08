//! In-process Chromium pages (CEF windowless) and one CDP page session with its
//! event state.
//!
//! The rendering switches live in cefhost (they reproduce the former puppeteer +
//! @sparticuz/chromium argv). This module keeps the page-level contract the
//! browse ops are written against: CDP commands, lifecycle/network/console state,
//! request interception, and a CDP screenshot decoded into the raw BGRA frame
//! OCR and the PNG writer share.

use crate::cefhost::{self, Event, PageHandle};
use crate::js;
use crate::shm::{Frame, Segment};
use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// This process's Chromium. CEF is initialized once, at process start, with
/// SNAPBOT_BROWSER_ARGS; it cannot be relaunched in process.
pub struct Browser {
    _private: (),
}

pub struct LaunchOptions {
    /// Kept for the callers' shape; CEF always runs its renderer out of process.
    pub single_process: bool,
    /// Browser-wide certificate-error bypass; cefhost always sets it.
    pub ignore_https_errors: bool,
    pub extra_args: Vec<String>,
}

impl Browser {
    /// Attach to the in-process Chromium. A request asking for launch args other
    /// than the ones CEF was initialized with fails hard: there is no second
    /// browser to launch (the former relaunch-on-new-args has no CEF equivalent).
    pub async fn launch(opts: LaunchOptions) -> Result<Browser> {
        let _ = (opts.single_process, opts.ignore_https_errors);
        if opts.extra_args.as_slice() != cefhost::launch_args() {
            bail!(
                "browser_args {:?} differ from this process's Chromium launch args {:?} (SNAPBOT_BROWSER_ARGS); embedded Chromium reads its switches once, at process start",
                opts.extra_args,
                cefhost::launch_args()
            );
        }
        Ok(Browser { _private: () })
    }

    /// The embedded browser lives as long as the process.
    pub fn connected(&self) -> bool {
        true
    }

    /// Close this handle's default-context pages.
    pub async fn close(self) {
        let _ = cefhost::dispose_context(DEFAULT_CONTEXT).await;
    }

    pub async fn new_page(&self, context: Option<&str>) -> Result<Page> {
        new_page(context).await
    }
}

/// The context pages land in when the caller names none.
const DEFAULT_CONTEXT: &str = "default";

/// A fresh cookie jar and cache per request (CEF request context).
pub async fn create_context() -> Result<String> {
    Ok(uuid::Uuid::new_v4().simple().to_string())
}

pub async fn dispose_context(id: &str) -> Result<()> {
    cefhost::dispose_context(id).await
}

/// A new about:blank page in `context` (the default context when None), at
/// puppeteer's default 800x600 window.
pub async fn new_page(context: Option<&str>) -> Result<Page> {
    let (handle, rx) = cefhost::create_page(context.unwrap_or(DEFAULT_CONTEXT), 800, 600).await?;
    Page::attach(handle, rx).await
}

/// A document response the main frame received for one loader.
#[derive(Clone, Debug)]
pub struct DocResponse {
    pub request_id: String,
    pub status: i64,
    pub url: String,
    pub headers: Value,
}

/// Request interception rules; the event loop applies them to Fetch.requestPaused.
pub enum Intercept {
    /// The browse context rules: abort -> fulfill -> host-scoped headers -> continue.
    Browse { abort: Vec<Regex>, fulfill: Vec<Fulfill>, header_rules: Vec<(String, Map<String, Value>)> },
    /// The evidence capture's fault injection: abort any subresource whose URL
    /// contains a pattern; the main-frame navigation always continues.
    Block { patterns: Vec<String> },
}

pub struct Fulfill {
    pub re: Regex,
    pub status: i64,
    pub content_type: String,
    pub body: Vec<u8>,
}

#[derive(Default)]
pub struct PageState {
    pub main_frame: String,
    pub url: String,
    pub loader: String,
    pub lifecycle: HashMap<String, HashSet<String>>,
    pub same_doc: u64,
    pub doc_resp: HashMap<String, DocResponse>,
    pub extra: HashMap<String, Vec<(i64, Value)>>,
    pub inflight: HashSet<String>,
    pub last_net: Option<Instant>,
    pub req_url: HashMap<String, String>,
    pub observe: bool,
    pub console_errors: Vec<String>,
    pub failed_requests: Vec<Value>,
    pub http_errors: Vec<Value>,
    pub crashed: bool,
}

/// The tallest CEF view a single CDP screenshot captures. A taller document
/// needs vertically offset OSR views before their compositor screenshots.
const TILE_MAX: usize = 8192;

/// One screenshot: the frame plus how many CDP captures it took.
pub struct Shot {
    pub frame: Frame,
    pub tiles: u64,
}

/// One attached page session.
pub struct Page {
    pub handle: PageHandle,
    pub state: Arc<Mutex<PageState>>,
    pub tick: watch::Receiver<u64>,
    pub intercept: Arc<Mutex<Option<Intercept>>>,
    /// The viewport set_viewport emulates (None: the 800x600 creation window).
    viewport: Mutex<Option<(i64, i64)>>,
}

/// JS `.slice(0, n)` over UTF-16 code units.
pub fn js_slice(s: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

/// puppeteer's console text for one Runtime.consoleAPICalled argument.
fn console_arg_text(arg: &Value) -> String {
    if arg.get("objectId").is_some() {
        let desc = arg.get("description").and_then(Value::as_str).unwrap_or("");
        if arg.get("subtype").and_then(Value::as_str) == Some("error") && !desc.is_empty() {
            return desc.split('\n').next().unwrap_or("").to_string();
        }
        let kind = arg.get("subtype").and_then(Value::as_str).or_else(|| arg.get("type").and_then(Value::as_str)).unwrap_or("");
        let class = arg.get("className").and_then(Value::as_str).unwrap_or("undefined");
        return format!("[{kind} {class}]");
    }
    if let Some(u) = arg.get("unserializableValue").and_then(Value::as_str) {
        return u.trim_end_matches('n').to_string();
    }
    match arg.get("value") {
        None | Some(Value::Null) => String::new(),
        Some(v) => js::js_string(v),
    }
}

/// puppeteer's getErrorDetails/createClientError message for an exception.
pub fn exception_message(details: &Value) -> String {
    let Some(exc) = details.get("exception") else {
        return details.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    };
    let is_error = exc.get("type").and_then(Value::as_str) == Some("object") && exc.get("subtype").and_then(Value::as_str) == Some("error");
    if !is_error && exc.get("objectId").is_none() {
        return console_arg_text(exc);
    }
    let desc = exc.get("description").and_then(Value::as_str).unwrap_or("");
    let mut lines: Vec<&str> = desc.split("\n    at ").collect();
    let frames = details.pointer("/stackTrace/callFrames").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
    let size = frames.min(lines.len().saturating_sub(1));
    if size > 0 {
        let keep = lines.len() - size;
        lines.truncate(keep);
    }
    let name = exc.get("className").and_then(Value::as_str).unwrap_or("");
    let mut message = lines.join("\n");
    let prefix = format!("{name}: ");
    if !name.is_empty() && message.starts_with(&prefix) {
        message = message[prefix.len()..].to_string();
    }
    if message.is_empty() {
        return if name.is_empty() { "Error".to_string() } else { name.to_string() };
    }
    message
}

fn status_text(code: i64) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// The device metrics puppeteer's setViewport emulated, plus an optional
/// visible-area override (the tile a full-page capture paints).
fn device_metrics(width: i64, height: i64, viewport: Option<Value>) -> Value {
    let mut p = json!({"mobile": false, "width": width, "height": height, "deviceScaleFactor": 1,
                       "screenOrientation": {"angle": 0, "type": "portraitPrimary"}});
    if let Some(v) = viewport {
        p["viewport"] = v;
    }
    p
}

impl Page {
    pub async fn attach(handle: PageHandle, rx: tokio::sync::mpsc::UnboundedReceiver<Event>) -> Result<Page> {
        let state = Arc::new(Mutex::new(PageState::default()));
        let (tick_tx, tick) = watch::channel(0u64);
        let intercept: Arc<Mutex<Option<Intercept>>> = Arc::new(Mutex::new(None));
        tokio::spawn(event_loop(handle.id, rx, state.clone(), tick_tx, intercept.clone()));
        let page = Page { handle, state, tick, intercept, viewport: Mutex::new(None) };
        page.send("Page.enable", json!({})).await?;
        let tree = page.send("Page.getFrameTree", json!({})).await?;
        {
            let mut st = page.state.lock().unwrap();
            let f = &tree["frameTree"]["frame"];
            st.main_frame = f["id"].as_str().unwrap_or("").to_string();
            st.url = f["url"].as_str().unwrap_or("").to_string();
            st.loader = f["loaderId"].as_str().unwrap_or("").to_string();
        }
        page.send("Page.setLifecycleEventsEnabled", json!({"enabled": true})).await?;
        page.send("Runtime.enable", json!({})).await?;
        page.send("Network.enable", json!({})).await?;
        page.send("Log.enable", json!({})).await?;
        Ok(page)
    }

    pub async fn send(&self, method: &str, params: Value) -> Result<Value> {
        if self.state.lock().unwrap().crashed {
            bail!("Page crashed!");
        }
        crate::cdp::send(self.handle.id, method, params).await
    }

    pub fn url(&self) -> String {
        self.state.lock().unwrap().url.clone()
    }

    /// Wait until `cond` holds over the page state, or the deadline passes.
    pub async fn wait_state<T>(&self, deadline: Option<Instant>, mut cond: impl FnMut(&PageState) -> Option<T>) -> Option<T> {
        let mut tick = self.tick.clone();
        loop {
            if let Some(v) = cond(&self.state.lock().unwrap()) {
                return Some(v);
            }
            let changed = tick.changed();
            match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return None;
                    }
                    // A periodic re-check covers time-based conditions (network idle).
                    let slice = (d - now).min(Duration::from_millis(100));
                    let _ = tokio::time::timeout(slice, changed).await;
                }
                None => {
                    let _ = tokio::time::timeout(Duration::from_millis(100), changed).await;
                }
            }
        }
    }

    /// puppeteer setViewport (EmulationManager.emulateViewport defaults). The
    /// OSR window is resized to the same size, since the window is what paints.
    pub async fn set_viewport(&self, width: i64, height: i64) -> Result<()> {
        cefhost::resize(&self.handle, width as i32, height as i32).await?;
        self.send("Emulation.setDeviceMetricsOverride", device_metrics(width, height, None)).await?;
        self.send("Emulation.setTouchEmulationEnabled", json!({"enabled": false})).await?;
        *self.viewport.lock().unwrap() = Some((width, height));
        Ok(())
    }

    /// Start recording console errors, page errors, failed requests and >=400 responses.
    pub fn observe(&self) {
        self.state.lock().unwrap().observe = true;
    }

    pub async fn set_intercept(&self, rules: Intercept) -> Result<()> {
        *self.intercept.lock().unwrap() = Some(rules);
        self.send("Network.setCacheDisabled", json!({"cacheDisabled": true})).await?;
        self.send("Fetch.enable", json!({"patterns": [{"urlPattern": "*"}], "handleAuthRequests": false})).await?;
        Ok(())
    }

    /// Runtime.evaluate by value. Ok(None) is JS `undefined`.
    pub async fn evaluate(&self, expr: &str) -> Result<Option<Value>> {
        let started = Instant::now();
        loop {
            let r = self
                .send("Runtime.evaluate", json!({"expression": expr, "returnByValue": true, "awaitPromise": true, "userGesture": true}))
                .await;
            match r {
                Ok(v) => return eval_result(&v),
                // Between a navigation's commit and its new context there is no
                // default context yet; puppeteer waits for it, so do we.
                Err(e) if e.to_string().contains("Cannot find default execution context") && started.elapsed() < Duration::from_secs(5) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Runtime.evaluate returning a remote object id (element handle), or None.
    pub async fn query_object(&self, expr: &str) -> Result<Option<String>> {
        let v = self.send("Runtime.evaluate", json!({"expression": expr, "returnByValue": false, "awaitPromise": true})).await?;
        if let Some(d) = v.get("exceptionDetails") {
            bail!("{}", exception_message(d));
        }
        Ok(v.pointer("/result/objectId").and_then(Value::as_str).map(str::to_string))
    }

    pub async fn call_on(&self, object_id: &str, func: &str, args: Vec<Value>) -> Result<Option<Value>> {
        let args: Vec<Value> = args.into_iter().map(|a| json!({"value": a})).collect();
        let v = self
            .send(
                "Runtime.callFunctionOn",
                json!({"objectId": object_id, "functionDeclaration": func, "arguments": args,
                       "returnByValue": true, "awaitPromise": true, "userGesture": true}),
            )
            .await?;
        eval_result(&v)
    }

    pub async fn release(&self, object_id: &str) {
        let _ = self.send("Runtime.releaseObject", json!({"objectId": object_id})).await;
    }

    /// Let the renderer commit the preceding page change before capture.
    /// No userGesture: a screenshot must not grant user activation.
    async fn settle_frames(&self) -> Result<()> {
        let v = self
            .send(
                "Runtime.evaluate",
                json!({"expression": "new Promise(r => requestAnimationFrame(() => requestAnimationFrame(() => r(true))))",
                       "returnByValue": true, "awaitPromise": true}),
            )
            .await?;
        if let Some(d) = v.get("exceptionDetails") {
            bail!("frame settle: {}", exception_message(d));
        }
        Ok(())
    }

    /// Capture the compositor's finished visible surface through CDP, then
    /// decode it into the BGRA frame consumed by OCR and the PNG writer.
    async fn capture_frame(&self, width: usize, height: usize, y: Option<usize>) -> Result<Frame> {
        self.settle_frames().await?;
        let mut params = json!({"format": "png", "fromSurface": true,
                                "captureBeyondViewport": false, "optimizeForSpeed": true});
        // CDP's clip bounds the PNG to the painted OSR tile and selects its
        // document offset without scrolling fixed elements into every tile.
        if let Some(y) = y {
            params["clip"] = json!({"x": 0, "y": y, "width": width, "height": height, "scale": 1});
        }
        let reply = self
            .send("Page.captureScreenshot", params)
            .await?;
        let encoded = reply.get("data").and_then(Value::as_str).ok_or_else(|| anyhow!("captureScreenshot returned no PNG data"))?;
        let png = base64::engine::general_purpose::STANDARD.decode(encoded)?;
        decode_screenshot(&png, width, height)
    }

    /// CEF's off-screen CDP capture repeats the physical viewport when asked
    /// for a single full-content clip. Resize that physical view and select each
    /// visible-area slice before capturing; stitch its BGRA rows into one frame.
    pub async fn screenshot(&self, full_page: bool) -> Result<Shot> {
        let (vw, vh) = self.viewport.lock().unwrap().unwrap_or((800, 600));
        if !full_page {
            return Ok(Shot { frame: self.capture_frame(vw as usize, vh as usize, None).await?, tiles: 1 });
        }
        let m = self.send("Page.getLayoutMetrics", json!({})).await?;
        let size = m.get("cssContentSize").or_else(|| m.get("contentSize")).ok_or_else(|| anyhow!("getLayoutMetrics returned no content size"))?;
        let dim = |k: &str| size.get(k).and_then(Value::as_f64).filter(|f| *f > 0.0).ok_or_else(|| anyhow!("content size has no {k}"));
        let (width, height) = (dim("width")?.ceil() as usize, dim("height")?.ceil() as usize);
        if width > TILE_MAX {
            bail!("full-page width {width} exceeds the {TILE_MAX}px compositor view; tiling is vertical only");
        }
        let stride = width.checked_mul(4).ok_or_else(|| anyhow!("BGRA row size overflow"))?;
        let len = stride.checked_mul(height).ok_or_else(|| anyhow!("BGRA frame size overflow"))?;
        let seg = Segment::create(len)?;
        let mut tiles = 0u64;
        let captured = async {
            for y in (0..height).step_by(TILE_MAX) {
                let th = (height - y).min(TILE_MAX);
                cefhost::resize(&self.handle, width as i32, th as i32).await?;
                let visible = json!({"x": 0, "y": y, "width": width, "height": th, "scale": 1});
                self.send("Emulation.setDeviceMetricsOverride", device_metrics(width as i64, height as i64, Some(visible))).await?;
                let tile = self.capture_frame(width, th, Some(y)).await?;
                let start = y * stride;
                seg.as_mut_slice()[start..start + tile.seg.len()].copy_from_slice(tile.seg.as_slice());
                tiles += 1;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        // Restore the page's original layout even when one tile fails.
        let restored = async {
            cefhost::resize(&self.handle, vw as i32, vh as i32).await?;
            let vp = *self.viewport.lock().unwrap();
            match vp {
                Some((w, h)) => self.send("Emulation.setDeviceMetricsOverride", device_metrics(w, h, None)).await.map(|_| ()),
                None => self.send("Emulation.clearDeviceMetricsOverride", json!({})).await.map(|_| ()),
            }
        }
        .await;
        captured?;
        restored?;
        Ok(Shot { frame: Frame { seg, width, height, stride }, tiles })
    }

    /// The response body text as puppeteer's HTTPResponse.text() decodes it.
    pub async fn response_text(&self, request_id: &str) -> Result<String> {
        let r = self.send("Network.getResponseBody", json!({"requestId": request_id})).await?;
        let body = r.get("body").and_then(Value::as_str).unwrap_or("");
        let text = if r.get("base64Encoded").and_then(Value::as_bool).unwrap_or(false) {
            let bytes = base64::engine::general_purpose::STANDARD.decode(body)?;
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            body.to_string()
        };
        Ok(text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text))
    }

    /// The headers puppeteer reports for a response: the raw extra-info
    /// headers when they arrived, lowercased.
    pub fn response_headers(&self, resp: &DocResponse) -> Value {
        let st = self.state.lock().unwrap();
        let raw = st
            .extra
            .get(&resp.request_id)
            .and_then(|v| v.iter().rev().find(|(s, _)| *s == resp.status).map(|(_, h)| h.clone()))
            .unwrap_or_else(|| resp.headers.clone());
        let mut out = Map::new();
        if let Some(obj) = raw.as_object() {
            for (k, v) in obj {
                out.insert(k.to_lowercase(), v.clone());
            }
        }
        Value::Object(out)
    }

    // ---- input ----

    pub async fn mouse_click(&self, x: f64, y: f64) -> Result<()> {
        let mv = self.send("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y, "button": "none", "buttons": 0, "modifiers": 0}));
        let down = self.send(
            "Input.dispatchMouseEvent",
            json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1, "modifiers": 0}),
        );
        let up = self.send(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1, "modifiers": 0}),
        );
        let (a, b, c) = tokio::join!(mv, down, up);
        a?;
        b?;
        c?;
        Ok(())
    }

    pub async fn key_press(&self, key: &str, delay_ms: u64) -> Result<()> {
        let def = crate::keys::lookup(key).ok_or_else(|| anyhow!("Unknown key: \"{key}\""))?;
        let mut down = json!({
            "type": if def.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "modifiers": 0, "windowsVirtualKeyCode": def.key_code, "code": def.code, "key": def.key,
            "autoRepeat": false, "location": def.location, "isKeypad": def.location == 3,
        });
        if let Some(t) = &def.text {
            down["text"] = json!(t);
            down["unmodifiedText"] = json!(t);
        }
        self.send("Input.dispatchKeyEvent", down).await?;
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        self.send(
            "Input.dispatchKeyEvent",
            json!({"type": "keyUp", "modifiers": 0, "key": def.key, "windowsVirtualKeyCode": def.key_code, "code": def.code, "location": def.location}),
        )
        .await?;
        Ok(())
    }

    /// puppeteer keyboard.type: known keys are pressed, anything else is inserted.
    pub async fn type_text(&self, text: &str, delay_ms: u64) -> Result<()> {
        for c in text.chars() {
            let s = c.to_string();
            if crate::keys::lookup(&s).is_some() {
                self.key_press(&s, delay_ms).await?;
            } else {
                self.send("Input.insertText", json!({"text": s})).await?;
            }
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
        Ok(())
    }
}

/// Decode Chromium's PNG once; preserve the existing BGRA memfd contract.
fn decode_screenshot(bytes: &[u8], width: usize, height: usize) -> Result<Frame> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let header = decoder.read_header_info()?;
    if (header.width as usize, header.height as usize) != (width, height) {
        bail!("captureScreenshot returned {}x{}, expected {width}x{height}", header.width, header.height);
    }
    // The default decoder limit is 64 MiB; the requested CSS clip can exceed it.
    decoder.set_limits(png::Limits { bytes: usize::MAX });
    let mut reader = decoder.read_info()?;
    let mut pixels = vec![0; reader.output_buffer_size().ok_or_else(|| anyhow!("PNG output size overflow"))?];
    let info = reader.next_frame(&mut pixels)?;
    let stride = width.checked_mul(4).ok_or_else(|| anyhow!("BGRA row size overflow"))?;
    let len = stride.checked_mul(height).ok_or_else(|| anyhow!("BGRA frame size overflow"))?;
    let seg = Segment::create(len)?;
    let out = seg.as_mut_slice();
    match info.color_type {
        png::ColorType::Rgb => {
            for (dst, src) in out.chunks_exact_mut(4).zip(pixels[..info.buffer_size()].chunks_exact(3)) {
                dst.copy_from_slice(&[src[2], src[1], src[0], 255]);
            }
        }
        png::ColorType::Rgba => {
            for (dst, src) in out.chunks_exact_mut(4).zip(pixels[..info.buffer_size()].chunks_exact(4)) {
                dst.copy_from_slice(&[src[2], src[1], src[0], src[3]]);
            }
        }
        other => bail!("captureScreenshot returned unsupported PNG color type {other:?}"),
    }
    Ok(Frame { seg, width, height, stride })
}

fn eval_result(v: &Value) -> Result<Option<Value>> {
    if let Some(d) = v.get("exceptionDetails") {
        bail!("{}", exception_message(d));
    }
    let r = &v["result"];
    if r.get("type").and_then(Value::as_str) == Some("undefined") {
        return Ok(None);
    }
    if let Some(u) = r.get("unserializableValue").and_then(Value::as_str) {
        // NaN/Infinity serialize to null in the JSON reply; -0 is 0.
        return Ok(Some(if u == "-0" { json!(0) } else { Value::Null }));
    }
    Ok(Some(r.get("value").cloned().unwrap_or(Value::Null)))
}

async fn event_loop(
    page: i32,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    state: Arc<Mutex<PageState>>,
    tick: watch::Sender<u64>,
    intercept: Arc<Mutex<Option<Intercept>>>,
) {
    let mut n = 0u64;
    while let Some(ev) = rx.recv().await {
        let p = &ev.params;
        match ev.method.as_str() {
            "Fetch.requestPaused" => handle_paused(page, p, &state, &intercept),
            _ => apply_event(&mut state.lock().unwrap(), &ev.method, p),
        }
        n += 1;
        let _ = tick.send(n);
    }
    state.lock().unwrap().crashed = true;
    let _ = tick.send(n + 1);
}

fn apply_event(st: &mut PageState, method: &str, p: &Value) {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match method {
        "Page.frameNavigated" => {
            let f = &p["frame"];
            if f.get("parentId").is_none() {
                st.main_frame = s(f, "id");
                st.url = format!("{}{}", s(f, "url"), s(f, "urlFragment"));
                st.loader = s(f, "loaderId");
            }
        }
        "Page.navigatedWithinDocument" => {
            if s(p, "frameId") == st.main_frame {
                st.url = s(p, "url");
                st.same_doc += 1;
            }
        }
        "Page.lifecycleEvent" => {
            if s(p, "frameId") == st.main_frame {
                let loader = s(p, "loaderId");
                let name = s(p, "name");
                if name == "init" {
                    st.lifecycle.insert(loader.clone(), HashSet::new());
                }
                st.lifecycle.entry(loader).or_default().insert(name);
            }
        }
        "Network.requestWillBeSent" => {
            let id = s(p, "requestId");
            st.req_url.insert(id.clone(), s(&p["request"], "url"));
            st.inflight.insert(id);
            st.last_net = Some(Instant::now());
        }
        "Network.responseReceived" => {
            let r = &p["response"];
            let status = r.get("status").and_then(Value::as_f64).unwrap_or(0.0) as i64;
            if st.observe && status >= 400 {
                st.http_errors.push(json!({"url": js_slice(&s(r, "url"), 2000), "status": status}));
            }
            if s(p, "type") == "Document" && s(p, "frameId") == st.main_frame {
                st.doc_resp.insert(
                    s(p, "loaderId"),
                    DocResponse { request_id: s(p, "requestId"), status, url: s(r, "url"), headers: r.get("headers").cloned().unwrap_or(json!({})) },
                );
            }
        }
        "Network.responseReceivedExtraInfo" => {
            let status = p.get("statusCode").and_then(Value::as_f64).unwrap_or(0.0) as i64;
            st.extra.entry(s(p, "requestId")).or_default().push((status, p.get("headers").cloned().unwrap_or(json!({}))));
        }
        "Network.loadingFinished" => {
            st.inflight.remove(&s(p, "requestId"));
            st.last_net = Some(Instant::now());
        }
        "Network.loadingFailed" => {
            let id = s(p, "requestId");
            st.inflight.remove(&id);
            st.last_net = Some(Instant::now());
            if st.observe {
                let url = st.req_url.get(&id).cloned().unwrap_or_default();
                st.failed_requests.push(json!({"url": js_slice(&url, 2000), "error": s(p, "errorText")}));
            }
        }
        "Runtime.consoleAPICalled" => {
            if st.observe && s(p, "type") == "error" {
                let args = p.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
                let text = args.iter().map(console_arg_text).collect::<Vec<_>>().join(" ");
                st.console_errors.push(js_slice(&text, 2000));
            }
        }
        "Log.entryAdded" => {
            let e = &p["entry"];
            if st.observe && s(e, "level") == "error" && s(e, "source") != "worker" {
                st.console_errors.push(js_slice(&s(e, "text"), 2000));
            }
        }
        "Runtime.exceptionThrown" => {
            if st.observe {
                let m = exception_message(&p["exceptionDetails"]);
                st.console_errors.push(format!("pageerror: {}", js_slice(&m, 2000)));
            }
        }
        "Inspector.targetCrashed" => st.crashed = true,
        _ => {}
    }
}

fn handle_paused(page: i32, p: &Value, state: &Arc<Mutex<PageState>>, intercept: &Arc<Mutex<Option<Intercept>>>) {
    let request_id = p.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
    let url = p.pointer("/request/url").and_then(Value::as_str).unwrap_or("").to_string();
    let (method, params) = {
        let guard = intercept.lock().unwrap();
        match guard.as_ref() {
            None => ("Fetch.continueRequest", json!({"requestId": request_id})),
            Some(Intercept::Block { patterns }) => {
                let main = state.lock().unwrap().main_frame.clone();
                let nav = p.get("resourceType").and_then(Value::as_str) == Some("Document")
                    && p.get("frameId").and_then(Value::as_str) == Some(main.as_str());
                if !nav && patterns.iter().any(|pat| url.contains(pat.as_str())) {
                    ("Fetch.failRequest", json!({"requestId": request_id, "errorReason": "Failed"}))
                } else {
                    ("Fetch.continueRequest", json!({"requestId": request_id}))
                }
            }
            Some(Intercept::Browse { abort, fulfill, header_rules }) => {
                if abort.iter().any(|re| re.is_match(&url)) {
                    ("Fetch.failRequest", json!({"requestId": request_id, "errorReason": "Failed"}))
                } else if let Some(f) = fulfill.iter().find(|f| f.re.is_match(&url)) {
                    let headers = json!([
                        {"name": "content-type", "value": f.content_type},
                        {"name": "content-length", "value": f.body.len().to_string()},
                    ]);
                    let mut params = json!({"requestId": request_id, "responseCode": f.status, "responseHeaders": headers,
                                            "body": base64::engine::general_purpose::STANDARD.encode(&f.body)});
                    let phrase = status_text(f.status);
                    if !phrase.is_empty() {
                        params["responsePhrase"] = json!(phrase);
                    }
                    ("Fetch.fulfillRequest", params)
                } else {
                    let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(|h| h.to_lowercase())).unwrap_or_default();
                    match header_rules.iter().find(|(h, _)| *h == host) {
                        Some((_, extra)) => {
                            let mut merged = Map::new();
                            if let Some(obj) = p.pointer("/request/headers").and_then(Value::as_object) {
                                for (k, v) in obj {
                                    merged.insert(k.to_lowercase(), v.clone());
                                }
                            }
                            for (k, v) in extra {
                                merged.insert(k.clone(), v.clone());
                            }
                            let arr: Vec<Value> = merged
                                .into_iter()
                                .map(|(k, v)| json!({"name": k, "value": match v { Value::String(s) => s, other => js::js_string(&other) }}))
                                .collect();
                            ("Fetch.continueRequest", json!({"requestId": request_id, "headers": arr}))
                        }
                        None => ("Fetch.continueRequest", json!({"requestId": request_id})),
                    }
                }
            }
        }
    };
    // A request the page cancelled meanwhile fails this call; that is harmless.
    tokio::spawn(async move {
        let _ = crate::cdp::send(page, method, params).await;
    });
}
