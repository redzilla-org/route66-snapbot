//! Signed evidence attestation (canonicalization v4).
//!
//! WHY THIS EXISTS: public S3 capability URLs prove only possession of an
//! unguessable key. Ticket evidence also needs tamper evidence: a verifier must
//! fetch the exact object version, hash the bytes, and check a command-center
//! signature without trusting whoever uploaded the file. This handler is the
//! single holder of the private signing key.
//!
//! WHAT IS SIGNED (owner 2026-08-26: "the attestor should only sign what it
//! retrieves"): facts this handler observed itself -- the S3 object it fetched
//! and hashed, the dev CI state it read (raw, never judged: owner 2026-09-13,
//! "snapbot does not judge, it only snaps."), and for captures it made, what it
//! requested, received and perturbed. Canonicalization is unchanged from the
//! Node handler byte for byte: route66's verifiers rebuild it.

use crate::config;
use crate::js;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

/// Hard-coded by owner directive 2026-08-26 ("the AWS key should be hard-coded
/// (not Lambda env)"). Rotating the signing key replaces this constant, the
/// wrapped seed in SSM and the public key below in one change.
const UNWRAP_KEY_B64: &str = "o0nZu1JyR60uvbITWZggDAefj+4EGDyzbisahMYXId8=";

/// The public key route66 pins under command-center/evidence-attestor/.
pub const KEY_ID: &str = "15f05edcd4347a1d";
pub const PUBLIC_KEY_B64: &str = "KM7O2UaZph8v0tRbZjuwdaMv2MlRh4pvC2wLZs+sngg=";
pub const ALGORITHM: &str = "Ed25519";

/// v4 since GH #3840: the derived ci.true-green / ci.sha-match lines are gone.
pub const CANONICALIZATION: &str = "r66-evidence-attestation-v4";
const STATEMENT_VERSION: i64 = 4;

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(s: &str) -> Result<Vec<u8>> {
    Ok(base64::engine::general_purpose::STANDARD.decode(s)?)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// safeSegment: an S3 key segment from caller text.
pub fn safe_segment(v: Option<&Value>) -> String {
    let s = js::metadata_value(v, 160);
    let mut out = String::new();
    let mut in_bad = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
            in_bad = false;
        } else if !in_bad {
            out.push('-');
            in_bad = true;
        }
    }
    let t = out.trim_matches('-').to_string();
    if t.is_empty() {
        "evidence".to_string()
    } else {
        t
    }
}

/// Every URL names the exact version it attested (owner 2026-08-26).
pub fn public_url(bucket: &str, key: &str, version_id: &str) -> Result<String> {
    let region = &config::cfg()?.region;
    let path: Vec<String> = key.split('/').map(js::encode_uri_component).collect();
    let base = format!("https://{bucket}.s3.{region}.amazonaws.com/{}", path.join("/"));
    Ok(if version_id.is_empty() { base } else { format!("{base}?versionId={}", js::encode_uri_component(version_id)) })
}

fn manifest_line(k: &str, v: &Value) -> String {
    let s = match v {
        Value::Null => String::new(),
        other => js::js_string(other),
    };
    format!("{k}={}\n", s.replace(['\r', '\n'], " "))
}

/// The canonical v4 manifest: one line per fact, observed keys sorted.
pub fn canonical_manifest(obj: &Value, observed: &Map<String, Value>) -> String {
    let mut out = format!("{CANONICALIZATION}\n");
    for (line, field) in [
        ("bucket", "bucket"),
        ("key", "key"),
        ("version-id", "version_id"),
        ("sha256", "sha256"),
        ("content-length", "content_length"),
        ("content-type", "content_type"),
        ("last-modified", "last_modified"),
        ("etag", "etag"),
        ("attested-at-utc", "attested_at_utc"),
    ] {
        out.push_str(&manifest_line(line, obj.get(field).unwrap_or(&Value::Null)));
    }
    let mut keys: Vec<&String> = observed.keys().collect();
    // JS default sort compares UTF-16 code units; keys here are ASCII.
    keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    for k in keys {
        out.push_str(&manifest_line(&format!("observed.{k}"), &observed[k.as_str()]));
    }
    out
}

fn key_slot() -> &'static Mutex<Option<SigningKey>> {
    static KEY: std::sync::OnceLock<Mutex<Option<SigningKey>>> = std::sync::OnceLock::new();
    KEY.get_or_init(|| Mutex::new(None))
}

