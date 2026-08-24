//! ocrd — a shared OCR service with resident Tesseract engines, behind NATS.
//!
//! # Why a daemon
//!
//! Shelling out to the `tesseract` CLI pays a fixed cost per invocation —
//! process spawn plus loading the language model — comparable to the cost of
//! actually reading a typical page. This service loads the model once per
//! worker and keeps it resident, so every OCR consumer shares ONE pool of
//! loaded models instead of each client warming its own. It also decodes and
//! preprocesses in-process, which removes the temp-image round trip a CLI
//! caller needs in order to hand over a preprocessed page.
//!
//! # Generic on purpose
//!
//! Nothing here knows anything about the caller's domain. Language, page
//! segmentation mode, DPI, upscale factor and pixel budget are all REQUEST
//! PARAMETERS. The service's whole vocabulary is "here is an image and how to
//! read it"; what the image depicts and why is the caller's business.
//!
//! # Why NATS replaced the loopback TCP listener
//!
//! The old protocol bound `127.0.0.1:40066` and took a FILESYSTEM PATH per
//! request. Both assumptions broke on the reference workstation: a Linux ocrd
//! running inside a host-networked WSL2 container answered the Windows harness
//! on loopback perfectly well, and then could not open a single one of the
//! Windows paths it was handed. Worse, the port could not identify itself —
//! anything listening on 40066 looked like a healthy daemon, including a stale
//! build from a previous run and a WSL networking reservation with no owner at
//! all.
//!
//! Both problems are transport problems, and both disappear here:
//!
//! * The image travels IN the message as base64, so the daemon never needs to
//!   share a filesystem with its caller. There is no `path` field and no path
//!   fallback — one way in.
//! * The caller invents a per-run-unique subject prefix. Getting an answer on
//!   `<prefix>.ocr.version` proves the daemon that answered is the one this run
//!   started, which a port number can never prove.
//!
//! # Subjects
//!
//! ```text
//! <prefix>.ocr.read      request/reply, queue group "ocrd"
//!   ->  {"image":"<base64 png/jpeg>","psm":6,"lang":"eng","dpi":300,
//!        "upscale":2,"pixel_budget":1230000}
//!   <-  {"text":"...","error":""}
//!
//! <prefix>.ocr.version   request/reply, empty request
//!   <-  {"version":"ocrd-rust 0.1.0","impl":"rust"}
//! ```
//!
//! A non-empty `error` means that read failed. THE REPLY IS STILL SENT, always,
//! including for a request that will not even parse: a silently absent OCR
//! result is indistinguishable from a passing check at the caller, which is the
//! exact failure mode this daemon exists to make impossible.
//!
//! # Concurrency is the subscriber count, and nothing else
//!
//! `--workers N` spawns N tasks. Each one INDEPENDENTLY joins the queue group
//! on `<prefix>.ocr.read` and then handles one message at a time to completion.
//! NATS delivers each request to exactly one member of the group, so at most N
//! reads are ever in flight — the bound is the number of subscribers, full
//! stop. There is deliberately no semaphore, no channel gate and no bounded
//! task pool anywhere in this file: those are a SECOND scheduler layered on top
//! of the queue group's own, with two places to get the bound wrong instead of
//! one, and they leave requests parked inside this process where the server
//! cannot redeliver them to another daemon.
//!
//! # Server max_payload
//!
//! NATS defaults to a 1 MB max_payload. A page screenshot exceeds that on its
//! own, and base64 inflates it another ~33%, so THE SERVER MUST BE CONFIGURED
//! WITH A RAISED max_payload — the caller side runs it at 32 MB. Nothing in
//! this client imposes a smaller cap of its own; async-nats accepts whatever
//! the server advertises in its INFO. A request that exceeds the server's limit
//! is rejected by the SERVER, at the publisher, before it ever reaches here.

mod kstream;

use base64::Engine as _;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

// COMPILE FIX (build host, 2026-08-23): leptess::LepTess exposes no raw-frame
// setter (its tess_api field is private; only file/encoded-image inputs exist).
// TessApi's public .raw (tesseract-plumbing TessBaseApi) carries set_image for
// a raw 8-bit plane, which is what read_page needs — so the engine type is
// TessApi, not LepTess.
use leptess::tesseract::TessApi;

