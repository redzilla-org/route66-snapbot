//! ocrd — a shared, box-wide OCR daemon with resident Tesseract engines.
//!
//! # Why a daemon
//!
//! Shelling out to the `tesseract` CLI pays a fixed cost per invocation —
//! process spawn plus loading the language model — comparable to the cost of
//! actually reading a typical page. This service loads the model once per
//! worker and keeps it resident, and as a DAEMON it keeps those engines warm
//! across client processes and across runs: every OCR consumer on the machine
//! shares ONE pool of loaded models instead of each client warming its own.
//! It also decodes and preprocesses in-process, which removes the temp-image
//! round trip a CLI caller needs in order to hand over a preprocessed page.
//!
//! # Generic on purpose
//!
//! Nothing here knows anything about the caller's domain. Language, page
//! segmentation mode, DPI, upscale factor and pixel budget are all REQUEST
//! PARAMETERS. The service's whole vocabulary is "here is an image and how to
//! read it"; what the image depicts and why is the caller's business.
//!
//! # Lifecycle: the port IS the singleton lock
//!
//! The daemon binds one loopback TCP address. A second instance fails to bind
//! and exits 0 — that is the entire mutual-exclusion protocol, and it makes
//! client-side "ensure running" trivially race-free: every client that finds
//! the port closed may spawn a daemon, exactly one wins the bind, the losers
//! exit quietly, and every client connects to the winner. There is no pid
//! file, no lock file, and no shutdown handshake: the daemon runs until the
//! machine or an operator stops it, which is the point — its warm engines are
//! the asset, and tearing them down with every client forfeits it.
//!
//! # A pool that grows with demand and never shrinks
//!
//! A standard thread pool: one shared job queue, workers receiving from it.
//! When a request arrives and every worker is busy, one MORE worker is
//! spawned — no limit, no configured size, no admission control. A caller
//! driving OCR at scale already owns a CPU budget for the box; a service with
//! its own cap would be a SECOND, uncoordinated scheduler on the same cores.
//! And both halves of a worker are expensive to create and free to keep — the
//! engine is a loaded model, the thread idle-blocks on a channel — so the pool
//! settles at the box's peak concurrency and stays there.
//!
//! # Protocol
//!
//! Newline-delimited JSON over the TCP connection, one request per line in,
//! one response per line out. Replies come back OUT OF ORDER, necessarily:
//! workers finish at different speeds. Every reply carries the `id` of the
//! request it answers, and replies are routed to the CONNECTION that sent the
//! request — concurrent clients never see each other's traffic.
//!
//! ```text
//! -> {"id":"a1","path":"/tmp/page.png","psm":3,"lang":"eng","dpi":300,"upscale":3}
//! <- {"id":"a1","text":"..."}
//! ```
//!
//! A malformed line is logged and skipped rather than fatal: a caller that can
//! still make progress on its other pages should not lose them to one bad
//! line. A disconnected client's in-flight reads complete and their replies
//! are dropped on the closed socket — harmless, and simpler than cancellation.

mod kstream;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

// COMPILE FIX (build host, 2026-08-23): leptess::LepTess exposes no raw-frame
// setter (its tess_api field is private; only file/encoded-image inputs exist).
// TessApi's public .raw (tesseract-plumbing TessBaseApi) carries set_image for
// a raw 8-bit plane, which is what read_page needs — so the engine type is
// TessApi, not LepTess.
use leptess::tesseract::TessApi;
use serde::{Deserialize, Serialize};

/// Default listen address. Loopback only — this daemon serves the machine it
/// runs on and must never be reachable from off-box; the port is arbitrary
/// but FIXED, because it is the rendezvous every client compiles in.
// 40066, not the 466xx block originally chosen: 46620-46622 proved un-bindable
// on the reference Windows box (reserved with no visible owner -- WSL mirrored
// networking), and the bind-failure path used to mask that as a healthy
// singleton. See the AddrInUse probe in main().
const DEFAULT_ADDR: &str = "127.0.0.1:40066";

/// One page to read.
///
/// Every knob has a default, so the minimal request is just an id and a path —
/// but nothing is hardcoded, which is what keeps the service reusable.
#[derive(Debug, Deserialize)]
struct Request {
    id: String,
    path: String,

    /// Tesseract page segmentation mode. The caller owns this choice; different
    /// page shapes segment best under different modes.
    #[serde(default = "default_psm")]
    psm: u32,

