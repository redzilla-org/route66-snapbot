//! The PNG writer: a separate, lower-priority process that encodes captured
//! frames to PNG and stores them, off the timed capture path.
//!
//! WHY (owner 2026-10-07, verbatim): "move final PNG compression and storage into
//! a separate writer process; keep the fast in-memory raw-pixel pipe from Chromium
//! (CEF OSR) straight to Tesseract", "persistence doesn't affect pass/fail, so
//! async writing is NOT a coverup", "pass the raw frame byte buffers to the writer
//! process via shared memory, not a pipe", "The writer's buffer is unlimited", and
//! "the ONLY process with a lower CPU priority should be the background writer".
//!
//! WIRE: a SOCK_SEQPACKET unix socket. A frame is one small JSON descriptor plus
//! its memfd passed by SCM_RIGHTS; pixels never cross the socket. The writer
//! answers `done` per frame and `drained` once its whole queue is empty.
//! FAIL HARD: a write error kills the writer; a dead writer kills every client.

use crate::shm::{Frame, Segment};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::oneshot;

/// The writer's readiness line on its stdout, once the socket is listening.
const READY: &str = "SNAPBOT-WRITER-READY";
/// Room for one JSON descriptor (keys are short paths; 64 KiB is ample).
const MSG_MAX: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Socket primitives (SOCK_SEQPACKET + SCM_RIGHTS, plain libc)
// ---------------------------------------------------------------------------

fn sockaddr(path: &str) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        bail!("writer socket path too long: {path}");
    }
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    Ok((addr, len))
}

fn seqpacket() -> Result<OwnedFd> {
    // SAFETY: plain syscall, checked.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        bail!("socket(AF_UNIX, SOCK_SEQPACKET): {}", std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Send one message, optionally carrying one fd.
fn send_msg(sock: RawFd, body: &[u8], fd: Option<RawFd>) -> Result<()> {
    let mut iov = libc::iovec { iov_base: body.as_ptr() as *mut libc::c_void, iov_len: body.len() };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: the control buffer is CMSG_SPACE for exactly one fd.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        }
    }
    // SAFETY: msg points at live buffers for the call's duration.
    let n = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
    if n < 0 || n as usize != body.len() {
        bail!("writer socket send: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Receive one message and its fd, if any. Ok(None) is an orderly EOF.
fn recv_msg(sock: RawFd) -> Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    let mut buf = vec![0u8; MSG_MAX];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: as in send_msg.
    let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        bail!("writer socket receive: {}", std::io::Error::last_os_error());
    }
    if n == 0 {
        return Ok(None);
    }
    if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        bail!("writer socket message truncated");
    }
    let mut fd = None;
    // SAFETY: walking the control buffer recvmsg just filled.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            fd = Some(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>())));
        }
    }
    buf.truncate(n as usize);
    Ok(Some((buf, fd)))
}

// ---------------------------------------------------------------------------
// The writer process: `snapbot png-writer <socket path>`
// ---------------------------------------------------------------------------

struct Conn {
    fd: OwnedFd,
    send: Mutex<()>,
}

impl Conn {
    fn reply(&self, v: Value) {
        let _g = self.send.lock().unwrap();
        // A client that went away needs no answer.
        let _ = send_msg(self.fd.as_raw_fd(), v.to_string().as_bytes(), None);
    }
}

#[derive(Default)]
struct Queue {
    depth: u64,
    bytes_held: u64,
    drains: Vec<(Arc<Conn>, u64)>,
}

fn queue() -> &'static Mutex<Queue> {
    static Q: OnceLock<Mutex<Queue>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(Queue::default()))
}

struct Job {
    conn: Arc<Conn>,
    id: u64,
    key: String,
    frame: Frame,
}

/// Encode + store one frame, then answer and release its segment.
async fn write_one(job: Job) {
    let len = job.frame.seg.len() as u64;
    let bucket = bucket_or_exit();
    let url = format!("s3://{bucket}/{}", job.key);
    let t = Instant::now();
    let Job { conn, id, key, frame } = job;
    let png = match tokio::task::spawn_blocking(move || crate::pngenc::encode(&frame)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => fatal(&format!("encode {url}: {e:#}")),
        Err(e) => fatal(&format!("encode {url}: {e}")),
    };
    let encode_ms = t.elapsed().as_millis() as u64;
    let t = Instant::now();
    let stored = match crate::store::put_key(&key, &png, "image/png").await {
        Ok(v) => v,
        Err(e) => fatal(&format!("store {url}: {e:#}")),
    };
    let write_ms = t.elapsed().as_millis() as u64;
    let (depth, held, drains) = {
        let mut q = queue().lock().unwrap();
        q.depth -= 1;
        q.bytes_held -= len;
        let drains = if q.depth == 0 { std::mem::take(&mut q.drains) } else { Vec::new() };
        (q.depth, q.bytes_held, drains)
    };
    eprintln!(
        "SNAPBOT-PNG-WRITTEN {}",
        json!({"url": url, "png": url, "version_id": stored["version_id"], "sha256": stored["sha256"], "bytes": stored["bytes"],
               "encode_ms": encode_ms, "write_ms": write_ms, "queue_depth": depth, "bytes_held": held})
    );
    conn.reply(json!({"op": "done", "id": id}));
    for (c, d) in drains {
        c.reply(json!({"op": "drained", "id": d}));
    }
}