/// Build identity, reported by `--version` and by the `.ocr.version` subject.
///
/// DERIVED FROM Cargo.toml, never written out here: `.ocr.version` is the
/// identity proof this whole transport is built on, and an identity reply that
/// is wrong by a version defeats the reason the subject exists. Bumping the
/// crate version is the only edit needed; there is no second place to forget.
const VERSION: &str = concat!("ocrd-rust ", env!("CARGO_PKG_VERSION"));

/// Which implementation is answering, reported on `.ocr.version`.
///
/// The two implementations of this protocol are meant to be interchangeable, so
/// nothing else in the wire format reveals which one is running. This one field
/// is the deliberate exception, because an operator debugging a bad read needs
/// to know what is actually installed.
const IMPL: &str = "rust";

/// The queue group every read subscriber joins.
///
/// Fixed, not configurable: the group name is the mechanism that makes N
/// subscribers share one request stream instead of each receiving a copy, and a
/// caller that could set it could only ever set it wrong.
const READ_QUEUE_GROUP: &str = "ocrd";

/// One page to read.
///
/// Every knob has a default, so the minimal request is just an image — but
/// nothing is hardcoded, which is what keeps the service reusable. The defaults
/// are the ones the previous TCP protocol used, unchanged: a caller that sends
/// nothing but an image gets exactly what it used to get.
#[derive(Debug, Deserialize)]
struct Request {
    /// The image itself, base64, PNG or JPEG. NOT a path — see the module
    /// preamble. This is what lets the daemon run anywhere the caller can
    /// reach over NATS rather than only where it shares a disk.
    image: String,

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

impl Request {
    /// Applies the "absent OR zero means default" rule.
    ///
    /// Serde's `#[serde(default)]` covers an ABSENT field, but callers
    /// serialising from a struct send the zero value instead of omitting it,
    /// and a `psm` of 0 or a `pixel_budget` of 0 is not a request to segment
    /// with mode 0 and allow no pixels — it is an unset field. Normalising here
    /// rather than at each use site keeps the fallbacks in exactly one place.
    fn normalize(&mut self) {
        if self.psm == 0 {
            self.psm = default_psm();
        }
        if self.lang.is_empty() {
            self.lang = default_lang();
        }
        if self.dpi == 0 {
            self.dpi = default_dpi();
        }
        if self.upscale == 0 {
            self.upscale = default_upscale();
        }
        if self.pixel_budget == 0 {
            self.pixel_budget = default_pixel_budget();
        }
    }
}

/// The answer to exactly one read.
///
/// BOTH fields are always serialised, even when empty. The caller's contract is
/// `{"text":"...","error":""}`, and a decoder that must distinguish "field
/// absent" from "field empty" has one more state to get wrong for no gain.
/// `text` and `error` are mutually exclusive in practice: a failure is reported
/// as a reply rather than raised as a service-level fault, because one
/// unreadable image must not take down a service other pages are queued behind.
#[derive(Debug, Serialize)]
struct Response {
    text: String,
    error: String,
}

impl Response {
    fn ok(text: String) -> Self {
        Response { text, error: String::new() }
    }

    fn err(error: String) -> Self {
        Response { text: String::new(), error }
    }

    /// Serialises to the reply payload.
    ///
    /// Infallible by construction — two owned Strings always serialise — but
    /// the fallback is spelled out rather than unwrapped so that a serialiser
    /// surprise degrades to an error REPLY instead of killing the worker and
    /// leaving the caller waiting on a request that will never be answered.
    fn payload(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|e| {
            format!("{{\"text\":\"\",\"error\":\"encode reply: {e}\"}}").into_bytes()
        })
    }
}

/// The `.ocr.version` reply: build identity plus which implementation.
#[derive(Debug, Serialize)]
struct VersionResponse {
    version: &'static str,
    #[serde(rename = "impl")]
    implementation: &'static str,
}

