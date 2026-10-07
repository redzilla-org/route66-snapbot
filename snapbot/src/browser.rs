//! Chromium process lifecycle and one CDP page session with its event state.
//!
//! The launch argv reproduces what the Node handler ran: puppeteer-core
//! 25.9.0's default switches plus @sparticuz/chromium 149's serverless set, so
//! screenshots render with the same flags (hidden scrollbars, sRGB, no font
//! hinting) as the baselines route66 compares against.

use crate::cdp::{Cdp, Event};
use crate::js;
use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

/// The Chromium payload baked into the image (Dockerfile): the sparticuz
/// binary, its swiftshader libraries beside it, fonts/ and al2023/lib.
fn chromium_dir() -> PathBuf {
    PathBuf::from(std::env::var("SNAPBOT_CHROMIUM_DIR").unwrap_or_else(|_| "/opt/chromium".to_string()))
}

/// @sparticuz/chromium 149 `args` (graphics mode on).
fn sparticuz_args() -> Vec<String> {
    [
        "--ash-no-nudges",
        "--disable-domain-reliability",
        "--disable-print-preview",
        "--disk-cache-size=33554432",
        "--no-default-browser-check",
        "--no-pings",
        "--single-process",
        "--font-render-hinting=none",
        "--disable-features=AudioServiceOutOfProcess,IsolateOrigins,site-per-process",
        "--enable-features=SharedArrayBuffer",
        "--ignore-gpu-blocklist",
        "--in-process-gpu",
        "--use-gl=angle",
        "--use-angle=swiftshader",
        "--enable-unsafe-swiftshader",
        "--allow-running-insecure-content",
        "--disable-setuid-sandbox",
        "--disable-site-isolation-trials",
        "--disable-web-security",
        "--headless='shell'",
        "--no-sandbox",
        "--no-zygote",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Extract and remove every `<flag>=a,b` feature list from `args`.
fn take_features(args: &mut Vec<String>, flag: &str) -> Vec<String> {
    let prefix = format!("{flag}=");
    let mut out = Vec::new();
    args.retain(|a| {
        if let Some(rest) = a.strip_prefix(&prefix) {
            out.extend(rest.trim().split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty()));
            false
        } else {
            true
        }
    });
    out
}

/// puppeteer-core 25.9.0 ChromeLauncher.defaultArgs + computeLaunchArguments,
/// with headless "shell", over the given user args.
fn chrome_argv(mut user: Vec<String>, user_data_dir: &str) -> Vec<String> {
    let user_disabled = take_features(&mut user, "--disable-features");
    let user_enabled = take_features(&mut user, "--enable-features");
    let mut enabled = vec!["PdfOopif".to_string()];
    enabled.extend(user_enabled);
    let mut disabled: Vec<String> = [
        "Translate",
        "AcceptCHFrame",
        "MediaRouter",
        "OptimizationHints",
        "WebUIReloadButton",
        "WebUIOmniboxPopup",
        "WebUIOmniboxAimPopup",
        "ProcessPerSiteUpToMainFrameThreshold",
        "IsolateSandboxedIframes",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    disabled.extend(user_disabled);
    disabled.retain(|f| !enabled.contains(f));
    let mut argv: Vec<String> = [
        "--allow-pre-commit-input",
        "--disable-background-networking",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-breakpad",
        "--disable-client-side-phishing-detection",
        "--disable-component-extensions-with-background-pages",
        "--disable-crash-reporter",
        "--disable-default-apps",
        "--disable-dev-shm-usage",
        "--disable-hang-monitor",
        "--disable-infobars",
        "--disable-ipc-flooding-protection",
        "--disable-popup-blocking",
        "--disable-prompt-on-repost",
        "--disable-renderer-backgrounding",
        "--disable-search-engine-choice-screen",
        "--disable-sync",
        "--enable-automation",
        "--export-tagged-pdf",
        "--force-color-profile=srgb",
        "--generate-pdf-document-outline",
        "--metrics-recording-only",
        "--no-first-run",
        "--password-store=basic",
        "--use-mock-keychain",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    argv.push(format!("--disable-features={}", disabled.join(",")));
    argv.push(format!("--enable-features={}", enabled.join(",")));
    argv.push("--headless".to_string());
    argv.push("--hide-scrollbars".to_string());
    argv.push("--mute-audio".to_string());
    argv.push("--disable-extensions".to_string());
    if user.iter().all(|a| a.starts_with('-')) {
        argv.push("about:blank".to_string());
    }
    argv.extend(user);
    argv.push("--remote-debugging-port=0".to_string());
    argv.push(format!("--user-data-dir={user_data_dir}"));
    argv
}

/// One running Chromium and its DevTools connection.
pub struct Browser {
    pub cdp: Arc<Cdp>,
    child: tokio::process::Child,
    user_data_dir: PathBuf,
}

pub struct LaunchOptions {
    /// Keep sparticuz's --single-process (the one-shot evidence capture) or
    /// drop it (the pool browser, which recycles browser contexts: in single
    /// process mode a second context crashes the browser).
    pub single_process: bool,
    /// Browser-wide certificate-error bypass (the pool's acceptInsecureCerts).
    pub ignore_https_errors: bool,
    pub extra_args: Vec<String>,
}

impl Browser {
    pub async fn launch(opts: LaunchOptions) -> Result<Browser> {
        let dir = chromium_dir();
        let exe = std::env::var("SNAPBOT_CHROMIUM").map(PathBuf::from).unwrap_or_else(|_| dir.join("chromium"));
        let mut user = sparticuz_args();
        if !opts.single_process {
            user.retain(|a| a != "--single-process");
        }
        user.extend(opts.extra_args.iter().cloned());
        let user_data_dir = std::env::temp_dir().join(format!("puppeteer_dev_chrome_profile-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&user_data_dir)?;
        let argv = chrome_argv(user, &user_data_dir.to_string_lossy());

        // The sparticuz payload's environment (its setupLambdaEnvironment),
        // scoped to the browser process only.
        let lib = dir.join("al2023").join("lib");
        let ld = match std::env::var("LD_LIBRARY_PATH") {
            Ok(v) if !v.is_empty() => format!("{}:{v}", lib.display()),
            _ => lib.display().to_string(),
        };
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.args(&argv)
            .env("LD_LIBRARY_PATH", ld)
            .env("FONTCONFIG_PATH", std::env::var("FONTCONFIG_PATH").unwrap_or_else(|_| dir.join("fonts").display().to_string()))
            .env("HOME", std::env::var("HOME").unwrap_or_else(|_| std::env::temp_dir().display().to_string()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| anyhow!("launch {}: {e}", exe.display()))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("chromium stderr unavailable"))?;
        let mut lines = BufReader::new(stderr).lines();
        let mut seen = Vec::new();
        let ws = tokio::time::timeout(Duration::from_secs(30), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(i) = line.find("ws://") {
                    if line.contains("DevTools listening on") {
                        return Some(line[i..].trim().to_string());
                    }
                }
                if seen.len() < 40 {
                    seen.push(line);
                }
            }
            None
        })
        .await;
        let ws = match ws {
            Ok(Some(ws)) => ws,
            Ok(None) => bail!("Failed to launch the browser process!\n{}", seen.join("\n")),
            Err(_) => bail!("Timed out after 30000 ms while waiting for the WS endpoint URL to appear in stdout!"),
        };
        // Keep draining stderr so a chatty browser can never block on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        let cdp = Cdp::connect(&ws).await?;
        if opts.ignore_https_errors {
            cdp.send(None, "Security.setIgnoreCertificateErrors", json!({"ignore": true})).await?;
        }
        Ok(Browser { cdp, child, user_data_dir })
    }

    pub fn connected(&self) -> bool {
        !self.cdp.is_closed()
    }

    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.cdp.send(None, "Browser.close", json!({}))).await;
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }

    pub async fn new_page(&self, context: Option<&str>) -> Result<Page> {
        new_page(self.cdp.clone(), context).await
    }
}

/// Target.createBrowserContext: a fresh cookie jar and cache per request.
pub async fn create_context(cdp: &Cdp) -> Result<String> {
    let r = cdp.send(None, "Target.createBrowserContext", json!({})).await?;
    r.get("browserContextId").and_then(Value::as_str).map(str::to_string).ok_or_else(|| anyhow!("createBrowserContext returned no id"))
}

pub async fn dispose_context(cdp: &Cdp, id: &str) -> Result<()> {
    cdp.send(None, "Target.disposeBrowserContext", json!({"browserContextId": id})).await?;
    Ok(())
}

/// A new about:blank page in `context` (the default context when None).
pub async fn new_page(cdp: Arc<Cdp>, context: Option<&str>) -> Result<Page> {
    let mut p = json!({"url": "about:blank"});
    if let Some(c) = context {
        p["browserContextId"] = Value::String(c.to_string());
    }
    let t = cdp.send(None, "Target.createTarget", p).await?;
    let target_id = t.get("targetId").and_then(Value::as_str).unwrap_or("").to_string();
    let a = cdp.send(None, "Target.attachToTarget", json!({"targetId": target_id, "flatten": true})).await?;
    let session = a.get("sessionId").and_then(Value::as_str).unwrap_or("").to_string();
    Page::attach(cdp, session, target_id).await
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

/// WHY (route66 GH #4082): the last screenshot's (CDP round-trip ms, base64 decode ms).
/// A lane process drives one page at a time, so the caller that just took the shot
/// reads its own numbers. Measurement only.
pub static LAST_CAPTURE: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// One attached page session.
pub struct Page {
    pub cdp: Arc<Cdp>,
    pub session: String,
    pub target_id: String,
    pub state: Arc<Mutex<PageState>>,
    pub tick: watch::Receiver<u64>,
    pub intercept: Arc<Mutex<Option<Intercept>>>,
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

impl Page {
    pub async fn attach(cdp: Arc<Cdp>, session: String, target_id: String) -> Result<Page> {
        let rx = cdp.subscribe(&session);
        let state = Arc::new(Mutex::new(PageState::default()));
        let (tick_tx, tick) = watch::channel(0u64);
        let intercept: Arc<Mutex<Option<Intercept>>> = Arc::new(Mutex::new(None));
        tokio::spawn(event_loop(cdp.clone(), session.clone(), rx, state.clone(), tick_tx, intercept.clone()));
        let page = Page { cdp, session, target_id, state, tick, intercept };
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
        self.cdp.send(Some(&self.session), method, params).await
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

    /// puppeteer setViewport (EmulationManager.emulateViewport defaults).
    pub async fn set_viewport(&self, width: i64, height: i64) -> Result<()> {
        self.send(
            "Emulation.setDeviceMetricsOverride",
            json!({"mobile": false, "width": width, "height": height, "deviceScaleFactor": 1,
                   "screenOrientation": {"angle": 0, "type": "portraitPrimary"}}),
        )
        .await?;
        self.send("Emulation.setTouchEmulationEnabled", json!({"enabled": false})).await?;
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

    /// Page.captureScreenshot: png, optimizeForSpeed (owner 2026-09-29: request
    /// the fast encoder). A full-page shot clips to the CSS content size with
    /// captureBeyondViewport, as puppeteer does; without the clip Chromium
    /// returns only the viewport.
    pub async fn screenshot(&self, full_page: bool) -> Result<Vec<u8>> {
        let mut p = json!({"format": "png", "optimizeForSpeed": true, "fromSurface": true, "captureBeyondViewport": full_page});
        if full_page {
            let m = self.send("Page.getLayoutMetrics", json!({})).await?;
            let size = m.get("cssContentSize").or_else(|| m.get("contentSize")).ok_or_else(|| anyhow!("getLayoutMetrics returned no content size"))?;
            let dim = |k: &str| size.get(k).and_then(Value::as_f64).filter(|f| *f > 0.0).ok_or_else(|| anyhow!("content size has no {k}"));
            p["clip"] = json!({"x": 0, "y": 0, "width": dim("width")?.ceil(), "height": dim("height")?.ceil(), "scale": 1});
        }
        // WHY (route66 GH #4082, owner 2026-10-07: "is chromium spending time on compressing
        // PNGs?"): split the CDP round trip (Chromium's raster plus PNG encode) from the
        // base64 decode here. The caller logs them once it knows where the PNG was stored
        // (LAST_CAPTURE). Measurement only.
        let t = Instant::now();
        let r = self.send("Page.captureScreenshot", p).await?;
        let cdp_ms = t.elapsed().as_millis() as u64;
        let data = r.get("data").and_then(Value::as_str).ok_or_else(|| anyhow!("captureScreenshot returned no data"))?;
        let t = Instant::now();
        let png = base64::engine::general_purpose::STANDARD.decode(data)?;
        *LAST_CAPTURE.lock().unwrap() = (cdp_ms, t.elapsed().as_millis() as u64);
        Ok(png)
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
    cdp: Arc<Cdp>,
    session: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    state: Arc<Mutex<PageState>>,
    tick: watch::Sender<u64>,
    intercept: Arc<Mutex<Option<Intercept>>>,
) {
    let mut n = 0u64;
    while let Some(ev) = rx.recv().await {
        let p = &ev.params;
        match ev.method.as_str() {
            "Fetch.requestPaused" => handle_paused(&cdp, &session, p, &state, &intercept),
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

fn handle_paused(cdp: &Arc<Cdp>, session: &str, p: &Value, state: &Arc<Mutex<PageState>>, intercept: &Arc<Mutex<Option<Intercept>>>) {
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
    let cdp = cdp.clone();
    let session = session.to_string();
    // A request the page cancelled meanwhile fails this call; that is harmless.
    tokio::spawn(async move {
        let _ = cdp.send(Some(&session), method, params).await;
    });
}
