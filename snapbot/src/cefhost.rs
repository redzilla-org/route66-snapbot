//! Chromium embedded in this process: CEF windowless (off-screen) rendering.
//!
//! WHY (owner 2026-10-07, verbatim: "this sounds perfect: Embed Chromium in
//! snapbot ... hands over the raw pixel buffer in the same process"). The page
//! paints into CEF's OnPaint BGRA buffer inside this process, so a screenshot is
//! a memcpy into a memfd segment: no Chromium PNG encode, no base64 over a
//! websocket, no PNG decode before OCR. Every other browse op is the same CDP it
//! always was, delivered in process through CefBrowserHost::SendDevToolsMessage
//! and observed through a DevToolsMessageObserver.
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

use crate::shm::Segment;
use anyhow::{anyhow, bail, Result};
use cef::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// One CDP event from a page, routed to its session.
#[derive(Clone, Debug)]
pub struct Event {
    pub method: String,
    pub params: Value,
}

/// A pending full- or part-frame copy, armed by `capture` and filled by OnPaint.
struct Capture {
    want: (i32, i32),
    dest: Arc<Segment>,
    dest_stride: usize,
    dest_row: usize,
    done: oneshot::Sender<()>,
}

/// The per-page state OnPaint and GetViewRect read: the OSR view size (the
/// "window" the page lays out in) and an armed capture, if any.
pub struct Slot {
    id: AtomicI32,
    view: Mutex<(i32, i32)>,
    capture: Mutex<Option<Capture>>,
}

/// Cross-thread routing: CDP replies by message id, events by browser id.
#[derive(Default)]
struct Hub {
    pending: Mutex<HashMap<i32, oneshot::Sender<std::result::Result<Value, String>>>>,
    sessions: Mutex<HashMap<i32, mpsc::UnboundedSender<Event>>>,
    closed: Mutex<HashMap<i32, oneshot::Sender<()>>>,
    next_msg: AtomicI32,
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
        "--ignore-gpu-blocklist",
        "--in-process-gpu",
        "--use-gl=angle",
        "--use-angle=swiftshader",
        "--enable-unsafe-swiftshader",
        // WHY (route66 GH #4082): a CEF OSR frame is whatever the compositor
        // drew, and cc draws before raster finishes (unrastered tiles paint as
        // the white background; lv 20261008T005048Z full-page shots were white
        // below row ~900). Page.captureScreenshot waited for raster; OnPaint
        // does not. Headless deterministic mode's pair makes every draw wait
        // for all tiles and decode images synchronously, so the first frame
        // of the wanted size is a complete one.
        "--run-all-compositor-stages-before-draw",
        "--disable-checker-imaging",
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

        fn on_paint(
            &self,
            browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            // Popups (select dropdowns) paint separately and are not page pixels.
            if type_.get_raw() != PaintElementType::VIEW.get_raw() || buffer.is_null() {
                return;
            }
            if browser.map(|b| b.identifier()) != Some(self.slot.id.load(Ordering::SeqCst)) {
                return;
            }
            let mut armed = self.slot.capture.lock().unwrap();
            let matches = armed.as_ref().is_some_and(|c| c.want == (width, height));
            if !matches {
                return;
            }
            let c = armed.take().unwrap();
            // The buffer is valid only during this call: one copy, row by row,
            // into the frame's memfd segment.
            let row = width as usize * 4;
            // SAFETY: CEF hands a width*height*4 BGRA buffer for the duration of OnPaint.
            let src = unsafe { std::slice::from_raw_parts(buffer, row * height as usize) };
            let dst = c.dest.as_mut_slice();
            for y in 0..height as usize {
                let off = (c.dest_row + y) * c.dest_stride;
                dst[off..off + row].copy_from_slice(&src[y * row..(y + 1) * row]);
            }
            let _ = c.done.send(());
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
            let Some(tx) = hub().pending.lock().unwrap().remove(&message_id) else { return };
            let v: Value = result.and_then(|r| serde_json::from_slice(r).ok()).unwrap_or(Value::Null);
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
    let slot = Arc::new(Slot { id: AtomicI32::new(-1), view: Mutex::new((width, height)), capture: Mutex::new(None) });
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
    on_ui(move || {
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

/// Copy the next painted `want`-sized view frame of `page` into rows
/// `dest_row..` of `dest` (`dest_stride` bytes per row). The caller has already
/// made the renderer commit the state it wants painted; this forces a repaint
/// (Invalidate) and takes the first frame of the right size.
pub async fn capture(page: &PageHandle, want: (i32, i32), dest: Arc<Segment>, dest_stride: usize, dest_row: usize) -> Result<()> {
    if want.0 <= 0 || want.1 <= 0 || (dest_row + want.1 as usize) * dest_stride > dest.len() || (want.0 as usize) * 4 > dest_stride {
        bail!("capture of {}x{} does not fit the {}-byte frame at row {dest_row}", want.0, want.1, dest.len());
    }
    let (tx, rx) = oneshot::channel();
    *page.slot.capture.lock().unwrap() = Some(Capture { want, dest, dest_stride, dest_row, done: tx });
    let id = page.id;
    on_ui(move || {
        if let Some(h) = host_of(id) {
            h.invalidate(PaintElementType::VIEW);
        }
    });
    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(())) => Ok(()),
        _ => {
            page.slot.capture.lock().unwrap().take();
            bail!("no {}x{} paint arrived within 10s", want.0, want.1)
        }
    }
}