    #[serde(default = "default_lang")]
    lang: String,

    /// Resolution hint. Tesseract's heuristics behave badly on screenshots
    /// without one, since a screen capture carries no meaningful DPI metadata.
    #[serde(default = "default_dpi")]
    dpi: u32,

    /// Maximum integer upscale. Small anti-aliased text needs more pixels per
    /// stroke than a screen capture provides; 1 disables it.
    #[serde(default = "default_upscale")]
    upscale: u32,

    /// Ceiling on upscaled size, in pixels. Guards against a tall page turning a
    /// 3x upscale into an image that costs more than everything else combined.
    #[serde(default = "default_pixel_budget")]
    pixel_budget: u64,
}

fn default_psm() -> u32 {
    3
}
fn default_lang() -> String {
    "eng".to_string()
}
fn default_dpi() -> u32 {
    300
}
fn default_upscale() -> u32 {
    1
}
fn default_pixel_budget() -> u64 {
    20_000_000
}

/// The answer to exactly one request.
///
/// `text` and `error` are mutually exclusive, and a failure is reported as a
/// response rather than raised as a service-level fault — one unreadable image
/// must not take down a service that other pages are still queued behind.
#[derive(Debug, Serialize)]
struct Response {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn ok(id: String, text: String) -> Self {
        Response { id, text: Some(text), error: None }
    }

    fn err(id: String, error: String) -> Self {
        Response { id, text: None, error: Some(error) }
    }
}

/// One queued job: the request plus the reply lane of the connection that sent
/// it. Carrying the lane WITH the job is what routes each reply back to its
/// own client with no routing table anywhere.
struct Job {
    req: Request,
    replies: mpsc::Sender<Response>,
}

/// The shared job queue.
///
/// ONE queue, every worker receiving from it — the standard thread-pool shape.
/// Workers self-select the next job, so there is no dispatcher-side bookkeeping
/// at all: no per-worker channel, no idle list, no index, and no ordering
/// invariant to get wrong.
type Jobs = Arc<Mutex<mpsc::Receiver<Job>>>;

/// How many workers are mid-read right now.
///
/// One of the inputs to the growth decision (with the queued and spawned
/// counts). A stale read can only ever cost one surplus worker, which the
/// never-shrink policy simply keeps.
type Busy = Arc<AtomicUsize>;

/// How many accepted jobs are waiting in the queue, not yet picked up.
///
/// WHY this exists (measured 2026-08-23): with growth keyed on `busy` alone, a
/// single client bursting 59 images down one connection outran the busy
/// counter — requests were enqueued faster than workers moved jobs into the
/// "busy" state, so the `busy >= workers` check almost never fired and the
/// pool stayed at 1-2 workers. The burst drained near-serially at ~3 s/image:
/// 229 s for 59 images against 44 s for a plain CLI loop. Counting the queue
/// depth into the growth condition lets the pool reach burst size immediately.
type Queued = Arc<AtomicUsize>;

/// Reads one page with an already-resident engine.
///
/// The engine is reused across calls, which is the point — but that means its
/// per-request state must be set every time, never assumed to carry over.
fn read_page(engine: &mut TessApi, req: &Request) -> Result<String, String> {
    let raw = std::fs::read(&req.path).map_err(|e| format!("read {}: {e}", req.path))?;

    // SCALE-1 PASSTHROUGH (measured 2026-08-23): when the budget clamp (or a
    // requested upscale of 1) leaves the image at native size, the preprocess
    // buys nothing — no upscaling happened — and its 8-bit gray plane triggers
    // tesseract's pathologically slow binarization path on ultra-wide pages.
    // Evidence on a 19599x1002 screenshot (scale 1 under the 20 MPix budget):
    // 177 s through the gray plane vs 3.2 s for the tesseract CLI on the raw
    // RGB PNG; feeding that CLI a PLAIN grayscale PNG of the same page took
    // 39 s and a contrast-stretched one 40 s — so the cost is gray-plane input
    // itself, not the stretch and not these kernels. Hand tesseract the
    // ORIGINAL file (leptonica Pix, RGB path) whenever scale is 1; the
    // preprocessed-gray path is unchanged for scale >= 2, where the upscale is
    // the whole point.
    let scale = kstream::planned_scale(&raw, req.upscale as usize, req.pixel_budget)?;
    if scale == 1 {
        let pix = leptess::leptonica::pix_read(std::path::Path::new(&req.path))
            .map_err(|e| format!("pix_read {}: {e:?}", req.path))?;
        engine.set_image(&pix);
        engine.set_source_resolution(req.dpi as i32);
        return engine
            .get_utf8_text()
            .map_err(|e| format!("ocr {}: {e}", req.path));
    }

    // Decode, grayscale, contrast-stretch and upscale in ONE copied pipeline
    // (src/kstream.rs). The plane goes straight into the engine: no PGM temp file,
    // which at a 3x upscale was ~16 MB written and read back per image.
    let (pixels, w, h) = kstream::preprocess(&raw, req.upscale as usize, req.pixel_budget)?;

    // 8-bit grayscale: one byte per pixel, rows tightly packed — the pipeline
    // above allocates exact geometry and never introduces stride padding.
    engine
        .raw
        .set_image(&pixels, w as i32, h as i32, 1, w as i32)
        .map_err(|e| format!("set_image {}: {e}", req.path))?;

    engine.set_source_resolution(req.dpi as i32);

    engine
        .get_utf8_text()
        .map_err(|e| format!("ocr {}: {e}", req.path))
}

