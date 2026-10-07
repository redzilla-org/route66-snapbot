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

/// The key an artifact named `name` lands at under the request's s3_prefix.
/// Chosen before the write, so the step response and its log lines can name
/// the PNG while the writer process is still encoding it.
pub fn key(event: &Value, name: &str) -> Result<String> {
    let prefix = match event.get("s3_prefix") {
        Some(Value::String(s)) if !s.is_empty() => s.trim_end_matches('/').to_string(),
        _ => bail!("browse screenshot requires s3_prefix"),
    };
    Ok(format!("{prefix}/{name}"))
}

/// The bucket every browse artifact goes to.
pub fn bucket() -> Result<String> {
    Ok(config::cfg()?.bucket.clone())
}

/// Store one artifact at `key` and return where it went plus its sha256.
pub async fn put_key(key: &str, bytes: &[u8], content_type: &str) -> Result<Value> {
    let sha256 = hex::encode(Sha256::digest(bytes));
    let cfg = config::cfg()?;
    let put = config::s3()
        .await
        .put_object()
        .bucket(&cfg.bucket)
        .key(key)
        .body(bytes.to_vec().into())
        .content_type(content_type)
        .send()
        .await
        .map_err(|e| anyhow!("S3 PutObject {key}: {}", aws_sdk_s3::error::DisplayErrorContext(&e)))?;
    Ok(json!({"store": "s3", "bucket": cfg.bucket, "key": key, "version_id": put.version_id().unwrap_or(""), "sha256": sha256, "bytes": bytes.len()}))
}