/// Reads one page with an already-resident engine.
///
/// The engine is reused across calls, which is the point — but that means its
/// per-request state must be set every time, never assumed to carry over.
///
/// UNCHANGED FROM THE TCP DAEMON except for its input: it takes the decoded
/// image BYTES rather than a path. This is a transport change, not a pipeline
/// change, so `kstream` and both engine paths below are byte-for-byte the
/// decisions that were measured.
fn read_page(engine: &mut TessApi, req: &Request, raw: &[u8]) -> Result<String, String> {
    // SCALE-1 PASSTHROUGH (measured 2026-08-23): when the budget clamp (or a
    // requested upscale of 1) leaves the image at native size, the preprocess
    // buys nothing — no upscaling happened — and its 8-bit gray plane triggers
    // tesseract's pathologically slow binarization path on ultra-wide pages.
    // Evidence on a 19599x1002 screenshot (scale 1 under the 20 MPix budget):
    // 177 s through the gray plane vs 3.2 s for the tesseract CLI on the raw
    // RGB PNG; feeding that CLI a PLAIN grayscale PNG of the same page took
    // 39 s and a contrast-stretched one 40 s — so the cost is gray-plane input
    // itself, not the stretch and not these kernels. Hand tesseract the
    // ORIGINAL encoded image (leptonica Pix, RGB path) whenever scale is 1; the
    // preprocessed-gray path is unchanged for scale >= 2, where the upscale is
    // the whole point.
    //
    // pix_read_mem, not pix_read: there is no file to point leptonica at any
    // more. Same leptonica decode, same RGB Pix, no temp file round trip.
    //
    // A HEADER THIS PLANNER CANNOT READ IS NOT AN ERROR, IT IS SCALE 1. The
    // planner reads a PNG IHDR, and the wire contract admits JPEG as well; a
    // JPEG has no IHDR, so planning fails and the honest answer is "do not
    // upscale, hand leptonica the original", which decodes both formats. The
    // alternative — refusing the request — would turn a supported format into a
    // read failure. A genuinely corrupt payload still fails, one step later and
    // just as loudly, in pix_read_mem.
    let scale = kstream::planned_scale(raw, req.upscale as usize, req.pixel_budget).unwrap_or(1);
    if scale == 1 {
        let pix = leptess::leptonica::pix_read_mem(raw)
            .map_err(|e| format!("pix_read_mem: {e:?}"))?;
        engine.set_image(&pix);
        engine.set_source_resolution(req.dpi as i32);
        return engine.get_utf8_text().map_err(|e| format!("ocr: {e}"));
    }

    // Decode, grayscale, contrast-stretch and upscale in ONE copied pipeline
    // (src/kstream.rs). The plane goes straight into the engine: no PGM temp file,
    // which at a 3x upscale was ~16 MB written and read back per image.
    let (pixels, w, h) = kstream::preprocess(raw, req.upscale as usize, req.pixel_budget)?;

    // 8-bit grayscale: one byte per pixel, rows tightly packed — the pipeline
    // above allocates exact geometry and never introduces stride padding.
    engine
        .raw
        .set_image(&pixels, w as i32, h as i32, 1, w as i32)
        .map_err(|e| format!("set_image: {e}"))?;

    engine.set_source_resolution(req.dpi as i32);

    engine.get_utf8_text().map_err(|e| format!("ocr: {e}"))
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

/// Turns one request payload into one reply payload.
///
/// Every failure path — unparseable JSON, undecodable base64, a dead engine, an
/// unreadable image — lands in an `error` reply rather than a panic or a
/// dropped message, because the caller is blocked on a reply that only this
/// function can produce.
fn handle_read(engine: &mut Option<(String, TessApi)>, payload: &[u8]) -> Response {
    let mut req: Request = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => return Response::err(format!("malformed request: {e}")),
    };
    req.normalize();

    let raw = match base64::engine::general_purpose::STANDARD.decode(req.image.as_bytes()) {
        Ok(b) => b,
        Err(e) => return Response::err(format!("decode base64 image: {e}")),
    };
    if raw.is_empty() {
        return Response::err("empty image".to_string());
    }

    let tess = match ensure_engine(engine, &req.lang) {
        Ok(t) => t,
        Err(msg) => return Response::err(msg),
    };

    // COMPILE FIX: TessApi exposes no set_variable wrapper; reach through .raw
    // with CStrings.
    if let (Ok(name), Ok(value)) = (
        std::ffi::CString::new("tessedit_pageseg_mode"),
        std::ffi::CString::new(req.psm.to_string()),
    ) {
        tess.raw.set_variable(&name, &value).ok();
    }

    match read_page(tess, &req, &raw) {
        Ok(text) => Response::ok(text),
        Err(msg) => Response::err(msg),
    }
}