/// Creates or reuses the worker's engine, rebuilding only on a language change.
fn ensure_engine<'a>(
    slot: &'a mut Option<(String, TessApi)>,
    lang: &str,
) -> Result<&'a mut TessApi, String> {
    let rebuild = match slot {
        Some((have, _)) => have != lang,
        None => true,
    };
    if rebuild {
        let tess = TessApi::new(None, lang).map_err(|e| format!("init tesseract ({lang}): {e}"))?;
        *slot = Some((lang.to_string(), tess));
    }
    Ok(&mut slot.as_mut().unwrap().1)
}

/// Spawns one worker: a thread that owns an engine and serves jobs forever.
///
/// The engine is created LAZILY, on the first job, so growing the pool costs
/// nothing until there is actually work for the new worker. Workers never
/// exit — the daemon's whole purpose is keeping them, engines loaded, for the
/// next client.
fn spawn_worker(jobs: Jobs, busy: Busy, queued: Queued) {
    thread::spawn(move || {
        let mut engine: Option<(String, TessApi)> = None;

        loop {
            // The lock is held only across recv, then released before the read
            // starts — so one worker waits on the queue while the rest wait on
            // the lock, and a job never blocks behind another worker's OCR.
            let job = match jobs.lock() {
                Ok(rx) => rx.recv(),
                Err(_) => return,
            };

            let Ok(Job { req, replies }) = job else { return };

            // The job left the queue the moment recv returned; hand its count
            // from `queued` to `busy` so the growth condition sees every
            // accepted-but-unfinished job exactly once.
            queued.fetch_sub(1, Ordering::SeqCst);
            busy.fetch_add(1, Ordering::SeqCst);

            let resp = match ensure_engine(&mut engine, &req.lang) {
                Ok(tess) => {
                    // COMPILE FIX: TessApi exposes no set_variable wrapper;
                    // reach through .raw with CStrings.
                    if let (Ok(name), Ok(value)) = (
                        std::ffi::CString::new("tessedit_pageseg_mode"),
                        std::ffi::CString::new(req.psm.to_string()),
                    ) {
                        tess.raw.set_variable(&name, &value).ok();
                    }

                    match read_page(tess, &req) {
                        Ok(text) => Response::ok(req.id, text),
                        Err(msg) => Response::err(req.id, msg),
                    }
                }
                Err(msg) => Response::err(req.id, msg),
            };

            busy.fetch_sub(1, Ordering::SeqCst);
            // A send failure means the client disconnected mid-read; its answer
            // has nowhere to go, and that is fine.
            let _ = replies.send(resp);
        }
    });
}

