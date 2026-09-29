//! The browse artifact store: S3, everywhere.
//!
//! The local pool writes through Kumo's S3 (AWS_ENDPOINT_URL) with the very
//! PutObject the Lambda makes in AWS, so a local run exercises the production
//! path. The former SNAPBOT_STORE=local directory switch is deleted (owner
//! 2026-09-29: "why local is not S3 thru Kumo?").

use crate::config;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Store one artifact under the request's s3_prefix and return where it went
/// plus its sha256. Artifact bytes never travel back in the response.
pub async fn put(event: &Value, name: &str, bytes: &[u8], content_type: &str) -> Result<Value> {
    let sha256 = hex::encode(Sha256::digest(bytes));
    let prefix = match event.get("s3_prefix") {
        Some(Value::String(s)) if !s.is_empty() => s.trim_end_matches('/').to_string(),
        _ => bail!("browse screenshot requires s3_prefix"),
    };
    let key = format!("{prefix}/{name}");
    let cfg = config::cfg()?;
    let put = config::s3()
        .await
        .put_object()
        .bucket(&cfg.bucket)
        .key(&key)
        .body(bytes.to_vec().into())
        .content_type(content_type)
        .send()
        .await
        .map_err(|e| anyhow!("S3 PutObject {key}: {}", aws_sdk_s3::error::DisplayErrorContext(&e)))?;
    Ok(json!({"store": "s3", "bucket": cfg.bucket, "key": key, "version_id": put.version_id().unwrap_or(""), "sha256": sha256, "bytes": bytes.len()}))
}
