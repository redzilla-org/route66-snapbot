//! One durable screenshot store for local and managed Snapbot.
//!
//! Kumo still serves the local Runtime API, but screenshots go directly to the
//! command-center test-results bucket. The shared SHA-1 key is independent of
//! the test, run, and environment, so reports link to the same object.

use crate::config;
use anyhow::{anyhow, Result};
use aws_sdk_s3::error::ProvideErrorMetadata;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use tokio::sync::OnceCell;

static IMAGE_S3: OnceCell<aws_sdk_s3::Client> = OnceCell::const_new();

// The local pool carries emulator credentials for Lambda invocation. Override
// them only for image writes, using its mounted read-only command-center profile.
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
        // AWS_ENDPOINT_URL points at Kumo for the Runtime API; never let it
        // redirect image traffic away from the one central S3 bucket.
        let conf = aws_sdk_s3::config::Builder::from(&sdk)
            .endpoint_url("https://s3.us-east-1.amazonaws.com")
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
    let url = format!("https://{bucket}.s3.us-east-1.amazonaws.com/{key}");
    // Avoid a second write when another run already captured identical bytes.
    match image_s3().await.head_object().bucket(&bucket).key(&key).send().await {
        Ok(head) => return Ok(json!({"store": "s3", "bucket": bucket, "key": key,
            "url": url, "version_id": head.version_id().unwrap_or(""), "sha1": sha1,
            "bytes": size, "deduplicated": true})),
        Err(e) if e.as_service_error().is_some_and(|s| s.is_not_found()) => {},
        Err(e) => return Err(anyhow!("S3 HeadObject {key}: {}", aws_sdk_s3::error::DisplayErrorContext(&e))),
    }
    // The conditional write closes the race between concurrent captures of
    // identical pixels, including in a versioned bucket.
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
            let head = image_s3().await.head_object().bucket(&bucket).key(&key).send().await
                .map_err(|head| anyhow!("S3 HeadObject after duplicate {key}: {}", aws_sdk_s3::error::DisplayErrorContext(&head)))?;
            return Ok(json!({"store": "s3", "bucket": bucket, "key": key,
                "url": url, "version_id": head.version_id().unwrap_or(""), "sha1": sha1,
                "bytes": size, "deduplicated": true}));
        }
        Err(e) => return Err(anyhow!("S3 PutObject {key}: {}", aws_sdk_s3::error::DisplayErrorContext(&e))),
    };
    Ok(json!({"store": "s3", "bucket": bucket, "key": key, "url": url,
              "version_id": put.version_id().unwrap_or(""), "sha1": sha1, "bytes": size}))
}