fn decrypt_private_key(blob: &str) -> Result<SigningKey> {
    let wrapped: Value = serde_json::from_str(blob)?;
    if wrapped["v"] != json!(1) || wrapped["alg"] != json!("aes-256-gcm") {
        bail!("unsupported private-key wrapper");
    }
    let cipher = Aes256Gcm::new_from_slice(&unb64(UNWRAP_KEY_B64)?).map_err(|e| anyhow!("unwrap key: {e}"))?;
    let nonce = unb64(wrapped["nonce_b64"].as_str().unwrap_or(""))?;
    let ct = unb64(wrapped["ciphertext_b64"].as_str().unwrap_or(""))?;
    let aad = wrapped["aad"].as_str().unwrap_or("").as_bytes().to_vec();
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: &aad })
        .map_err(|_| anyhow!("Unsupported state or unable to authenticate data"))?;
    let key: Value = serde_json::from_slice(&plain)?;
    if key["v"] != json!(1) || key["alg"] != json!("ed25519") {
        bail!("unsupported signing key");
    }
    if key["key_id"] != json!(KEY_ID) || key["public_key_b64"] != json!(PUBLIC_KEY_B64) {
        bail!("SSM private key does not match repo-pinned public key");
    }
    let seed: [u8; 32] = unb64(key["private_seed_b64"].as_str().unwrap_or(""))?
        .try_into()
        .map_err(|_| anyhow!("signing seed must be 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

async fn private_key() -> Result<SigningKey> {
    let mut guard = key_slot().lock().await;
    if let Some(k) = guard.as_ref() {
        return Ok(k.clone());
    }
    let cfg = config::cfg()?;
    let res = config::ssm()
        .await
        .get_parameter()
        .name(&cfg.ssm_param)
        .with_decryption(false)
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_ssm::error::DisplayErrorContext(&e)))?;
    let v = res
        .parameter()
        .and_then(|p| p.value())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("SSM parameter {} has no value", cfg.ssm_param))?;
    let k = decrypt_private_key(v)?;
    *guard = Some(k.clone());
    Ok(k)
}

/// Verify a signature against the pinned public key.
pub fn verify(manifest: &str, signature_b64: &str) -> bool {
    let (Ok(pk), Ok(sig)) = (unb64(PUBLIC_KEY_B64), unb64(signature_b64)) else { return false };
    let Ok(pk) = <[u8; 32]>::try_from(pk.as_slice()) else { return false };
    let Ok(sig) = <[u8; 64]>::try_from(sig.as_slice()) else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
    vk.verify(manifest.as_bytes(), &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}

/// Alt text built only from signed observed fields.
fn inline_image_alt(statement: &Value) -> String {
    let o = &statement["observed"];
    let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let mut parts: Vec<String> = vec![
        if s("capture.evidence-type").is_empty() { "evidence".to_string() } else { s("capture.evidence-type") },
        if s("capture.final-url").is_empty() { s("capture.requested-url") } else { s("capture.final-url") },
        s("capture.viewport"),
    ];
    parts.retain(|p| !p.is_empty());
    let sha = statement["object"]["sha256"].as_str().unwrap_or("");
    parts.push(format!("sha256 {}", &sha[..sha.len().min(12)]));
    parts.join(" \u{2014} ").replace(['[', ']'], "")
}

fn render_evidence_text(statement: &Value, url: &str, attestation_url: &str) -> String {
    let o = &statement["observed"];
    let s = |v: &Value| match v {
        Value::Null => "undefined".to_string(),
        other => js::js_string(other),
    };
    let mut lines = vec!["-----BEGIN ROUTE66 SIGNED ATTESTATION-----".to_string(), String::new()];
    if statement["object"]["content_type"].as_str().unwrap_or("").starts_with("image/") {
        lines.push(format!("![{}]({url})", inline_image_alt(statement)));
        lines.push(String::new());
    }
    lines.push(format!("**{}: {}**", s(&o["claim.category"]), s(&o["claim.intent"])));
    lines.push(String::new());
    lines.push(format!("Target: {}", s(&o["claim.target"])));
    lines.push(String::new());
    lines.push(format!("Attested: {}", s(&statement["object"]["attested_at_utc"])));
    lines.push(String::new());
    lines.push(format!("Evidence: <{url}>"));
    lines.push(String::new());
    lines.push(format!("Statement: <{attestation_url}>"));
    lines.push(String::new());
    lines.push(format!("{} \u{b7} Key-ID: {}", s(&statement["canonicalization"]), s(&statement["key_id"])));
    lines.push(String::new());
    lines.push("-----END ROUTE66 SIGNED ATTESTATION-----".to_string());
    lines.join("\n")
}

/// Attest one object version: fetch, hash, sign, and write the sidecar.
pub async fn attest(key: &str, version_id: &str, observed: Map<String, Value>) -> Result<Value> {
    if key.is_empty() {
        bail!("missing object key");
    }
    let cfg = config::cfg()?;
    let s3 = config::s3().await;
    let mut head = s3.head_object().bucket(&cfg.bucket).key(key);
    if !version_id.is_empty() {
        head = head.version_id(version_id);
    }
    let head = head.send().await.map_err(|e| anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))?;
    let vid = head.version_id().map(str::to_string).filter(|v| !v.is_empty()).unwrap_or_else(|| version_id.to_string());
    if vid.is_empty() {
        bail!("object has no VersionId; the evidence bucket must be versioned");
    }
    let get = s3
        .get_object()
        .bucket(&cfg.bucket)
        .key(key)
        .version_id(&vid)
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))?;
    let bytes = get.body.collect().await.map_err(|e| anyhow!("read s3://{}/{key}: {e}", cfg.bucket))?.into_bytes();
    let obj = json!({
        "bucket": cfg.bucket,
        "key": key,
        "version_id": vid,
        "sha256": sha256_hex(&bytes),
        "content_length": head.content_length().map(|n| n.to_string()).unwrap_or_default(),
        "content_type": head.content_type().unwrap_or(""),
        "last_modified": head.last_modified().map(js::iso_from_smithy).unwrap_or_default(),
        "etag": head.e_tag().unwrap_or(""),
        "attested_at_utc": js::now_iso(),
    });
    let manifest = canonical_manifest(&obj, &observed);
    let signature = private_key().await?.sign(manifest.as_bytes());
    let statement = json!({
        "v": STATEMENT_VERSION,
        "canonicalization": CANONICALIZATION,
        "algorithm": ALGORITHM,
        "key_id": KEY_ID,
        "public_key_b64": PUBLIC_KEY_B64,
        "object": obj,
        "observed": observed,
        "manifest": manifest,
        "manifest_sha256": sha256_hex(manifest.as_bytes()),
        "signature_b64": b64(&signature.to_bytes()),
    });
    let sidecar_key = format!("{key}.attestation.json");
    let put = s3
        .put_object()
        .bucket(&cfg.bucket)
        .key(&sidecar_key)
        .body(serde_json::to_vec_pretty(&statement)?.into())
        .content_type("application/json")
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))?;
    let url = public_url(&cfg.bucket, key, &vid)?;
    let attestation_url = public_url(&cfg.bucket, &sidecar_key, put.version_id().unwrap_or(""))?;
    let mut out = statement.clone();
    out["url"] = json!(url);
    out["attestation_key"] = json!(sidecar_key);
    out["attestation_url"] = json!(attestation_url);
    out["evidence_text"] = json!(render_evidence_text(&statement, &url, &attestation_url));
    Ok(out)
}