/// Serves one client connection: reads request lines, queues jobs, and runs
/// this connection's single reply writer.
///
/// ONE writer per connection, because workers finish concurrently and
/// concurrent writers would interleave partial lines and destroy the framing.
/// The reply channel is this connection's private lane — dropping the last
/// sender (reader returns, in-flight workers reply) ends the writer, which is
/// the entire per-connection cleanup.
fn serve_client(stream: TcpStream, jobs_tx: mpsc::Sender<Job>, jobs: Jobs, busy: Busy, queued: Queued, workers: Arc<AtomicUsize>) {
    let (replies_tx, replies_rx) = mpsc::channel::<Response>();

    let write_half = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ocrd: clone stream: {e}");
            return;
        }
    };
    thread::spawn(move || {
        let mut out = write_half;
        for resp in replies_rx {
            if let Ok(line) = serde_json::to_string(&resp) {
                if writeln!(out, "{line}").is_err() {
                    return; // Client hung up.
                }
                // Flushed per reply: a caller blocked on it must not be stalled
                // by a buffer it cannot see. (TcpStream is unbuffered, so this
                // is documentation more than mechanism.)
                let _ = out.flush();
            }
        }
    });

    thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { return };
            if line.trim().is_empty() {
                continue;
            }

            let req: Request = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    // No id to answer with, so this goes to diagnostics and the
                    // service keeps serving: one malformed line must not cost
                    // the caller every read still in flight.
                    eprintln!("ocrd: malformed request: {e}");
                    continue;
                }
            };

            // Count this job as queued BEFORE it is enqueued, then GROW while
            // demand (mid-read + waiting) covers the whole pool. Keying growth
            // on `busy` alone lost a measured 5x on a 59-image single-client
            // burst (229 s vs 44 s for a CLI loop): the busy counter lags
            // request arrival, so the pool stayed at 1-2 workers and the burst
            // drained near-serially at ~3 s/image. Checked from concurrent
            // reader threads, so check-and-spawn is racy by design: the worst
            // case is a few surplus workers, which the never-shrink policy
            // keeps anyway.
            queued.fetch_add(1, Ordering::SeqCst);
            while busy.load(Ordering::SeqCst) + queued.load(Ordering::SeqCst)
                >= workers.load(Ordering::SeqCst)
            {
                spawn_worker(Arc::clone(&jobs), Arc::clone(&busy), Arc::clone(&queued));
                workers.fetch_add(1, Ordering::SeqCst);
            }

            if jobs_tx.send(Job { req, replies: replies_tx.clone() }).is_err() {
                queued.fetch_sub(1, Ordering::SeqCst);
                return;
            }
        }
        // Reader done: this connection's replies_tx drops here, and once the
        // last in-flight worker clone drops too, the writer thread ends.
    });
}

fn main() {
    // Sole flag: --listen <addr>. Anything else is a usage error; the daemon
    // deliberately has no other configuration surface.
    let mut addr = DEFAULT_ADDR.to_string();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag, value] if flag == "--listen" => addr = value.clone(),
        _ => {
            eprintln!("usage: ocrd [--listen host:port]   (default {DEFAULT_ADDR})");
            std::process::exit(2);
        }
    }

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // AddrInUse alone does NOT prove another daemon is serving. On this
            // Windows box, ports can be reserved with no visible owner (WSL
            // mirrored networking held 46620-46622: bind fails WSAEADDRINUSE,
            // yet netstat/Get-NetTCPConnection show nothing and nothing
            // answers). Exiting 0 on that masked the outage completely — the
            // speculative spawner saw a healthy singleton and then hung
            // connecting. So PROBE: only a port that actually answers is a
            // real singleton.
            let probe = addr
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
                .and_then(|sa| TcpStream::connect_timeout(&sa, Duration::from_secs(2)).ok());
            if probe.is_some() {
                // Another daemon answered: the spawn race's intended outcome.
                // Exit 0 so a client that speculatively spawned us sees
                // nothing wrong — the winner is serving.
                eprintln!("ocrd: {addr} already in use — another instance is serving; exiting");
                return;
            }
            eprintln!(
                "ocrd: {addr} is reserved but nothing answers — poisoned port \
                 (e.g. WSL mirrored-networking reservation); pick another with \
                 --listen"
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("ocrd: bind {addr}: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("ocrd: listening on {addr}");

    let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
    let jobs: Jobs = Arc::new(Mutex::new(jobs_rx));
    let busy: Busy = Arc::new(AtomicUsize::new(0));
    let queued: Queued = Arc::new(AtomicUsize::new(0));
    let workers = Arc::new(AtomicUsize::new(0));

    for stream in listener.incoming() {
        match stream {
            Ok(s) => serve_client(
                s,
                jobs_tx.clone(),
                Arc::clone(&jobs),
                Arc::clone(&busy),
                Arc::clone(&queued),
                Arc::clone(&workers),
            ),
            Err(e) => eprintln!("ocrd: accept: {e}"),
        }
    }
}
