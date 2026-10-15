//! One durable screenshot store for local and managed Snapbot.
//!
//! Local runs use one Kumo bucket; CI uses one AWS bucket. The shared SHA-1 key
//! is independent of the test and run, so reports link to the stored object.

use crate::config;
use anyhow::{anyhow, bail, Result};
use aws_sdk_s3::error::ProvideErrorMetadata;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::OnceCell;

static IMAGE_S3: OnceCell<aws_sdk_s3::Client> = OnceCell::const_new();

/// A caller-owned private capability preserves one browse outcome when Lambda's response is lost.
pub async fn put_browse_result(url: &str, payload: &Value, failed: bool, deadline_ms: Option<i64>) -> Result<()> {
    // Never follow a redirect with the private capability, and never include its URL in errors.
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    let client = HTTP.get_or_init(|| reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none()).build().expect("browse result HTTP client"));
    let bytes = serde_json::to_vec(&json!({"payload": payload, "function_error": if failed { "Unhandled" } else { "" }}))?;
    let deadline = deadline_ms.ok_or_else(|| anyhow!("browse result delivery requires the invocation deadline"))?;
    // A lost PUT response may follow a successful write; repeat only these same outcome bytes, never the browse.
    for attempt in 1..=3 {
        let remaining = deadline - crate::js::now_ms();
        if remaining <= 0 {
            bail!("browse result delivery exceeded the invocation deadline");
        }
        let response = client.put(url).header("content-type", "application/json")
            .timeout(Duration::from_millis(remaining as u64)).body(bytes.clone()).send().await;
        match response {
            Ok(response) => {
                if !response.status().is_success() {
                    bail!("browse result delivery returned HTTP {}", response.status().as_u16());
                }
                return Ok(());
            }
            Err(error) => {
                let transport = error.is_connect() || error.is_timeout() || error.is_request() || error.is_body();
                if !transport || attempt == 3 {
                    let kind = if error.is_timeout() { "timeout" } else if error.is_connect() { "connect" } else { "transport" };
                    bail!("browse result delivery failed after {attempt} attempt(s): {kind}: {}", error.without_url());
                }
                // Backoff consumes the original invocation budget rather than extending it.
                let remaining = deadline - crate::js::now_ms();
                if remaining <= 0 {
                    bail!("browse result delivery exceeded the invocation deadline");
                }
                tokio::time::sleep(Duration::from_millis(100.min(remaining as u64))).await;
            }
        }
    }
    unreachable!("each result delivery attempt returns or advances within the fixed limit")
}

// The observed lost PUT response must recover the original envelope without rerunning its workflow.
#[cfg(test)]
#[path = "store_result_test.rs"]
mod result_tests;

// An explicit endpoint keeps all local image traffic inside Kumo. CI omits it
// and writes to the real AWS bucket with its normal task credentials.
async fn image_s3() -> &'static aws_sdk_s3::Client {
    IMAGE_S3.get_or_init(|| async {
        let mut sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"));
        if let Ok(profile) = std::env::var("SNAPBOT_IMAGE_AWS_PROFILE") {
            let provider = aws_config::profile::ProfileFileCredentialsProvider::builder()
                .profile_name(profile)
                .build();
            sdk = sdk.credentials_provider(provider);
        }
        let sdk = sdk.load().await;
        let endpoint = std::env::var("SNAPBOT_IMAGE_S3_ENDPOINT")
            .unwrap_or_else(|_| "https://s3.us-east-1.amazonaws.com".to_owned());
        let conf = aws_sdk_s3::config::Builder::from(&sdk)
            .endpoint_url(endpoint)
            .force_path_style(true)
            .request_checksum_calculation(aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired)
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    }).await
}

/// The bucket every browse artifact goes to.
pub fn bucket() -> Result<String> {
    match std::env::var("SNAPBOT_IMAGE_BUCKET") {
        Ok(bucket) if !bucket.is_empty() => Ok(bucket),
        _ => Ok(config::cfg()?.bucket.clone()),
    }
}

/// SHA-1 names the encoded bytes, so identical screenshots have one stable key.
pub async fn put_content(bytes: Vec<u8>, content_type: &str) -> Result<Value> {
    let sha1 = hex::encode(Sha1::digest(&bytes));
    let key = format!("snapbot/sha1/{}/{}.png", &sha1[..2], sha1);
    let size = bytes.len();
    let bucket = bucket()?;
    // Local reports point at Kumo's host-reachable endpoint; CI links to AWS.
    let url = match std::env::var("SNAPBOT_IMAGE_S3_ENDPOINT") {
        Ok(endpoint) => format!("{}/{bucket}/{key}", endpoint.trim_end_matches('/')),
        Err(_) => format!("https://{bucket}.s3.us-east-1.amazonaws.com/{key}"),
    };
    // A conditional write deduplicates even concurrent captures in a versioned
    // bucket. It also avoids a missing-key HEAD probe, which needs ListBucket.
    let put = image_s3().await
        .put_object()
        .bucket(&bucket)
        .key(&key)
        .if_none_match("*")
        .body(bytes.into())
        .content_type(content_type)
        .send()
        .await;
    let put = match put {
        Ok(put) => put,
        Err(e) if e.as_service_error().is_some_and(|s| s.code() == Some("PreconditionFailed")) => {
            return Ok(json!({"store": "s3", "bucket": bucket, "key": key,
                "url": url, "sha1": sha1,
                "bytes": size, "deduplicated": true}));
        }
        Err(e) => return Err(anyhow!("S3 PutObject {key}: {}", aws_sdk_s3::error::DisplayErrorContext(&e))),
    };
    Ok(json!({"store": "s3", "bucket": bucket, "key": key, "url": url,
              "version_id": put.version_id().unwrap_or(""), "sha1": sha1, "bytes": size}))
}