/// PutObject a generated evidence object with its informational labels.
pub async fn put_evidence(key: &str, body: Vec<u8>, content_type: &str, metadata: &[(&str, String)]) -> Result<String> {
    let cfg = config::cfg()?;
    let mut req = config::s3().await.put_object().bucket(&cfg.bucket).key(key).body(body.into()).content_type(content_type);
    for (k, v) in metadata {
        req = req.metadata(*k, v);
    }
    let put = req.send().await.map_err(|e| anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))?;
    Ok(put.version_id().unwrap_or("").to_string())
}

/// `<env>/issue-evidence/issue-<n>/<uuid>/<stamp>-<env>-<suffix>`.
pub fn evidence_key(env: &str, issue: &str, stamp: &str, suffix: &str) -> String {
    format!(
        "{env}/issue-evidence/issue-{}/{}/{stamp}-{}-{suffix}",
        safe_segment(Some(&json!(issue))),
        uuid::Uuid::new_v4(),
        safe_segment(Some(&json!(env)))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_sorts_observed_and_flattens_newlines() {
        // WHY: verifiers rebuild these bytes; a reordered or multi-line field
        // would break every signature.
        let obj = json!({"bucket": "b", "key": "k", "version_id": "v", "sha256": "s", "content_length": "1",
                         "content_type": "t", "last_modified": "", "etag": "e", "attested_at_utc": "a"});
        let mut observed = Map::new();
        observed.insert("z".into(), json!("1"));
        observed.insert("a".into(), json!("x\ny"));
        let m = canonical_manifest(&obj, &observed);
        assert!(m.starts_with("r66-evidence-attestation-v4\nbucket=b\n"));
        assert!(m.ends_with("attested-at-utc=a\nobserved.a=x y\nobserved.z=1\n"));
    }

    #[test]
    fn safe_segment_collapses_and_trims() {
        assert_eq!(safe_segment(Some(&json!("--a b//c--"))), "a-b-c");
        assert_eq!(safe_segment(Some(&json!("///"))), "evidence");
    }
}
