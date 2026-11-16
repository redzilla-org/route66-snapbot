//! Chromium embedded in this process: CEF windowless (off-screen) rendering.
//!
//! WHY (owner 2026-10-07, verbatim: "this sounds perfect: Embed Chromium in
//! snapbot ... hands over the raw pixel buffer in the same process"). The page
//! paints into CEF's off-screen view; OnPaint drives the paint generation the
//! orchestrator waits on. A screenshot is ONE Page.captureScreenshot from the
//! compositor's current surface, decoded into the BGRA frame (route66 GH
//! #4486: frames built from OnPaint's accumulated dirty rects froze stale and
//! composite views). CDP commands stay in process through
//! CefBrowserHost::SendDevToolsMessage and a DevToolsMessageObserver.
//!
//! WHY THE `cef` CRATE (tauri-apps/cef-rs): it ships pre-generated Rust bindings
//! for the C API plus safe ref-counted wrappers and the `wrap_*!` handler macros,
//! and its build script downloads the matching CEF binary distribution. Owner
//! 2026-10-07: all code we write is Rust; no C/C++ shims of our own.
//!
//! THREADING: CEF runs its UI message loop on the process main thread
//! (`run_message_loop`); the tokio runtime runs on another thread. Every CEF
//! object lives only on the UI thread (thread-local `UI`); tokio code reaches it
//! by posting closures (`on_ui`) and gets answers through channels.

use crate::shm::Frame;
use anyhow::{anyhow, Result};
use base64::Engine as _;
use cef::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// One CDP event from a page, routed to its session.
#[derive(Clone, Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
}

/// The per-page OSR view size GetViewRect reads for layout, and the paint
/// generation the orchestrator awaits ("a step finishes when its result is
/// painted", route66 GH #4486).
pub struct Slot {
    id: AtomicI32,
    view: Mutex<(i32, i32)>,
    /// The paint generation: bumped after every VIEW paint.
    painted: tokio::sync::watch::Sender<u64>,
    /// The size of the latest VIEW paint (a view change completes at its new size).
    painted_size: Mutex<(i32, i32)>,
}

/// The image an armed capture takes: `width`x`height` BGRA rows from the top
/// of the visual viewport. A height above the emulated viewport's (a
/// full_page shot of a taller capture view) is clipped at the viewport's
/// document offset; otherwise CDP returns the emulated viewport as is.
#[derive(Clone, Copy)]
pub struct ShotSize {
    pub width: usize,
    pub height: usize,
    pub viewport_height: usize,
}

/// A capture waiting for its step's completing event, owned by the CEF UI
/// thread alone (plain fields). Its ready point (reply, condition, render
/// call, goto lifecycle, posted, unarmed) sends ONE Page.captureScreenshot
/// from the compositor's current surface (shoot). WHY (route66 GH #4486): the
/// OSR dirty-rect view froze stale frames (local-verify 20261116T070021Z
/// california/CTH/workflow-saved-search-newsfeed: the first paint;
/// 20261116T064055Z chicago/CTC/content-page: the redirect stub), and
/// a1f0817's invalidate-then-next-paint swap froze composites of different
/// moments (20261116T072458Z california/DMV/map-picker-unobscured-centering:
/// chips present, 290px filter sidebar missing, page-load map tiles).
struct Armed {
    /// This capture's identity: a surface reply completes only the capture
    /// that sent it (a disarm and re-arm can land while one is in flight).
    token: u64,
    size: ShotSize,
    tx: oneshot::Sender<Result<Frozen>>,
    ready: bool,
    /// How `ready` was set: "reply", "truthy", "lifecycle", "posted", ...
    by: &'static str,
    ready_at: Option<Instant>,
    /// A goto's completing lifecycle events: (wait_until name, the navigate's
    /// loaderId, its main frame id), and which of them fired.
    lifecycle: Option<(String, String, String)>,
    seen_wait: bool,
    seen_fcp: bool,
    /// The surface capture was sent: later ready events send no second one.
    shooting: bool,
    /// A condition poll is in flight (start_poll).
    polling: bool,
    /// A wait_for_* condition polled per animation frame (Completes::Condition).
    cond: Option<String>,
    /// The __r66Rendered payload that completes (Completes::Rendered).
    rendered: Option<String>,
}

/// Which surface-capture message a CDP reply answers: the visual viewport's
/// document offset (a clipped shot's first message), or the PNG itself.
#[derive(Clone, Copy)]
enum ShotStage {
    Metrics,
    Capture,
}

/// Which CDP message completes an armed page's step (set by the orchestrator
/// for the step's duration).
#[derive(Clone)]
pub enum Completes {
    /// The reply to the step's Runtime.evaluate (an `evaluate` step).
    Reply,
    /// A Runtime.evaluate reply whose value is truthy (a polled condition).
    Truthy,
    /// Page.navigate's reply names the loader; its `name` lifecycle event completes.
    Lifecycle(String),
    /// A wait_for_* condition, polled in the page from arm time once per
    /// animation frame (start_poll); its first truthy value completes.
    Condition(String),
    /// The page's __r66Rendered(name) call: the observer sends the surface
    /// capture from that callback.
    Rendered(String),
}

/// A captured surface handed to an image's readers, and what it cost.
pub struct Frozen {
    pub frame: Frame,
    /// What made the capture ready, and ready -> the surface capture's reply, in ms.
    pub ready_by: &'static str,
    pub ready_to_capture_ms: u64,
    /// The reply's JSON, base64 and PNG decode into the BGRA frame, in ms,
    /// and the PNG's size.
    pub decode_ms: u64,
    pub png_bytes: u64,
}

/// Cross-thread routing: CDP replies by message id, events by browser id.
#[derive(Default)]
struct Hub {
    pending: Mutex<HashMap<i32, oneshot::Sender<std::result::Result<Value, String>>>>,
    sessions: Mutex<HashMap<i32, mpsc::UnboundedSender<Event>>>,
    closed: Mutex<HashMap<i32, oneshot::Sender<()>>>,
    next_msg: AtomicI32,
    /// Per page: which message completes its current armed step (see Completes).
    completes: Mutex<HashMap<i32, Completes>>,
    quitting: AtomicBool,
}

fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(|| Hub { next_msg: AtomicI32::new(1), ..Default::default() })
}

/// UI-thread-only CEF objects.
struct UiBrowser {
    browser: Browser,
    context: String,
    _registration: Registration,
}

#[derive(Default)]
struct Ui {
    browsers: HashMap<i32, UiBrowser>,
    contexts: HashMap<String, RequestContext>,
}

thread_local! {
    static UI: RefCell<Ui> = RefCell::new(Ui::default());
    /// Each page's armed capture, by browser id (UI thread only; see Armed).
    static ARMED: RefCell<HashMap<i32, Armed>> = RefCell::new(HashMap::new());
    /// In-flight surface-capture messages: msg id -> (page, capture token, stage).
    static SHOTS: RefCell<HashMap<i32, (i32, u64, ShotStage)>> = RefCell::new(HashMap::new());
    /// In-flight CDP messages that complete an armed step: msg id -> (page, how).
    static COMPLETING: RefCell<HashMap<i32, (i32, Completes, String)>> = RefCell::new(HashMap::new());
    /// In-flight condition polls: msg id -> (page, capture token).
    static CHECKS: RefCell<HashMap<i32, (i32, u64)>> = RefCell::new(HashMap::new());
    /// Each page's __r66Rendered payloads since its main-frame document committed.
    static RENDERED: RefCell<HashMap<i32, Vec<String>>> = RefCell::new(HashMap::new());
}

/// A CEF (UTF-16) string as a Rust String.
fn cs(s: &CefString) -> String {
    CefStringUtf8::from(s).as_str().unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------
// Posting work to the UI thread
// ---------------------------------------------------------------------------

type UiFn = Box<dyn FnOnce() + Send>;

wrap_task! {
    struct UiTask {
        f: Arc<Mutex<Option<UiFn>>>,
    }

    impl Task {
        fn execute(&self) {
            if let Some(f) = self.f.lock().unwrap().take() {
                f();
            }
        }
    }
}

/// Run `f` on the CEF UI thread. A refused post means CEF is gone: fail hard.
fn on_ui(f: impl FnOnce() + Send + 'static) {
    let mut task = UiTask::new(Arc::new(Mutex::new(Some(Box::new(f) as UiFn))));
    if post_task(ThreadId::UI, Some(&mut task)) != 1 {
        eprintln!("snapbot fatal: CEF refused a UI-thread task (browser process shut down)");
        std::process::exit(1);
    }
}

/// Run `f` on the UI thread and await its result.
async fn ui_call<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    let (tx, rx) = oneshot::channel();
    on_ui(move || {
        let _ = tx.send(f());
    });
    rx.await.map_err(|_| anyhow!("CEF UI task dropped its reply"))
}

// ---------------------------------------------------------------------------
// Process entry: subprocess dispatch, initialize, message loop
// ---------------------------------------------------------------------------

/// The CEF runtime directory: libcef.so, its .pak/.bin/.dat resources, locales/.
fn cef_dir() -> String {
    std::env::var("SNAPBOT_CEF_DIR").unwrap_or_else(|_| "/opt/snapbot/cef".to_string())
}

