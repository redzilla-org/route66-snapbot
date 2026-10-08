//! The local pool for Kumo attachment (route66 local-verify).
//!
//! Kumo exposes the standard Lambda Runtime API and hands each invocation to
//! whichever poller is waiting on /next, so this needs no scheduler: it starts
//! SNAPBOT_POOL_PROCESSES lane processes (this binary, `kumo-lane`), each one
//! warm browser plus one poller and in-process image writer. Owner 2026-09-26, verbatim: "snapbot's lambda
//! pool should be at least 40 processes"; fewer is refused. Any lane exit after
//! startup is fatal: the pool is either full width or down, and the container's
//! restart policy brings it back warm.

use anyhow::{anyhow, bail, Result};
use std::collections::HashMap;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const POOL_MIN_PROCESSES: usize = 40;
const POOL_STARTUP_TIMEOUT: Duration = Duration::from_secs(180);
/// A lane reports readiness as one line with this prefix on its stdout.
const READY: &str = "\u{1}SNAPBOT-LANE-READY ";

/// Sum VmRSS over a process and its descendants (the lane plus its Chromium).
fn tree_rss_kib(root: u32) -> u64 {
    let mut parent = HashMap::new();
    let mut rss = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
            let Ok(status) = std::fs::read_to_string(e.path().join("status")) else { continue };
            let field = |k: &str| status.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok());
            parent.insert(pid, field("PPid:").unwrap_or(0) as u32);
            rss.insert(pid, field("VmRSS:").unwrap_or(0));
        }
    }
    let mut total = 0;
    for (&pid, &kib) in &rss {
        let mut p = pid;
        while p != 0 {
            if p == root {
                total += kib;
                break;
            }
            p = *parent.get(&p).unwrap_or(&0);
        }
    }
    total
}

pub async fn run_pool() -> Result<()> {
    crate::runtime::runtime_base()?;
    let n: usize = match std::env::var("SNAPBOT_POOL_PROCESSES") {
        Ok(v) => v.parse().map_err(|_| anyhow!("SNAPBOT_POOL_PROCESSES must be an integer >= {POOL_MIN_PROCESSES} (owner floor); got {v}"))?,
        Err(_) => POOL_MIN_PROCESSES,
    };
    if n < POOL_MIN_PROCESSES {
        bail!("SNAPBOT_POOL_PROCESSES must be an integer >= {POOL_MIN_PROCESSES} (owner floor); got {n}");
    }
    let started = Instant::now();
    let exe = std::env::current_exe()?;
    // Each warm lane now encodes and stores its own frames in background tasks,
    // so the dispatcher does not launch or coordinate a shared PNG process.
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, u32, u64)>();
    let (exit_tx, mut exit_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, String)>();
    for i in 0..n {
        let mut child = tokio::process::Command::new(&exe)
            .arg("kumo-lane")
            .env("SNAPBOT_LANE", i.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let pid = child.id().unwrap_or(0);
        let mut out = BufReader::new(child.stdout.take().ok_or_else(|| anyhow!("lane stdout"))?).lines();
        let tx = ready_tx.clone();
        // Forward the lane's log lines; the readiness line is the handshake.
        tokio::spawn(async move {
            while let Ok(Some(line)) = out.next_line().await {
                if let Some(ms) = line.strip_prefix(READY) {
                    let _ = tx.send((i, pid, ms.trim().parse().unwrap_or(0)));
                } else {
                    println!("{line}");
                }
            }
        });
        let etx = exit_tx.clone();
        // The lane's stdin stays open for its lifetime: EOF tells it the pool died.
        let stdin = child.stdin.take();
        tokio::spawn(async move {
            let _keep = stdin;
            let status = child.wait().await.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
            let _ = etx.send((i, status));
        });
    }
    let mut up: Vec<(usize, u32, u64)> = Vec::new();
    let deadline = tokio::time::Instant::now() + POOL_STARTUP_TIMEOUT;
    while up.len() < n {
        tokio::select! {
            Some(r) = ready_rx.recv() => up.push(r),
            Some((i, status)) = exit_rx.recv() => bail!("lane {i} exited before ready ({status})"),
            _ = tokio::time::sleep_until(deadline) => bail!("pool not ready within {}ms", POOL_STARTUP_TIMEOUT.as_millis()),
        }
    }
    let mut rss: Vec<u64> = up.iter().map(|(_, pid, _)| tree_rss_kib(*pid)).collect();
    let total: u64 = rss.iter().sum();
    rss.sort_unstable();
    let mut warm: Vec<u64> = up.iter().map(|r| r.2).collect();
    warm.sort_unstable();
    println!(
        "snapbot pool ready: {}/{n} processes in {}ms (chromium extract 0ms, lane warm p50 {}ms max {}ms) rss/process KiB p50 {} max {} total {total}",
        up.len(),
        started.elapsed().as_millis(),
        warm[warm.len() / 2],
        warm[warm.len() - 1],
        rss[rss.len() / 2],
        rss[rss.len() - 1],
    );
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let fatal = tokio::select! {
        Some((i, status)) = exit_rx.recv() => format!("snapbot pool fatal: lane {i} exited ({status}); the pool is below its {n}-process width"),
        _ = term.recv() => String::new(),
    };
    // Every browse joins its own writes before replying, so shutdown has no
    // shared writer queue to drain.
    if fatal.is_empty() {
        return Ok(());
    }
    bail!("{fatal}")
}

/// One pool lane: warm browser, readiness line, then the Runtime API loop.
pub async fn run_lane(handler: String) -> Result<()> {
    let started = Instant::now();
    let args: serde_json::Value = match std::env::var("SNAPBOT_BROWSER_ARGS") {
        Ok(v) if !v.is_empty() => serde_json::from_str(&v)?,
        _ => serde_json::json!([]),
    };
    crate::browse::warm(&args).await?;
    // No writer handshake is needed: the lane's first screenshot schedules a
    // local Rust task and the browse joins it before returning.
    println!("{READY}{}", started.elapsed().as_millis());
    // A lane outliving its dispatcher would keep a browser nobody supervises.
    tokio::spawn(async {
        let mut sink = [0u8; 64];
        let mut stdin = tokio::io::stdin();
        while matches!(stdin.read(&mut sink).await, Ok(n) if n > 0) {}
        eprintln!("snapbot lane: dispatcher gone; exiting");
        std::process::exit(1);
    });
    crate::runtime::run(handler).await
}