/// Everything the daemon needs off the command line.
struct Args {
    nats: String,
    subject_prefix: String,
    workers: usize,
}

/// Parses argv, or exits.
///
/// `--nats` and `--subject-prefix` are REQUIRED with no defaults on purpose.
/// A default URL invites a daemon to attach to whatever server happens to be
/// listening, and a default prefix throws away the whole identity guarantee the
/// per-run-unique prefix exists to provide — both would fail as a silently
/// wrong answer rather than a loud refusal.
fn parse_args() -> Args {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // --version prints and exits BEFORE any connection or engine work, which is
    // what makes it usable as an image-build self-check: a layer that has just
    // downloaded this binary can prove it executes on that userland with no
    // tessdata present and no NATS server running. The build scripts in
    // scripts/ run exactly this. It is NOT the identity probe any more — that
    // is the .ocr.version subject, which proves WHICH process answered, not
    // merely that some binary on disk runs.
    if argv.len() == 1 && argv[0] == "--version" {
        println!("{VERSION}");
        std::process::exit(0);
    }
    if argv.len() == 1 && (argv[0] == "--help" || argv[0] == "-h") {
        help();
    }

    let mut nats = None;
    let mut subject_prefix = None;
    let mut workers = None;

    let mut i = 0;
    while i + 1 < argv.len() {
        let value = argv[i + 1].clone();
        match argv[i].as_str() {
            "--nats" => nats = Some(value),
            "--subject-prefix" => subject_prefix = Some(value),
            "--workers" => match value.parse::<usize>() {
                Ok(n) if n > 0 => workers = Some(n),
                _ => usage(&format!("--workers must be a positive integer, got {value:?}")),
            },
            other => usage(&format!("unknown flag {other:?}")),
        }
        i += 2;
    }
    if i != argv.len() {
        // A LEFTOVER TOKEN HAS TWO DIFFERENT CAUSES, AND SAYING THE WRONG ONE
        // COSTS THE READER THE BUG. A known flag here really is missing its
        // value; anything else was never a flag at all, and reporting THAT as a
        // missing value sends the reader hunting for an argument that was never
        // the problem (`--help` reported "is missing its value" until 2026-08-24).
        let last = argv[i].as_str();
        if matches!(last, "--nats" | "--subject-prefix" | "--workers") {
            usage(&format!("flag {last:?} is missing its value"));
        }
        usage(&format!("unknown flag {last:?}"));
    }

    let (Some(nats), Some(subject_prefix)) = (nats, subject_prefix) else {
        usage("--nats and --subject-prefix are both required");
    };

    Args {
        nats,
        subject_prefix,
        // DEFAULT = AVAILABLE PARALLELISM. The daemon is the only OCR consumer
        // of these cores while a batch runs, so the box's own parallelism is
        // the right bound; `available_parallelism` respects cgroup/affinity
        // limits, which a raw core count does not.
        workers: workers.unwrap_or_else(|| {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
        }),
    }
}

/// The one copy of the usage text, shared by the help request and the error
/// path so the two can never describe different CLIs.
const USAGE: &str = "usage: ocrd --nats <url> --subject-prefix <prefix> [--workers <n>]\n       ocrd --version";

/// A REQUESTED help text is not an error: it goes to stdout and exits 0, so a
/// caller can pipe it without also having to treat success as failure.
fn help() -> ! {
    println!("{USAGE}");
    std::process::exit(0);
}

