//! Chromium embedded in this process: CEF windowless (off-screen) rendering.
//!
//! WHY (owner 2026-10-07, verbatim: "this sounds perfect: Embed Chromium in
//! snapbot ... hands over the raw pixel buffer in the same process"). The page
//! paints into CEF's off-screen view; OnPaint's BGRA buffer is kept per page and
//! a screenshot copies it once (route66 owner 2026-11-15: no Chromium PNG
//! round trip). CDP commands stay in process through
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

use crate::shm::{Frame, Segment};
use anyhow::{anyhow, bail, Result};
use cef::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
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
    /// The paint generation: bumped after every VIEW paint lands in the live view.
    painted: tokio::sync::watch::Sender<u64>,
    /// The size of the latest VIEW paint (a view change completes at its new size).
    painted_size: Mutex<(i32, i32)>,
}

/// A page's live VIEW memfd, owned by the CEF UI thread alone (route66 GH
/// #4486, owner 2026-11-16: "why do you need a lock? orchestrator"). OnPaint
/// copies exactly its dirty rects into it; a freeze, posted to the same
/// thread, hands it to an image's readers and installs a full copy as the
/// new live view, so a frozen frame is never written again.
struct Live {
    width: usize,
    height: usize,
    seg: Segment,
    /// OnPaint calls, and the dirty-rect bytes they copied, since the last freeze.
    paints: u64,
    paint_bytes: u64,
    /// The capture armed before the step whose image follows.
    armed: Option<Armed>,
}

/// A capture waiting for its step's completing event (route66 GH #4486, owner
/// 2026-11-16: "There should be no need to ever force a repaint, when you can
/// just grab the memfd from the last paint"). A matched reply or condition
/// freezes the live view in that same observer callback; a goto freezes on
/// the first paint after firstContentfulPaint once its wait_until fired too.
/// UI thread only: plain fields.
struct Armed {
    rows: usize,
    tx: oneshot::Sender<Result<Frozen>>,
    ready: bool,
    /// How `ready` was set: "reply", "truthy", "lifecycle" or "posted".
    by: &'static str,
    ready_at: Option<Instant>,
    /// A goto's completing lifecycle events: (wait_until name, the navigate's
    /// loaderId, its main frame id), and which of them fired.
    lifecycle: Option<(String, String, String)>,
    seen_wait: bool,
    seen_fcp: bool,
    /// A paint landed after firstContentfulPaint.
    painted_after_fcp: bool,
    /// A wait_for_* condition checked per paint (Completes::Condition).
    cond: Option<String>,
    /// The __r66Rendered payload that completes (Completes::Rendered).
    rendered: Option<String>,
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
    /// A wait_for_* condition, checked on the UI thread after every paint
    /// (and once at arm time); a true check with no newer paint completes.
    Condition(String),
    /// The page's __r66Rendered(name) call: the observer freezes the last
    /// paint in that callback.
    Rendered(String),
}

/// A frozen view handed to an image's readers, and what it cost.
pub struct Frozen {
    pub frame: Frame,
    /// What made the capture ready, and ready -> swapping paint, in ms.
    pub ready_by: &'static str,
    pub ready_to_paint_ms: u64,
    pub freeze_copy_ms: u64,
    pub freeze_copy_bytes: u64,
    pub paints: u64,
    pub paint_bytes: u64,
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
    /// Each page's live view, by browser id (UI thread only; see Live).
    static LIVE: RefCell<HashMap<i32, Live>> = RefCell::new(HashMap::new());
    /// In-flight CDP messages that complete an armed step: msg id -> (page, how).
    static COMPLETING: RefCell<HashMap<i32, (i32, Completes, String)>> = RefCell::new(HashMap::new());
    /// In-flight paint checks of an armed condition: msg id -> (page, the paint generation it checks).
    static CHECKS: RefCell<HashMap<i32, (i32, u64)>> = RefCell::new(HashMap::new());
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