/// The Chromium switches every page renders with. They reproduce the former
/// launch argv's rendering-relevant set (puppeteer-core 25.9.0 defaults plus
/// @sparticuz/chromium 149's serverless set: hidden scrollbars, sRGB, no font
/// hinting, no background throttling), so pixels match the baselines route66
/// compares against; plus the windowless-only `ozone-platform=headless` (no
/// display server exists in Lambda or the pool container).
fn base_switches() -> Vec<String> {
    [
        "--ozone-platform=headless",
        "--no-sandbox",
        "--no-zygote",
        "--disable-dev-shm-usage",
        "--hide-scrollbars",
        "--mute-audio",
        "--force-color-profile=srgb",
        "--font-render-hinting=none",
        "--disable-background-networking",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-renderer-backgrounding",
        "--disable-ipc-flooding-protection",
        "--disable-hang-monitor",
        "--disable-breakpad",
        "--disable-crash-reporter",
        "--disable-client-side-phishing-detection",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-popup-blocking",
        "--disable-prompt-on-repost",
        "--disable-sync",
        "--disable-domain-reliability",
        "--disable-print-preview",
        "--no-default-browser-check",
        "--no-first-run",
        "--no-pings",
        "--metrics-recording-only",
        "--password-store=basic",
        "--use-mock-keychain",
        "--enable-automation",
        "--allow-pre-commit-input",
        "--allow-running-insecure-content",
        "--disable-web-security",
        "--disable-site-isolation-trials",
        // The pool's acceptInsecureCerts: every caller ran with it on.
        "--ignore-certificate-errors",
        // WHY (route66 owner 2026-11-15, capture size per test case): the OSR
        // view is the capture size while the CSS viewport stays emulated.
        // Without this the main frame clips its painting to the emulated
        // viewport and every row below it stays background. It is the clip
        // Page.captureScreenshot's captureBeyondViewport lifts per shot
        // (record_whole_document); layout, viewport units and
        // IntersectionObserver still see the emulated viewport.
        "--blink-settings=mainFrameClipsContent=false",
        // WHY (route66 GH #4471): no GPU and no emulated GPU. Lambda has no GPU,
        // and no route66 page uses WebGL. SwiftShader JIT-compiled every pipeline
        // on one thread on arm64 and froze a fresh sandbox's first page 4-6 s.
        // Every pixel is drawn by the software compositor.
        "--disable-gpu",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Extract and remove every `<flag>=a,b` feature list from `args`.
fn take_features(args: &mut Vec<String>, flag: &str) -> Vec<String> {
    let prefix = format!("{flag}=");
    let mut out = Vec::new();
    args.retain(|a| match a.strip_prefix(&prefix) {
        Some(rest) => {
            out.extend(rest.split(',').map(|f| f.trim().to_string()).filter(|f| !f.is_empty()));
            false
        }
        None => true,
    });
    out
}

/// The final switch list: base set, then the launch args (SNAPBOT_BROWSER_ARGS),
/// with the feature lists merged as puppeteer merged them.
fn switches(extra: &[String]) -> Vec<String> {
    let mut user = extra.to_vec();
    let user_disabled = take_features(&mut user, "--disable-features");
    let user_enabled = take_features(&mut user, "--enable-features");
    let mut enabled = vec!["SharedArrayBuffer".to_string()];
    enabled.extend(user_enabled);
    let mut disabled: Vec<String> = [
        "Translate",
        "AcceptCHFrame",
        "MediaRouter",
        "OptimizationHints",
        "ProcessPerSiteUpToMainFrameThreshold",
        "IsolateSandboxedIframes",
        "AudioServiceOutOfProcess",
        "IsolateOrigins",
        "site-per-process",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    disabled.extend(user_disabled);
    disabled.retain(|f| !enabled.contains(f));
    let mut out = base_switches();
    out.push(format!("--disable-features={}", disabled.join(",")));
    out.push(format!("--enable-features={}", enabled.join(",")));
    out.extend(user);
    out
}

/// The launch args this process's Chromium was initialized with (CEF reads
/// switches once, at CefInitialize, so they are fixed for the process).
static LAUNCH_ARGS: OnceLock<Vec<String>> = OnceLock::new();

pub fn launch_args() -> &'static [String] {
    LAUNCH_ARGS.get().map(Vec::as_slice).unwrap_or(&[])
}

#[derive(Clone)]
struct AppState {
    switches: Vec<String>,
}

wrap_app! {
    struct SnapbotApp {
        state: AppState,
    }

    impl App {
        fn on_before_command_line_processing(&self, process_type: Option<&CefString>, command_line: Option<&mut CommandLine>) {
            // Only the browser process takes our switches; CEF forwards the
            // renderer-relevant ones to its children itself.
            let is_browser = process_type.map(|p| cs(p).is_empty()).unwrap_or(true);
            let Some(cl) = command_line else { return };
            if !is_browser {
                return;
            }
            for s in &self.state.switches {
                let s = s.trim_start_matches('-');
                match s.split_once('=') {
                    Some((k, v)) => cl.append_switch_with_value(Some(&CefString::from(k)), Some(&CefString::from(v))),
                    None => cl.append_switch(Some(&CefString::from(s))),
                }
            }
        }
    }
}

fn app() -> App {
    let extra: Vec<String> = match std::env::var("SNAPBOT_BROWSER_ARGS") {
        Ok(v) if !v.is_empty() => serde_json::from_str(&v).unwrap_or_else(|e| {
            eprintln!("snapbot fatal: SNAPBOT_BROWSER_ARGS is not a JSON array of strings: {e}");
            std::process::exit(1)
        }),
        _ => Vec::new(),
    };
    let _ = LAUNCH_ARGS.set(extra.clone());
    SnapbotApp::new(AppState { switches: switches(&extra) })
}

/// When CEF re-executed this binary as one of its children (renderer, GPU,
/// utility), run that child and return its exit code. Must run first in main.
pub fn run_subprocess_if_child() -> Option<i32> {
    if !std::env::args().any(|a| a.starts_with("--type=")) {
        return None;
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = args::Args::new();
    let mut app = app();
    Some(execute_process(Some(args.as_main_args()), Some(&mut app), std::ptr::null_mut()))
}

/// CefInitialize on the main thread, windowless, no sandbox. Fail hard.
pub fn initialize_or_exit() {
    // Chromium writes under $HOME; the Lambda sets none. Set before any thread
    // exists, as the former launcher set it for the browser process.
    if std::env::var_os("HOME").is_none() {
        std::env::set_var("HOME", std::env::temp_dir());
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = args::Args::new();
    let mut app = app();
    let exe = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("snapbot fatal: current_exe: {e}");
        std::process::exit(1)
    });
    let dir = cef_dir();
    // Each process needs its own root cache dir (CEF's process singleton lock);
    // 40 pool lanes each run their own CEF.
    let root = std::env::temp_dir().join(format!("snapbot-cef-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&root) {
        eprintln!("snapbot fatal: create {}: {e}", root.display());
        std::process::exit(1);
    }
    let settings = Settings {
        no_sandbox: 1,
        windowless_rendering_enabled: 1,
        multi_threaded_message_loop: 0,
        external_message_pump: 0,
        browser_subprocess_path: CefString::from(exe.to_string_lossy().as_ref()),
        resources_dir_path: CefString::from(dir.as_str()),
        locales_dir_path: CefString::from(format!("{dir}/locales").as_str()),
        root_cache_path: CefString::from(root.to_string_lossy().as_ref()),
        log_file: CefString::from(root.join("cef.log").to_string_lossy().as_ref()),
        // Opaque white page background (see create_page).
        background_color: 0xFFFF_FFFF,
        // WHY: every page snapbot loads must be identifiable as test traffic in
        // server logs (the /agent/valuations request log showed a snapbot
        // capture as a visitor). Owner, verbatim: "User Agent must include the
        // words 'Testing' and 'Snapbot'". The product token keeps Chrome/<major>
        // so sites sniffing Chrome render the same page.
        user_agent_product: CefString::from(
            format!("Chrome/{}.0.0.0 Snapbot Testing", sys::CHROME_VERSION_MAJOR).as_str(),
        ),
        ..Default::default()
    };
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) != 1 {
        eprintln!("snapbot fatal: CefInitialize failed (CEF dir {dir})");
        std::process::exit(1);
    }
}

/// Block the main thread in CEF's UI loop until `quit` lands, then shut down.
pub fn run_until_quit() {
    run_message_loop();
    shutdown();
}

/// Close every page, then leave the UI loop. Callable from any thread.
pub fn quit() {
    hub().quitting.store(true, Ordering::SeqCst);
    on_ui(|| {
        let ids: Vec<i32> = UI.with(|u| u.borrow().browsers.keys().copied().collect());
        if ids.is_empty() {
            quit_message_loop();
            return;
        }
        // Collect first: close_browser runs OnBeforeClose re-entrantly, which
        // edits the same registry.
        let hosts: Vec<BrowserHost> = UI.with(|u| u.borrow().browsers.values().filter_map(|b| b.browser.host()).collect());
        for h in hosts {
            h.close_browser(1);
        }
    });
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

wrap_render_handler! {
    struct PageRender {
        slot: Arc<Slot>,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let (w, h) = *self.slot.view.lock().unwrap();
                rect.x = 0;
                rect.y = 0;
                rect.width = w;
                rect.height = h;
            }
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, screen_info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
            // Device scale 1: one CSS pixel is one captured pixel, as the
            // former deviceScaleFactor: 1 emulation and clip scale 1 gave.
            match screen_info {
                Some(si) => {
                    si.device_scale_factor = 1.0;
                    1
                }
                None => 0,
            }
        }

        // A VIEW paint is the orchestrator's paint signal and an armed
        // condition's check point. Its pixels are not kept: an image is the
        // compositor's surface at its ready point (shoot), never this
        // buffer's accumulated dirty rects (route66 GH #4486). Popups (select
        // dropdowns) paint separately and are not page paints.
        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if type_.get_raw() != PaintElementType::VIEW.get_raw() || buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let id = self.slot.id.load(Ordering::SeqCst);
            *self.slot.painted_size.lock().unwrap() = (width, height);
            // The orchestrator's step-painted signal.
            self.slot.painted.send_modify(|g| *g += 1);
            // A condition whose poll ended with its document restarts in the
            // new one (a no-op while a poll runs or once ready).
            start_poll(id);
        }
    }
}