fn usage(problem: &str) -> ! {
    eprintln!("ocrd: {problem}");
    eprintln!("{USAGE}");
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    // FAIL LOUD, FAIL NOW. There is no fallback transport and no degraded mode:
    // a daemon that cannot reach its server can never answer a single request,
    // and the one thing worse than not starting is appearing to have started.
    // async-nats does not retry an initial connect by default, which is what we
    // want — the caller is waiting on a version reply that will never come, and
    // a non-zero exit tells it so immediately.
    let client = match async_nats::connect(&args.nats).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "ocrd: FATAL: cannot connect to NATS at {}: {e}\n\
                 ocrd: no fallback transport exists; exiting",
                args.nats
            );
            std::process::exit(1);
        }
    };

    let read_subject = format!("{}.ocr.read", args.subject_prefix);
    let version_subject = format!("{}.ocr.version", args.subject_prefix);

    // The server's advertised limit, logged rather than enforced. This client
    // sets no cap of its own; if this number is the 1 MB default then page
    // images WILL be rejected at the publisher, and seeing it in the log is how
    // that gets diagnosed in one step instead of ten.
    eprintln!(
        "ocrd: connected to {} (server max_payload {} bytes)",
        args.nats,
        client.server_info().max_payload
    );

    // THE IDENTITY SUBJECT, and the reason a plain `subscribe` is right here:
    // the caller asks a prefix only this run knows, so exactly one daemon can
    // possibly answer. There is no group to share and nothing to balance.
    let version_client = client.clone();
    let mut version_sub = match version_client.subscribe(version_subject.clone()).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ocrd: FATAL: subscribe {version_subject}: {e}");
            std::process::exit(1);
        }
    };
    tokio::spawn(async move {
        let body = serde_json::to_vec(&VersionResponse { version: VERSION, implementation: IMPL })
            .expect("version reply is two static strings");
        while let Some(msg) = version_sub.next().await {
            if let Some(reply) = msg.reply {
                let _ = version_client.publish(reply, body.clone().into()).await;
            }
        }
    });

    // EXACTLY N SUBSCRIBERS, EXACTLY N CONCURRENT READS. Each task owns its own
    // subscription to the queue group and its own resident engine, and handles
    // one message at a time to completion. The server hands each request to one
    // group member, so the in-flight count cannot exceed the member count —
    // that IS the concurrency bound, with nothing else in the process trying to
    // enforce it a second time.
    //
    // The engine is created LAZILY, on the first message, so a worker that
    // never receives one never pays for a model load.
    for id in 0..args.workers {
        let client = client.clone();
        let subject = read_subject.clone();
        tokio::spawn(async move {
            let mut sub = match client
                .queue_subscribe(subject.clone(), READ_QUEUE_GROUP.to_string())
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    // A worker that cannot subscribe silently shrinks the pool,
                    // which shows up later as unexplained slowness. Refuse to
                    // run in a shape nobody asked for.
                    eprintln!("ocrd: FATAL: worker {id} subscribe {subject}: {e}");
                    std::process::exit(1);
                }
            };
            let mut engine: Option<(String, TessApi)> = None;
            while let Some(msg) = sub.next().await {
                // OCR is blocking C code that runs for seconds. block_in_place
                // hands this thread's remaining tasks to another runtime thread
                // for the duration, so N simultaneous reads cannot starve the
                // connection driver that has to deliver their replies.
                let resp = tokio::task::block_in_place(|| handle_read(&mut engine, &msg.payload));

                // A request with no reply subject is a caller bug, not a read
                // failure, and there is nowhere to report it but the log.
                let Some(reply) = msg.reply else {
                    eprintln!("ocrd: request on {subject} carried no reply subject; dropping");
                    continue;
                };
                if let Err(e) = client.publish(reply, resp.payload().into()).await {
                    // Publish failure means the answer is lost and the caller
                    // is still waiting. Loud, but not fatal: the next request
                    // may well succeed, and killing the daemon would strand
                    // every other in-flight read too.
                    eprintln!("ocrd: publish reply: {e}");
                }
            }
        });
    }

    eprintln!(
        "ocrd: {} workers on {read_subject} (queue group {READ_QUEUE_GROUP}); identity on {version_subject}",
        args.workers
    );

    // Park forever. The daemon's warm engines are the asset; it runs until the
    // machine or an operator stops it. `pending()` blocks this task without
    // burning a thread, leaving the whole runtime to the workers.
    std::future::pending::<()>().await;
}
