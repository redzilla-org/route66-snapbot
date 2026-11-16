//! Encode and store each frame in its browser lane process.
//!
//! The task reads the same BGRA memory OCR uses, without a writer process or
//! socket. Its handle is joined after OCR so the step returns a durable SHA-1
//! content address and fails if encoding or storage failed.

use crate::shm::Frame;
use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::Arc;
use tokio::task::JoinHandle;

/// Start PNG compression and S3 storage while the lane runs OCR.
pub fn submit(frame: &Arc<Frame>) -> JoinHandle<Result<Value>> {
    let frame = Arc::clone(frame);
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let png = tokio::task::spawn_blocking(move || crate::pngenc::encode(&frame))
            .await
            .context("PNG encoder task exited")??;
        let encode_ms = started.elapsed().as_millis() as u64;
        let started = std::time::Instant::now();
        let mut stored = crate::store::put_content(png, "image/png").await?;
        let write_ms = started.elapsed().as_millis() as u64;
        eprintln!("SNAPBOT-PNG-WRITTEN {}", serde_json::json!({"url": stored["url"], "bytes": stored["bytes"],
            "sha1": stored["sha1"], "encode_ms": encode_ms, "write_ms": write_ms}));
        // The shot's per-reader timings (browse.rs moves them into its timings).
        stored["encode_ms"] = encode_ms.into();
        stored["write_ms"] = write_ms.into();
        Ok(stored)
    })
}

/// A screenshot step joins only its own write, with no pool-wide barrier.
pub async fn finish(job: JoinHandle<Result<Value>>) -> Result<Value> {
    job.await.context("screenshot writer task exited")?
}
