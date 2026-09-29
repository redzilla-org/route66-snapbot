//! Handler configuration and the shared AWS clients.
//!
//! The environment contract is unchanged from the Node handler: the five
//! EVIDENCE_ATTESTOR_* variables are required at startup (a misconfigured
//! function fails its first invocation loudly), and every client is built
//! once per process for EVIDENCE_ATTESTOR_REGION.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::OnceLock;
use tokio::sync::OnceCell;

pub struct Cfg {
    pub region: String,
    pub bucket: String,
    pub ssm_param: String,
    pub ci_read_role_name: String,
    /// {"<env>": {"account": "...", "region": "..."}} derived by deploy.py.
    pub dev_ci: Value,
}

static CFG: OnceLock<std::result::Result<Cfg, String>> = OnceLock::new();

fn required(name: &str) -> std::result::Result<String, String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("missing required environment variable {name}")),
    }
}

fn load() -> std::result::Result<Cfg, String> {
    let dev_ci = required("EVIDENCE_ATTESTOR_DEV_CI_JSON")?;
    Ok(Cfg {
        region: required("EVIDENCE_ATTESTOR_REGION")?,
        bucket: required("EVIDENCE_ATTESTOR_BUCKET")?,
        ssm_param: required("EVIDENCE_ATTESTOR_SSM_PARAM")?,
        ci_read_role_name: required("EVIDENCE_ATTESTOR_CI_READ_ROLE_NAME")?,
        dev_ci: serde_json::from_str(&dev_ci).map_err(|e| format!("EVIDENCE_ATTESTOR_DEV_CI_JSON: {e}"))?,
    })
}

pub fn cfg() -> Result<&'static Cfg> {
    CFG.get_or_init(load).as_ref().map_err(|e| anyhow!("{e}"))
}

static SDK: OnceCell<aws_config::SdkConfig> = OnceCell::const_new();

/// The ambient SDK config for the handler's own region. AWS_ENDPOINT_URL is
/// honored, which is how the local pool reaches Kumo.
pub async fn sdk() -> &'static aws_config::SdkConfig {
    SDK.get_or_init(|| async {
        let region = cfg().map(|c| c.region.clone()).unwrap_or_else(|_| "us-west-2".to_string());
        aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region))
            .load()
            .await
    })
    .await
}

static S3: OnceCell<aws_sdk_s3::Client> = OnceCell::const_new();
static SSM: OnceCell<aws_sdk_ssm::Client> = OnceCell::const_new();
static STS: OnceCell<aws_sdk_sts::Client> = OnceCell::const_new();
static LAMBDA: OnceCell<aws_sdk_lambda::Client> = OnceCell::const_new();

/// Path-style addressing against a custom endpoint (Kumo, the self-test), where
/// bucket.<host> virtual hosts do not resolve; checksums only when required, so
/// the emulator receives a plain body.
pub async fn s3() -> &'static aws_sdk_s3::Client {
    S3.get_or_init(|| async {
        let custom = std::env::var("AWS_ENDPOINT_URL").is_ok_and(|v| !v.is_empty());
        let conf = aws_sdk_s3::config::Builder::from(sdk().await)
            .force_path_style(custom)
            .request_checksum_calculation(aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired)
            .build();
        aws_sdk_s3::Client::from_conf(conf)
    })
    .await
}
pub async fn ssm() -> &'static aws_sdk_ssm::Client {
    SSM.get_or_init(|| async { aws_sdk_ssm::Client::new(sdk().await) }).await
}
pub async fn sts() -> &'static aws_sdk_sts::Client {
    STS.get_or_init(|| async { aws_sdk_sts::Client::new(sdk().await) }).await
}
pub async fn lambda() -> &'static aws_sdk_lambda::Client {
    LAMBDA.get_or_init(|| async { aws_sdk_lambda::Client::new(sdk().await) }).await
}