/// The store's bucket, or a hard exit: a writer that cannot name its bucket can
/// store nothing.
fn bucket_or_exit() -> String {
    crate::store::bucket().unwrap_or_else(|e| fatal(&format!("bucket: {e:#}")))
}

fn fatal(msg: &str) -> ! {
    eprintln!("snapbot png-writer fatal: {msg}");
    std::process::exit(1)
}

/// One client connection: frames and drains until EOF.
fn serve_conn(conn: Arc<Conn>, rt: tokio::runtime::Handle) {
    loop {
        let (body, fd) = match recv_msg(conn.fd.as_raw_fd()) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(e) => fatal(&format!("{e:#}")),
        };
        let v: Value = serde_json::from_slice(&body).unwrap_or_else(|e| fatal(&format!("bad descriptor: {e}")));
        let id = v["id"].as_u64().unwrap_or_else(|| fatal("descriptor without id"));
        match v["op"].as_str() {
            Some("frame") => {
                let n = |k: &str| v[k].as_u64().unwrap_or_else(|| fatal(&format!("frame descriptor without {k}"))) as usize;
                let (w, h, stride) = (n("width"), n("height"), n("stride"));
                let key = v["key"].as_str().unwrap_or_else(|| fatal("frame descriptor without key")).to_string();
                let fd = fd.unwrap_or_else(|| fatal("frame descriptor without its memfd"));
                let seg = Segment::from_fd(fd, stride * h).unwrap_or_else(|e| fatal(&format!("{e:#}")));
                let len = seg.len() as u64;
                let (depth, held) = {
                    let mut q = queue().lock().unwrap();
                    q.depth += 1;
                    q.bytes_held += len;
                    (q.depth, q.bytes_held)
                };
                eprintln!("SNAPBOT-WRITER-QUEUE {}", json!({"key": key, "queue_depth": depth, "bytes_held": held}));
                let job = Job { conn: conn.clone(), id, key, frame: Frame { seg, width: w, height: h, stride } };
                rt.spawn(write_one(job));
            }
            Some("drain") => {
                let mut q = queue().lock().unwrap();
                if q.depth == 0 {
                    drop(q);
                    conn.reply(json!({"op": "drained", "id": id}));
                } else {
                    q.drains.push((conn.clone(), id));
                }
            }
            other => fatal(&format!("unknown op {other:?}")),
        }
    }
}

/// `snapbot png-writer <socket>`: listen, announce readiness, serve forever.
pub fn serve(path: &str) -> Result<()> {
    // WHY (owner 2026-10-07, verbatim: "the ONLY process with a lower CPU priority
    // should be the background writer"): persistence never decides pass/fail, so
    // its encode yields the CPU to capture and OCR. Nothing else changes priority.
    // SAFETY: plain syscall on this process.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) } != 0 {
        bail!("setpriority(10): {}", std::io::Error::last_os_error());
    }
    let _ = std::fs::remove_file(path);
    let sock = seqpacket()?;
    let (addr, len) = sockaddr(path)?;
    // SAFETY: binding the socket we own to a valid sockaddr_un.
    if unsafe { libc::bind(sock.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) } != 0 {
        bail!("bind {path}: {}", std::io::Error::last_os_error());
    }
    if unsafe { libc::listen(sock.as_raw_fd(), 128) } != 0 {
        bail!("listen {path}: {}", std::io::Error::last_os_error());
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    println!("{READY}");
    loop {
        // SAFETY: accept on our listening socket.
        let c = unsafe { libc::accept4(sock.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if c < 0 {
            bail!("accept {path}: {}", std::io::Error::last_os_error());
        }
        let conn = Arc::new(Conn { fd: unsafe { OwnedFd::from_raw_fd(c) }, send: Mutex::new(()) });
        let h = rt.handle().clone();
        std::thread::spawn(move || serve_conn(conn, h));
    }
}

// ---------------------------------------------------------------------------
// Spawning the writer (pool dispatcher, or a standalone Lambda process)
// ---------------------------------------------------------------------------

/// Start `snapbot png-writer <path>` and wait for its readiness line.
pub fn spawn(path: &str) -> Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .arg("png-writer")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| anyhow!("spawn png-writer: {e}"))?;
    let out = child.stdout.take().ok_or_else(|| anyhow!("png-writer stdout"))?;
    let mut line = String::new();
    BufReader::new(out).read_line(&mut line)?;
    if line.trim() != READY {
        bail!("png-writer did not become ready (said {line:?})");
    }
    Ok(child)
}