wrap_life_span_handler! {
    struct PageLife {
        slot: Arc<Slot>,
    }

    impl LifeSpanHandler {
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let Some(b) = browser else { return };
            let id = b.identifier();
            if id != self.slot.id.load(Ordering::SeqCst) {
                return;
            }
            // Dropping the session sender ends the page's event loop.
            hub().sessions.lock().unwrap().remove(&id);
            UI.with(|u| u.borrow_mut().browsers.remove(&id));
            // Dropping an armed capture's sender fails its waiter ("dropped").
            ARMED.with(|m| m.borrow_mut().remove(&id));
            RENDERED.with(|r| r.borrow_mut().remove(&id));
            if let Some(tx) = hub().closed.lock().unwrap().remove(&id) {
                let _ = tx.send(());
            }
            if hub().quitting.load(Ordering::SeqCst) && UI.with(|u| u.borrow().browsers.is_empty()) {
                quit_message_loop();
            }
        }
    }
}

wrap_client! {
    struct PageClient {
        render: RenderHandler,
        life: LifeSpanHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life.clone())
        }
    }
}

wrap_dev_tools_message_observer! {
    struct CdpObserver {
        slot: Arc<Slot>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            // A surface-capture message's reply: its raw bytes (the PNG reply
            // runs to megabytes) go to on_shot_reply unparsed here.
            if let Some((page, token, stage)) = SHOTS.with(|s| s.borrow_mut().remove(&message_id)) {
                on_shot_reply(page, token, stage, success == 1, result.unwrap_or(&[]));
                return;
            }
            let v: Value = result.and_then(|r| serde_json::from_slice(r).ok()).unwrap_or(Value::Null);
            // A condition poll's reply: true makes the capture ready and sends
            // its surface capture. The poll held its true reply for two
            // animation frames (start_poll); the capture forces a fresh
            // compositor frame of the state it read (route66 local-verify
            // 2026-11-15 20:42 PT froze spinners on CTH search-result-page-2
            // with every card data-loaded). A failed poll (its document went
            // away) restarts on the next paint.
            if let Some((page, token)) = CHECKS.with(|c| c.borrow_mut().remove(&message_id)) {
                poll_ended(page, token);
                if success == 1 && truthy(v.pointer("/result/value")) {
                    ARMED.with(|m| {
                        if let Some(a) = m.borrow_mut().get_mut(&page).filter(|a| a.token == token && a.cond.is_some()) {
                            a.make_ready("poll");
                            shoot(page, a);
                        }
                    });
                }
                return;
            }
            // The step's completing message, on this (the paint) thread, before forwarding.
            if let Some((page, how, frame)) = COMPLETING.with(|c| c.borrow_mut().remove(&message_id)) {
                ARMED.with(|m| {
                    let mut m = m.borrow_mut();
                    let Some(a) = m.get_mut(&page) else { return };
                    // A matched reply makes the capture ready and sends its
                    // surface capture (shoot).
                    let by = match how {
                        Completes::Reply if success == 1 => "reply",
                        Completes::Truthy if success == 1 && truthy(v.pointer("/result/value")) => "truthy",
                        Completes::Lifecycle(name) if success == 1 => match v.get("loaderId").and_then(Value::as_str) {
                            Some(loader) => {
                                a.lifecycle = Some((name, loader.to_string(), frame));
                                return;
                            }
                            // A same-document navigation: no new document; the reply completes it.
                            None => "reply",
                        },
                        _ => return,
                    };
                    a.make_ready(by);
                    shoot(page, a);
                });
            }
            let Some(tx) = hub().pending.lock().unwrap().remove(&message_id) else { return };
            let _ = tx.send(if success == 1 {
                Ok(v)
            } else {
                Err(v.get("message").and_then(Value::as_str).unwrap_or("CDP error").to_string())
            });
        }

        fn on_dev_tools_event(&self, _browser: Option<&mut Browser>, method: Option<&CefString>, params: Option<&[u8]>) {
            let id = self.slot.id.load(Ordering::SeqCst);
            let method = method.map(cs).unwrap_or_default();
            let params: Value = params.and_then(|p| serde_json::from_slice(p).ok()).unwrap_or(Value::Null);
            // The page's render callback (called after its own double rAF):
            // send the surface capture from this callback. A main-frame commit
            // starts a new document's render calls.
            if method == "Page.frameNavigated" && params.pointer("/frame/parentId").is_none() {
                RENDERED.with(|r| r.borrow_mut().remove(&id));
            }
            if method == "Runtime.bindingCalled" && params.get("name").and_then(Value::as_str) == Some("__r66Rendered") {
                let payload = params.get("payload").and_then(Value::as_str).unwrap_or("").to_string();
                RENDERED.with(|r| r.borrow_mut().entry(id).or_default().push(payload.clone()));
                ARMED.with(|m| {
                    if let Some(a) = m.borrow_mut().get_mut(&id).filter(|a| a.rendered.as_deref() == Some(payload.as_str())) {
                        a.make_ready("rendered");
                        shoot(id, a);
                    }
                });
            }
            // A goto's completing lifecycle event, on this (the paint) thread, before forwarding.
            if method == "Page.lifecycleEvent" {
                ARMED.with(|m| {
                    let mut m = m.borrow_mut();
                    let Some(a) = m.get_mut(&id) else { return };
                    let Some((wait, loader, frame)) = a.lifecycle.clone() else { return };
                    let field = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    if field("loaderId") != loader || field("frameId") != frame {
                        return;
                    }
                    // A goto is painted at wait_until AND firstContentfulPaint.
                    if field("name") == wait {
                        a.seen_wait = true;
                    }
                    if field("name") == "firstContentfulPaint" {
                        a.seen_fcp = true;
                    }
                    // Both seen: the surface capture, whose forced redraw
                    // composites the document as it stands now.
                    if a.seen_wait && a.seen_fcp && a.make_ready("lifecycle") {
                        shoot(id, a);
                    }
                });
            }
            if let Some(tx) = hub().sessions.lock().unwrap().get(&id) {
                let _ = tx.send(Event { method, params });
            }
        }

        fn on_dev_tools_agent_detached(&self, _browser: Option<&mut Browser>) {
            // The page's DevTools agent went away under a live session: the
            // renderer died. Surface it as the target crash CDP clients expect.
            let id = self.slot.id.load(Ordering::SeqCst);
            if let Some(tx) = hub().sessions.lock().unwrap().get(&id) {
                let _ = tx.send(Event { method: "Inspector.targetCrashed".into(), params: json!({}) });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

/// A page as the tokio side holds it: its browser id and paint slot.
pub struct PageHandle {
    pub id: i32,
    pub slot: Arc<Slot>,
}

/// A new about:blank windowless page in `context` (a fresh in-memory cookie
/// jar and cache per context id), its event stream, and its paint slot.
pub async fn create_page(context: &str, width: i32, height: i32) -> Result<(PageHandle, mpsc::UnboundedReceiver<Event>)> {
    let slot = Arc::new(Slot { id: AtomicI32::new(-1), view: Mutex::new((width, height)), 
                               painted: tokio::sync::watch::channel(0).0, painted_size: Mutex::new((0, 0)) });
    let (tx, rx) = mpsc::unbounded_channel();
    let ctx = context.to_string();
    let s = slot.clone();
    let id = ui_call(move || -> std::result::Result<i32, String> {
        let mut rc = UI.with(|u| {
            let mut u = u.borrow_mut();
            if !u.contexts.contains_key(&ctx) {
                // An empty cache_path is an in-memory (incognito) context.
                if let Some(c) = request_context_create_context(Some(&RequestContextSettings::default()), None) {
                    u.contexts.insert(ctx.clone(), c);
                }
            }
            u.contexts.get(&ctx).cloned()
        });
        let rc = rc.as_mut().ok_or_else(|| format!("CEF refused to create request context {ctx}"))?;
        let window = WindowInfo { windowless_rendering_enabled: 1, ..Default::default() };
        // Opaque white behind the page, as headless Chromium painted it; CEF's
        // windowless default is transparent (black once alpha is dropped).
        let settings = BrowserSettings { windowless_frame_rate: 30, background_color: 0xFFFF_FFFF, ..Default::default() };
        let mut client = PageClient::new(PageRender::new(s.clone()), PageLife::new(s.clone()));
        let browser = browser_host_create_browser_sync(
            Some(&window),
            Some(&mut client),
            Some(&CefString::from("about:blank")),
            Some(&settings),
            None,
            Some(rc),
        )
        .ok_or_else(|| "CEF refused to create a windowless browser".to_string())?;
        let id = browser.identifier();
        s.id.store(id, Ordering::SeqCst);
        // Route events before the observer exists, so none is ever dropped.
        hub().sessions.lock().unwrap().insert(id, tx);
        let host = browser.host().ok_or_else(|| "new browser has no host".to_string())?;
        let mut obs = CdpObserver::new(s.clone());
        let registration = host.add_dev_tools_message_observer(Some(&mut obs)).ok_or_else(|| "CEF refused the DevTools observer".to_string())?;
        // A focused page: document.hasFocus(), :focus and focus events behave
        // as they did in the headless browser.
        host.set_focus(1);
        UI.with(|u| u.borrow_mut().browsers.insert(id, UiBrowser { browser, context: ctx.clone(), _registration: registration }));
        Ok(id)
    })
    .await?
    .map_err(|e| anyhow!(e))?;
    Ok((PageHandle { id, slot }, rx))
}

/// Close every page of `context` and forget the context; resolves once every
/// one of its browsers has closed.
pub async fn dispose_context(context: &str) -> Result<()> {
    let ctx = context.to_string();
    let waits = ui_call(move || {
        let mut waits = Vec::new();
        // Collect first: close_browser may run OnBeforeClose re-entrantly,
        // which edits the same registry.
        let hosts: Vec<BrowserHost> = UI.with(|u| {
            let mut u = u.borrow_mut();
            let mut hosts = Vec::new();
            for (id, b) in &u.browsers {
                if b.context == ctx {
                    let (tx, rx) = oneshot::channel();
                    hub().closed.lock().unwrap().insert(*id, tx);
                    hosts.extend(b.browser.host());
                    waits.push(rx);
                }
            }
            u.contexts.remove(&ctx);
            hosts
        });
        for h in hosts {
            h.close_browser(1);
        }
        waits
    })
    .await?;
    for w in waits {
        tokio::time::timeout(Duration::from_secs(10), w).await.map_err(|_| anyhow!("page of context {context} did not close within 10s"))?.ok();
    }
    Ok(())
}

wrap_completion_callback! {
    struct ConnectionsClosed {
        tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    }

    impl CompletionCallback {
        fn on_complete(&self) {
            if let Some(tx) = self.tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }
}

/// Close every idle and active connection (HTTP/1, HTTP/2, QUIC) of
/// `context`, keeping its HTTP cache, cookies and DNS; resolves when Chromium
/// reports them closed. A context not yet created has none.
pub async fn close_all_connections(context: &str) -> Result<()> {
    let ctx = context.to_string();
    let wait = ui_call(move || {
        UI.with(|u| u.borrow().contexts.get(&ctx).cloned()).map(|rc| {
            let (tx, rx) = oneshot::channel();
            let mut cb = ConnectionsClosed::new(Arc::new(Mutex::new(Some(tx))));
            rc.close_all_connections(Some(&mut cb));
            rx
        })
    })
    .await?;
    if let Some(w) = wait {
        tokio::time::timeout(Duration::from_secs(5), w).await.map_err(|_| anyhow!("connections of context {context} did not close within 5s"))?.ok();
    }
    Ok(())
}

/// Close one page's browser and wait for it; its request context stays open.
pub async fn close_page(id: i32) -> Result<()> {
    let wait = ui_call(move || {
        // Collect first: close_browser may run OnBeforeClose re-entrantly,
        // which edits the same registry.
        let host = UI.with(|u| u.borrow().browsers.get(&id).and_then(|b| b.browser.host()));
        host.map(|h| {
            let (tx, rx) = oneshot::channel();
            hub().closed.lock().unwrap().insert(id, tx);
            h.close_browser(1);
            rx
        })
    })
    .await?;
    if let Some(w) = wait {
        tokio::time::timeout(Duration::from_secs(10), w).await.map_err(|_| anyhow!("page {id} did not close within 10s"))?.ok();
    }
    Ok(())
}

/// Whether a page's browser is still open.
pub fn session_open(id: i32) -> bool {
    hub().sessions.lock().unwrap().contains_key(&id)
}

/// One CDP command on page `id`, in process.
pub async fn send(id: i32, method: &str, params: Value) -> std::result::Result<Value, String> {
    let msg_id = hub().next_msg.fetch_add(1, Ordering::SeqCst);
    let body = serde_json::to_vec(&json!({"id": msg_id, "method": method, "params": params})).map_err(|e| e.to_string())?;
    let (tx, rx) = oneshot::channel();
    hub().pending.lock().unwrap().insert(msg_id, tx);
    // A goto's navigate names its main frame; its lifecycle event must match it.
    let frame = params.get("frameId").and_then(Value::as_str).unwrap_or("").to_string();
    let completes = hub().completes.lock().unwrap().get(&id).cloned().filter(|c| match c {
        Completes::Lifecycle(_) => method == "Page.navigate",
        _ => method == "Runtime.evaluate",
    });
    on_ui(move || {
        if let Some(c) = completes {
            COMPLETING.with(|m| m.borrow_mut().insert(msg_id, (id, c, frame)));
        }
        let sent = host_of(id).map(|h| h.send_dev_tools_message(Some(&body[..])) == 1).unwrap_or(false);
        if !sent {
            if let Some(tx) = hub().pending.lock().unwrap().remove(&msg_id) {
                let _ = tx.send(Err("Target closed".to_string()));
            }
        }
    });
    rx.await.map_err(|_| "Target closed".to_string())?
}

/// Resize page `id`'s OSR view (its window) and let CEF re-query GetViewRect.
pub async fn resize(page: &PageHandle, width: i32, height: i32) -> Result<()> {
    *page.slot.view.lock().unwrap() = (width, height);
    let id = page.id;
    ui_call(move || {
        if let Some(h) = host_of(id) {
            h.was_resized();
        }
    })
    .await
}

/// Page `id`'s host, with the registry borrow released before the caller
/// touches CEF (its calls can re-enter our handlers synchronously).
fn host_of(id: i32) -> Option<BrowserHost> {
    UI.with(|u| u.borrow().browsers.get(&id).and_then(|b| b.browser.host()))
}

/// A slot's current paint generation (the event loop holds the slot, not the handle).
pub fn slot_generation(slot: &Slot) -> u64 {
    *slot.painted.borrow()
}

/// Page `id`'s current paint generation (how many VIEW paints have landed).
pub fn paint_generation(page: &PageHandle) -> u64 {
    *page.slot.painted.borrow()
}

/// Wait until page `id` has painted after generation `since`: a step's
/// result is painted (route66 GH #4486). No clock: the caller's step deadline
/// bounds it.
pub async fn painted_after(page: &PageHandle, since: u64) -> Result<u64> {
    let mut painted = page.slot.painted.subscribe();
    loop {
        let g = *painted.borrow_and_update();
        if g > since {
            return Ok(g);
        }
        painted.changed().await.map_err(|_| anyhow!("page {} closed before it painted", page.id))?;
    }
}

impl Armed {
    /// Mark the capture ready; true when this call made it so.
    fn make_ready(&mut self, by: &'static str) -> bool {
        if self.ready {
            return false;
        }
        (self.ready, self.by, self.ready_at) = (true, by, Some(Instant::now()));
        true
    }
}

/// A fresh armed capture of `size` (UI thread inserts it), not yet ready.
fn new_armed(size: ShotSize, tx: oneshot::Sender<Result<Frozen>>, cond: Option<String>, rendered: Option<String>) -> Armed {
    // One process-wide counter: tokens never repeat, so a stale reply never
    // completes a later capture on the same page.
    static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
    let token = NEXT_TOKEN.fetch_add(1, Ordering::SeqCst);
    Armed { token, size, tx, ready: false, by: "", ready_at: None, lifecycle: None, seen_wait: false, seen_fcp: false, shooting: false, polling: false, cond, rendered }
}

/// A ready capture sends its ONE surface capture (route66 GH #4486): CDP
/// Page.captureScreenshot fromSurface, which forces a compositor frame of the
/// page as it stands and copies that surface, so the image can neither
/// predate the ready point (the OSR view copy's stale frames) nor mix regions
/// of different moments (a1f0817's invalidate-swap composites). A shot taller
/// than the emulated viewport first reads the visual viewport's document
/// offset for its clip. Posted, so CEF never re-enters an observer under
/// ARMED's borrow. A second ready event sends nothing.
fn shoot(id: i32, a: &mut Armed) {
    if a.shooting {
        return;
    }
    a.shooting = true;
    let (token, size) = (a.token, a.size);
    on_ui(move || {
        if size.height > size.viewport_height {
            send_shot(id, token, ShotStage::Metrics, "Page.getLayoutMetrics", json!({}));
        } else {
            send_shot(id, token, ShotStage::Capture, "Page.captureScreenshot", capture_params(None));
        }
    });
}

/// Page.captureScreenshot's parameters: PNG from the compositor's surface,
/// never beyond the viewport (no layout change for the shot); no clip
/// returns the emulated viewport, a clip `width`x`height` rows at the visual
/// viewport's offset. optimizeForSpeed only lowers the zlib effort: PNG stays
/// lossless.
fn capture_params(clip: Option<Value>) -> Value {
    let mut p = json!({"format": "png", "fromSurface": true, "captureBeyondViewport": false, "optimizeForSpeed": true});
    if let Some(c) = clip {
        p["clip"] = c;
    }
    p
}

/// Send one surface-capture message for capture `token` on page `id` (UI
/// thread). A refused send fails the capture now.
fn send_shot(id: i32, token: u64, stage: ShotStage, method: &str, params: Value) {
    let msg_id = hub().next_msg.fetch_add(1, Ordering::SeqCst);
    let body = serde_json::to_vec(&json!({"id": msg_id, "method": method, "params": params})).unwrap();
    SHOTS.with(|s| s.borrow_mut().insert(msg_id, (id, token, stage)));
    if host_of(id).map(|h| h.send_dev_tools_message(Some(&body[..])) == 1) != Some(true) {
        SHOTS.with(|s| s.borrow_mut().remove(&msg_id));
        fail_shot(id, token, format!("page {id} refused its {method} surface capture"));
    }
}

/// Page `id`'s armed capture, removed, when it is still capture `token`.
fn take_armed(id: i32, token: u64) -> Option<Armed> {
    ARMED.with(|m| {
        let mut m = m.borrow_mut();
        if m.get(&id).is_some_and(|a| a.token == token) {
            m.remove(&id)
        } else {
            None
        }
    })
}

/// Fail capture `token` on page `id` with `e` (a no-op once it is gone).
fn fail_shot(id: i32, token: u64, e: String) {
    if let Some(a) = take_armed(id, token) {
        let _ = a.tx.send(Err(anyhow!(e)));
    }
}

/// A CDP error reply's message.
fn cdp_message(v: &Value) -> String {
    v.get("message").and_then(Value::as_str).unwrap_or("CDP error").to_string()
}

/// A surface-capture message's reply (UI thread, in the observer). Metrics:
/// send the clipped capture at the visual viewport's document offset.
/// Capture: hand the reply bytes to a decode thread, so the multi-megabyte
/// base64 and PNG decode never stalls the UI thread; it resolves the waiter.
fn on_shot_reply(id: i32, token: u64, stage: ShotStage, ok: bool, result: &[u8]) {
    match stage {
        ShotStage::Metrics => {
            let v: Value = serde_json::from_slice(result).unwrap_or(Value::Null);
            if !ok {
                return fail_shot(id, token, format!("Page.getLayoutMetrics failed: {}", cdp_message(&v)));
            }
            let Some(size) = ARMED.with(|m| m.borrow().get(&id).filter(|a| a.token == token).map(|a| a.size)) else { return };
            let at = |k: &str| v.pointer(&format!("/cssVisualViewport/{k}")).and_then(Value::as_f64);
            let (Some(x), Some(y)) = (at("pageX"), at("pageY")) else {
                return fail_shot(id, token, "Page.getLayoutMetrics returned no cssVisualViewport pageX/pageY".to_string());
            };
            let clip = json!({"x": x, "y": y, "width": size.width, "height": size.height, "scale": 1});
            // Posted: no CDP send from inside this observer callback.
            on_ui(move || send_shot(id, token, ShotStage::Capture, "Page.captureScreenshot", capture_params(Some(clip))));
        }
        ShotStage::Capture => {
            let Some(a) = take_armed(id, token) else { return };
            let ready_to_capture_ms = a.ready_at.map_or(0, |t| t.elapsed().as_millis() as u64);
            let (by, size, tx) = (a.by, a.size, a.tx);
            let bytes = result.to_vec();
            std::thread::spawn(move || {
                let r = decode_shot(ok, &bytes, size).map(|(frame, decode_ms, png_bytes)| Frozen { frame, ready_by: by, ready_to_capture_ms, decode_ms, png_bytes });
                let _ = tx.send(r);
            });
        }
    }
}

/// Page.captureScreenshot's reply as the BGRA frame of exactly `size`, its
/// decode ms and PNG bytes. A failed reply, missing data or another size fails.
fn decode_shot(ok: bool, bytes: &[u8], size: ShotSize) -> Result<(Frame, u64, u64)> {
    let t = Instant::now();
    let v: Value = serde_json::from_slice(bytes)?;
    if !ok {
        return Err(anyhow!("Page.captureScreenshot failed: {}", cdp_message(&v)));
    }
    let data = v.get("data").and_then(Value::as_str).ok_or_else(|| anyhow!("Page.captureScreenshot returned no PNG data"))?;
    let png = base64::engine::general_purpose::STANDARD.decode(data)?;
    let frame = crate::browser::decode_screenshot(&png, size.width, size.height)?;
    Ok((frame, t.elapsed().as_millis() as u64, png.len() as u64))
}

/// Wait until page `id` paints at `w`x`h` after generation `since`: a view
/// change completes at its new size, never on a paint at the old one.
pub async fn painted_at_size(page: &PageHandle, mut since: u64, w: i32, h: i32) -> Result<()> {
    loop {
        since = painted_after(page, since).await?;
        if *page.slot.painted_size.lock().unwrap() == (w, h) {
            return Ok(());
        }
    }
}

/// Arm a capture on page `id` before the step whose image follows: the step's
/// completing message (`completes`, matched on the UI thread) makes it ready
/// and sends its surface capture (shoot); the decoded frame resolves here.
pub async fn arm(page: &PageHandle, size: ShotSize, completes: Option<Completes>) -> oneshot::Receiver<Result<Frozen>> {
    let (tx, rx) = oneshot::channel();
    let id = page.id;
    let cond = match &completes {
        Some(Completes::Condition(expr)) => Some(expr.clone()),
        _ => None,
    };
    let rendered = match &completes {
        Some(Completes::Rendered(name)) => Some(name.clone()),
        _ => None,
    };
    match completes.filter(|c| !matches!(c, Completes::Condition(_) | Completes::Rendered(_))) {
        Some(c) => hub().completes.lock().unwrap().insert(id, c),
        None => hub().completes.lock().unwrap().remove(&id),
    };
    let (done_tx, done_rx) = oneshot::channel();
    on_ui(move || {
        ARMED.with(|m| {
            let mut a = new_armed(size, tx, cond.clone(), rendered.clone());
            // Already rendered in this document: its callback ran after its
            // double rAF; the surface capture goes now (shoot).
            if let Some(name) = &rendered {
                if RENDERED.with(|r| r.borrow().get(&id).is_some_and(|v| v.contains(name))) {
                    a.make_ready("rendered-earlier");
                    shoot(id, &mut a);
                }
            }
            m.borrow_mut().insert(id, a);
        });
        // The condition poll starts now: already true sends the surface capture.
        start_poll(id);
        let _ = done_tx.send(());
    });
    let _ = done_rx.await;
    rx
}

/// The step returned without its completing message being matched (a step
/// whose completion is not one CDP message): the armed capture is ready and
/// sends its surface capture now (shoot).
pub fn ready_posted(page: &PageHandle) {
    let id = page.id;
    hub().completes.lock().unwrap().remove(&id);
    on_ui(move || {
        ARMED.with(|m| {
            if let Some(a) = m.borrow_mut().get_mut(&id) {
                a.make_ready("posted");
                shoot(id, a);
            }
        });
    });
}

/// Drop page `id`'s armed capture (its step failed or its image was skipped).
/// A surface capture still in flight finds no capture of its token and is dropped.
pub fn disarm(page: &PageHandle) {
    let id = page.id;
    hub().completes.lock().unwrap().remove(&id);
    on_ui(move || ARMED.with(|m| {
        m.borrow_mut().remove(&id);
    }));
}

/// Capture page `id` with no step to wait on (an image right after another
/// image, first in a request, after a skipped step, the prime path, the
/// evidence viewport shot): a capture armed ready now, which sends its
/// surface capture at once (shoot). Copying the last delivered frame froze a
/// document's first paint (route66 local-verify 20261116T070021Z
/// california/CTH/workflow-saved-search-newsfeed, capture_ms=6). The receiver
/// resolves on the decoded reply; the caller's deadline bounds it.
pub async fn freeze(page: &PageHandle, size: ShotSize) -> oneshot::Receiver<Result<Frozen>> {
    let (tx, rx) = oneshot::channel();
    let id = page.id;
    hub().completes.lock().unwrap().remove(&id);
    on_ui(move || {
        ARMED.with(|m| {
            let mut a = new_armed(size, tx, None, None);
            a.make_ready("unarmed");
            shoot(id, &mut a);
            m.borrow_mut().insert(id, a);
        });
    });
    rx
}

/// Start page `id`'s armed condition poll unless one is in flight or the
/// capture is ready (UI thread, outside ARMED's borrow). The poll runs in the
/// page: the condition is evaluated now and then once per animation frame,
/// as Playwright's polling: 'raf' does, independent of OnPaint. WHY: a
/// per-paint check never re-ran a condition that turned true after the last
/// paint (contained harness on 9b47274: "wait_for_function result not
/// captured within its 8000ms deadline (armed surface capture never
/// returned)" for a flag set after a double rAF). A poll ended by its
/// document (navigation) restarts on the next paint, the new document's.
fn start_poll(id: i32) {
    let started = ARMED.with(|m| {
        let mut m = m.borrow_mut();
        let a = m.get_mut(&id)?;
        if a.ready || a.polling {
            return None;
        }
        let expr = a.cond.clone()?;
        a.polling = true;
        Some((a.token, expr))
    });
    let Some((token, expr)) = started else { return };
    let msg_id = hub().next_msg.fetch_add(1, Ordering::SeqCst);
    // A true result waits two animation frames before replying, so the state
    // it read is committed before the reply sends the surface capture. The
    // page-global token ends a superseded poll (a later capture's poll on the
    // same document) instead of leaving it looping every frame.
    let expression = format!("(async () => {{ globalThis.__r66Poll = {token}; while (globalThis.__r66Poll === {token}) {{ const v = await ({expr}); if (v) {{ await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))); return v; }} await new Promise(r => requestAnimationFrame(r)); }} return false; }})()");
    let body = serde_json::to_vec(&json!({"id": msg_id, "method": "Runtime.evaluate",
        "params": {"expression": expression, "returnByValue": true, "awaitPromise": true}})).unwrap();
    CHECKS.with(|c| c.borrow_mut().insert(msg_id, (id, token)));
    if host_of(id).map(|h| h.send_dev_tools_message(Some(&body[..])) == 1) != Some(true) {
        CHECKS.with(|c| c.borrow_mut().remove(&msg_id));
        poll_ended(id, token);
    }
}

/// Capture `token`'s poll on page `id` is no longer in flight.
fn poll_ended(id: i32, token: u64) {
    ARMED.with(|m| {
        if let Some(a) = m.borrow_mut().get_mut(&id).filter(|a| a.token == token) {
            a.polling = false;
        }
    });
}

/// JavaScript truthiness of a returned value (absent = undefined).
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}