        // Keep the view's pixels: a full copy when the size changed, else only
        // the dirty rects. Popups (select dropdowns) paint separately and are
        // not page pixels.
        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if type_.get_raw() != PaintElementType::VIEW.get_raw() || buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let (w, h) = (width as usize, height as usize);
            let row = w * 4;
            // SAFETY: CEF hands a width*height*4 BGRA buffer for the duration of OnPaint.
            let src = unsafe { std::slice::from_raw_parts(buffer, row * h) };
            let id = self.slot.id.load(Ordering::SeqCst);
            let cond = LIVE.with(|l| {
                let mut l = l.borrow_mut();
                let live = l.entry(id).or_insert_with(|| Live { width: 0, height: 0, seg: blank_view(1, 1), paints: 0, paint_bytes: 0, armed: None });
                if (live.width, live.height) != (w, h) {
                    // A new view size: a live view of that size (Chromium marks
                    // the whole view dirty, so the rects below fill it).
                    live.seg = blank_view(w, h);
                    (live.width, live.height) = (w, h);
                }
                let dst = live.seg.as_mut_slice();
                for r in dirty_rects.unwrap_or(&[]) {
                    let clamp = |v: i32, max: usize| (v.max(0) as usize).min(max);
                    let (x0, x1) = (clamp(r.x, w), clamp(r.x + r.width, w));
                    for y in clamp(r.y, h)..clamp(r.y + r.height, h) {
                        let (a, b) = (y * row + x0 * 4, y * row + x1 * 4);
                        dst[a..b].copy_from_slice(&src[a..b]);
                        live.paint_bytes += (b - a) as u64;
                    }
                }
                live.paints += 1;
                if let Some(a) = live.armed.as_mut() {
                    if a.seen_fcp {
                        a.painted_after_fcp = true;
                    }
                }
                // A ready capture freezes on this paint (a goto after its
                // firstContentfulPaint, or a view not yet painted at its size).
                let view = *self.slot.view.lock().unwrap();
                if live.armed.as_ref().is_some_and(|a| a.ready) {
                    complete(live, view);
                }
                live.armed.as_ref().and_then(|a| a.cond.clone())
            });
            *self.slot.painted_size.lock().unwrap() = (width, height);
            // The orchestrator's step-painted signal.
            self.slot.painted.send_modify(|g| *g += 1);
            if let Some(expr) = cond {
                send_check(id, &expr, *self.slot.painted.borrow());
            }
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
            LIVE.with(|l| l.borrow_mut().remove(&id));
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
            let v: Value = result.and_then(|r| serde_json::from_slice(r).ok()).unwrap_or(Value::Null);
            // A paint check: true with no newer paint freezes that paint; a
            // newer paint has sent its own check; false waits for the next paint.
            if let Some((page, p)) = CHECKS.with(|c| c.borrow_mut().remove(&message_id)) {
                if success == 1 && truthy(v.pointer("/result/value")) && *self.slot.painted.borrow() == p {
                    let view = *self.slot.view.lock().unwrap();
                    LIVE.with(|l| {
                        let mut l = l.borrow_mut();
                        let Some(live) = l.get_mut(&page) else { return };
                        if let Some(a) = live.armed.as_mut().filter(|a| a.cond.is_some()) {
                            a.make_ready("paint-check");
                            complete(live, view);
                        }
                    });
                }
                return;
            }
            // The step's completing message, on this (the paint) thread, before forwarding.
            if let Some((page, how, frame)) = COMPLETING.with(|c| c.borrow_mut().remove(&message_id)) {
                let view = *self.slot.view.lock().unwrap();
                LIVE.with(|l| {
                    let mut l = l.borrow_mut();
                    let Some(live) = l.get_mut(&page) else { return };
                    let Some(a) = live.armed.as_mut() else { return };
                    // A matched reply freezes the view now, in this callback.
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
                    complete(live, view);
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
            // The page's render callback: freeze the last paint in this callback.
            if method == "Runtime.bindingCalled" && params.get("name").and_then(Value::as_str) == Some("__r66Rendered") {
                let payload = params.get("payload").and_then(Value::as_str).unwrap_or("").to_string();
                let view = *self.slot.view.lock().unwrap();
                LIVE.with(|l| {
                    let mut l = l.borrow_mut();
                    let Some(live) = l.get_mut(&id) else { return };
                    if let Some(a) = live.armed.as_mut().filter(|a| a.rendered.as_deref() == Some(payload.as_str())) {
                        a.make_ready("rendered");
                        complete(live, view);
                    }
                });
            }
            // A goto's completing lifecycle event, on this (the paint) thread, before forwarding.
            if method == "Page.lifecycleEvent" {
                let view = *self.slot.view.lock().unwrap();
                LIVE.with(|l| {
                    let mut l = l.borrow_mut();
                    let Some(live) = l.get_mut(&id) else { return };
                    let Some(a) = live.armed.as_mut() else { return };
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
                    if a.seen_wait && a.seen_fcp {
                        a.make_ready("lifecycle");
                        // Already painted after FCP: that paint is in the live view.
                        if a.painted_after_fcp {
                            complete(live, view);
                        }
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

/// Freeze the armed capture from the live view as it was last painted: swap A
/// for B = copy of A and send A. A view not yet painted at its current size
/// stays armed (ready) and freezes on the first paint at that size.
fn complete(live: &mut Live, view: (i32, i32)) {
    if (live.width, live.height) != (view.0 as usize, view.1 as usize) {
        return;
    }
    let Some(a) = live.armed.take() else { return };
    let ready_to_paint_ms = a.ready_at.map_or(0, |t| t.elapsed().as_millis() as u64);
    let _ = a.tx.send(swap(live, a.rows).map(|f| Frozen { ready_by: a.by, ready_to_paint_ms, ..f }));
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

/// Swap the live view: allocate B, copy A into B, install B, return A with
/// its top `rows`. UI thread only, between paints, so A is never torn and
/// never written again.
fn swap(live: &mut Live, rows: usize) -> Result<Frozen> {
    let t = Instant::now();
    let next = Segment::create(live.seg.len())?;
    next.as_mut_slice().copy_from_slice(live.seg.as_slice());
    let a = std::mem::replace(&mut live.seg, next);
    Ok(Frozen {
        frame: Frame { seg: Arc::new(a), width: live.width, height: rows.min(live.height), stride: live.width * 4 },
        ready_by: "immediate",
        ready_to_paint_ms: 0,
        freeze_copy_ms: t.elapsed().as_millis() as u64,
        freeze_copy_bytes: live.seg.len() as u64,
        paints: std::mem::take(&mut live.paints),
        paint_bytes: std::mem::take(&mut live.paint_bytes),
    })
}

/// Arm a capture on page `id` before the step whose image follows: the step's
/// completing message (`completes`, matched on the UI thread) makes it ready,
/// and the first paint after that swaps the view and sends A here.
pub async fn arm(page: &PageHandle, rows: usize, completes: Option<Completes>) -> oneshot::Receiver<Result<Frozen>> {
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
    let gen_now = page.slot.clone();
    let (done_tx, done_rx) = oneshot::channel();
    on_ui(move || {
        LIVE.with(|l| {
            let mut l = l.borrow_mut();
            let live = l.entry(id).or_insert_with(|| Live { width: 0, height: 0, seg: blank_view(1, 1), paints: 0, paint_bytes: 0, armed: None });
            live.armed = Some(Armed { rows, tx, ready: false, by: "", ready_at: None, lifecycle: None, seen_wait: false, seen_fcp: false, painted_after_fcp: false, cond: cond.clone(), rendered: rendered.clone() });
        });
        // One check now: already true with no paint pending freezes the last paint.
        if let Some(expr) = cond {
            send_check(id, &expr, *gen_now.painted.borrow());
        }
        let _ = done_tx.send(());
    });
    let _ = done_rx.await;
    rx
}

/// The step returned without its completing message being matched (a step
/// whose completion is not one CDP message): freeze the armed capture now.
pub fn ready_posted(page: &PageHandle) {
    let id = page.id;
    let view = *page.slot.view.lock().unwrap();
    hub().completes.lock().unwrap().remove(&id);
    on_ui(move || {
        LIVE.with(|l| {
            let mut l = l.borrow_mut();
            let Some(live) = l.get_mut(&id) else { return };
            if let Some(a) = live.armed.as_mut() {
                a.make_ready("posted");
                complete(live, view);
            }
        });
    });
}

/// Drop page `id`'s armed capture (its step failed or its image was skipped).
pub fn disarm(page: &PageHandle) {
    let id = page.id;
    hub().completes.lock().unwrap().remove(&id);
    on_ui(move || LIVE.with(|l| {
        if let Some(live) = l.borrow_mut().get_mut(&id) {
            live.armed = None;
        }
    }));
}

/// Freeze page `id`'s live view now, with no step to wait on (an image right
/// after another image, or first in a request): the swap runs on the UI
/// thread, which owns the view, between paints.
pub async fn freeze(page: &PageHandle, rows: usize) -> Result<Frozen> {
    let (tx, rx) = oneshot::channel();
    let id = page.id;
    let (vw, vh) = *page.slot.view.lock().unwrap();
    on_ui(move || {
        let r = LIVE.with(|l| {
            let mut l = l.borrow_mut();
            let live = l.get_mut(&id).ok_or_else(|| anyhow!("page {id} has not painted"))?;
            if (live.width, live.height) != (vw as usize, vh as usize) {
                bail!("page {id} painted {}x{}, not its {vw}x{vh} view", live.width, live.height);
            }
            live.armed = None;
            swap(live, rows)
        });
        let _ = tx.send(r);
    });
    rx.await.map_err(|_| anyhow!("page {id}'s UI thread dropped the freeze"))?
}

/// Send an armed condition's check for paint generation `p` (UI thread).
fn send_check(id: i32, expr: &str, p: u64) {
    let msg_id = hub().next_msg.fetch_add(1, Ordering::SeqCst);
    let expression = format!("(async () => {{ return ({expr}); }})()");
    let body = serde_json::to_vec(&json!({"id": msg_id, "method": "Runtime.evaluate",
        "params": {"expression": expression, "returnByValue": true, "awaitPromise": true}})).unwrap();
    CHECKS.with(|c| c.borrow_mut().insert(msg_id, (id, p)));
    if host_of(id).map(|h| h.send_dev_tools_message(Some(&body[..])) == 1) != Some(true) {
        CHECKS.with(|c| c.borrow_mut().remove(&msg_id));
    }
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

/// A zeroed live view of `w`x`h` (OnPaint fills it from its dirty rects).
fn blank_view(w: usize, h: usize) -> Segment {
    match Segment::create(w * h * 4) {
        Ok(s) => s,
        Err(e) => panic!("live view segment of {w}x{h}: {e}"),
    }
}