// ---------------------------------------------------------------------------
// The client side (a capturing lane or Lambda process)
// ---------------------------------------------------------------------------

struct Client {
    fd: OwnedFd,
    send: Mutex<()>,
    next: AtomicU64,
    outstanding: Mutex<HashMap<u64, ()>>,
    idle: tokio::sync::Notify,
    drains: Mutex<HashMap<u64, oneshot::Sender<()>>>,
}

fn connect(path: &str) -> Result<Client> {
    let sock = seqpacket()?;
    let (addr, len) = sockaddr(path)?;
    // SAFETY: connecting our socket to a valid sockaddr_un.
    if unsafe { libc::connect(sock.as_raw_fd(), (&addr as *const libc::sockaddr_un).cast(), len) } != 0 {
        bail!("connect png-writer at {path}: {}", std::io::Error::last_os_error());
    }
    Ok(Client {
        fd: sock,
        send: Mutex::new(()),
        next: AtomicU64::new(1),
        outstanding: Mutex::new(HashMap::new()),
        idle: tokio::sync::Notify::new(),
        drains: Mutex::new(HashMap::new()),
    })
}

/// This process's writer connection. The pool dispatcher's writer is named by
/// SNAPBOT_WRITER_SOCK; a process without one (the managed Lambda) starts its
/// own. A writer that dies takes this process down with it.
static CLIENT: OnceLock<&'static Client> = OnceLock::new();

/// Whether this process ever connected to a writer (submitted or ensured).
pub fn started() -> bool {
    CLIENT.get().is_some()
}

fn client() -> &'static Client {
    CLIENT.get_or_init(|| {
        let path = match std::env::var("SNAPBOT_WRITER_SOCK") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                let p = std::env::temp_dir().join(format!("snapbot-writer-{}.sock", std::process::id())).display().to_string();
                let mut child = spawn(&p).unwrap_or_else(|e| fatal(&format!("{e:#}")));
                std::thread::spawn(move || {
                    let status = child.wait();
                    eprintln!("snapbot fatal: png-writer exited ({status:?})");
                    std::process::exit(1);
                });
                p
            }
        };
        let c: &'static Client = Box::leak(Box::new(connect(&path).unwrap_or_else(|e| fatal(&format!("{e:#}")))));
        std::thread::spawn(move || loop {
            match recv_msg(c.fd.as_raw_fd()) {
                Ok(Some((body, _))) => {
                    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let id = v["id"].as_u64().unwrap_or(0);
                    match v["op"].as_str() {
                        Some("done") => {
                            let mut o = c.outstanding.lock().unwrap();
                            o.remove(&id);
                            if o.is_empty() {
                                c.idle.notify_waiters();
                            }
                        }
                        Some("drained") => {
                            if let Some(tx) = c.drains.lock().unwrap().remove(&id) {
                                let _ = tx.send(());
                            }
                        }
                        _ => {}
                    }
                }
                Ok(None) | Err(_) => {
                    eprintln!("snapbot fatal: the png-writer connection closed (writer died)");
                    std::process::exit(1);
                }
            }
        });
        c
    })
}

/// Connect now, so a lane fails at startup rather than at its first shot.
pub fn ensure() {
    let _ = client();
}

/// Hand `frame` to the writer: its memfd plus a descriptor. Returns at once;
/// the encode and the store happen in the writer.
pub fn submit(frame: &Frame, key: &str) -> Result<()> {
    let c = client();
    let id = c.next.fetch_add(1, Ordering::SeqCst);
    let body = json!({"op": "frame", "id": id, "key": key, "width": frame.width, "height": frame.height, "stride": frame.stride});
    c.outstanding.lock().unwrap().insert(id, ());
    let _g = c.send.lock().unwrap();
    send_msg(c.fd.as_raw_fd(), body.to_string().as_bytes(), Some(frame.seg.raw_fd()))
}

/// Wait until every frame THIS process submitted is stored.
pub async fn drain_own() {
    let c = client();
    loop {
        let notified = c.idle.notified();
        if c.outstanding.lock().unwrap().is_empty() {
            return;
        }
        notified.await;
    }
}

/// Wait until the writer's whole queue (every client's frames) is stored.
pub async fn drain_all() -> Result<()> {
    let c = client();
    let id = c.next.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    c.drains.lock().unwrap().insert(id, tx);
    {
        let _g = c.send.lock().unwrap();
        send_msg(c.fd.as_raw_fd(), json!({"op": "drain", "id": id}).to_string().as_bytes(), None)?;
    }
    rx.await.map_err(|_| anyhow!("png-writer drain reply lost"))
}
