//! attest-aws-resource: a caller-supplied READ-ONLY AWS call made by this
//! handler with the caller's TEMPORARY credentials, its result stored as JSON
//! evidence and attested (owner 2026-08-26: "check and attest the existence or
//! particular attribute of an aws resource (via aws sdk readonly)").
//!
//! The Node handler dispatched `new Client(...).send(new <Op>Command(params))`
//! dynamically. The Rust SDK has no dynamic dispatch, so this module speaks
//! the services' wire protocols directly (awsJson1_0/1_1, awsQuery, restXml
//! for the S3 reads) and signs with SigV4 -- the same pinned service set.
//! A failed call is REFUSED (route66#4039): nothing stored, signed or posted.

use crate::attest;
use crate::{ci, js};
use anyhow::{anyhow, bail, Result};
use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SignatureLocation, SigningSettings};
use aws_sigv4::sign::v4;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

/// `Filter` admits FilterLogEvents; `Start` must never be added (StartQuery,
/// DetectStackDrift and friends begin work in the target account).
fn read_only(op: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(Describe|Get|List|Head|Lookup|Query|Scan|BatchGet|Filter)[A-Za-z0-9]*$").unwrap()).is_match(op)
}

enum Proto {
    /// awsJson: (content-type version, X-Amz-Target prefix)
    Json(&'static str, &'static str),
    /// awsQuery API version
    Query(&'static str),
    S3,
}

/// (signing name, endpoint host prefix, protocol) for the pinned service set.
fn service(name: &str) -> Result<(&'static str, &'static str, Proto)> {
    Ok(match name {
        "dynamodb" => ("dynamodb", "dynamodb", Proto::Json("1.0", "DynamoDB_20120810")),
        "sfn" => ("states", "states", Proto::Json("1.0", "AWSStepFunctions")),
        "ssm" => ("ssm", "ssm", Proto::Json("1.1", "AmazonSSM")),
        "cloudwatch-logs" => ("logs", "logs", Proto::Json("1.1", "Logs_20140328")),
        "sts" => ("sts", "sts", Proto::Query("2011-06-15")),
        "cloudformation" => ("cloudformation", "cloudformation", Proto::Query("2010-05-15")),
        "cloudwatch" => ("monitoring", "monitoring", Proto::Query("2010-08-01")),
        "s3" => ("s3", "s3", Proto::S3),
        other => bail!("service {other} is not in the attestor's pinned AWS SDK client set"),
    })
}

pub struct Creds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
}

struct Reply {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(60)).build().expect("http client"))
}

async fn signed(method: &str, url: &str, headers: Vec<(String, String)>, body: Vec<u8>, signing: &str, region: &str, c: &Creds) -> Result<Reply> {
    let identity = aws_credential_types::Credentials::new(
        c.access_key_id.clone(),
        c.secret_access_key.clone(),
        Some(c.session_token.clone()),
        None,
        "attest-aws-resource",
    )
    .into();
    let mut settings = SigningSettings::default();
    settings.signature_location = SignatureLocation::Headers;
    if signing == "s3" {
        settings.payload_checksum_kind = aws_sigv4::http_request::PayloadChecksumKind::XAmzSha256;
    }
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(signing)
        .time(SystemTime::now())
        .settings(settings)
        .build()
        .map_err(|e| anyhow!("sigv4 params: {e}"))?
        .into();
    let signable = SignableRequest::new(method, url, headers.iter().map(|(k, v)| (k.as_str(), v.as_str())), SignableBody::Bytes(&body))
        .map_err(|e| anyhow!("sigv4 request: {e}"))?;
    let (instructions, _) = sign(signable, &params).map_err(|e| anyhow!("sigv4 sign: {e}"))?.into_parts();
    let m = reqwest::Method::from_bytes(method.as_bytes())?;
    let mut req = http().request(m, url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    for (k, v) in instructions.headers() {
        req = req.header(k, v);
    }
    let res = req.body(body).send().await?;
    Ok(Reply { status: res.status().as_u16(), headers: res.headers().clone(), body: res.bytes().await?.to_vec() })
}

/// Flatten JSON params into awsQuery form fields (lists as member.N).
fn flatten_query(prefix: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(o) => {
            for (k, x) in o {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten_query(&p, x, out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                flatten_query(&format!("{prefix}.member.{}", i + 1), x, out);
            }
        }
        Value::Null => {}
        other => out.push((prefix.to_string(), js::js_string(other))),
    }
}

/// Generic XML -> JSON: repeated or `member` children become arrays, leaves strings.
fn xml_to_json(xml: &[u8]) -> Result<Value> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    // Stack of (name, children, text).
    let mut stack: Vec<(String, Vec<(String, Value)>, String)> = vec![("#root".into(), Vec::new(), String::new())];
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => stack.push((String::from_utf8_lossy(e.local_name().as_ref()).into_owned(), Vec::new(), String::new())),
            Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if let Some(top) = stack.last_mut() {
                    top.1.push((name, json!("")));
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.2.push_str(&t.unescape()?);
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.2.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::End(_) => {
                let (name, children, text) = stack.pop().ok_or_else(|| anyhow!("unbalanced XML"))?;
                let v = fold(children, text);
                if let Some(top) = stack.last_mut() {
                    top.1.push((name, v));
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let (_, children, _) = stack.pop().ok_or_else(|| anyhow!("empty XML"))?;
    Ok(fold(children, String::new()))
}

fn fold(children: Vec<(String, Value)>, text: String) -> Value {
    if children.is_empty() {
        return json!(text);
    }
    if children.iter().all(|(n, _)| n == "member") {
        return Value::Array(children.into_iter().map(|(_, v)| v).collect());
    }
    let mut out = Map::new();
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (n, _) in &children {
        *counts.entry(n.clone()).or_default() += 1;
    }
    for (n, v) in children {
        if counts[&n] > 1 {
            match out.entry(n).or_insert_with(|| json!([])) {
                Value::Array(a) => a.push(v),
                _ => {}
            }
        } else {
            out.insert(n, v);
        }
    }
    Value::Object(out)
}

/// Timestamps arrive as epoch seconds on the JSON protocols; the JS SDK
/// rendered them as Dates. Members named *Date / *DateTime get the same form.
fn js_dates(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for (k, x) in o.iter_mut() {
                let is_date = k.ends_with("Date") || k.ends_with("DateTime") || k.ends_with("date");
                if is_date {
                    if let Some(f) = x.as_f64() {
                        *x = json!(js::iso_from_millis((f * 1000.0).round() as i64));
                        continue;
                    }
                }
                js_dates(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(js_dates),
        _ => {}
    }
}

fn error_of(proto_json: bool, r: &Reply) -> (String, String) {
    if proto_json {
        let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        let ty = v["__type"].as_str().unwrap_or("").rsplit('#').next().unwrap_or("").to_string();
        let msg = v["message"].as_str().or_else(|| v["Message"].as_str()).unwrap_or("").to_string();
        return (if ty.is_empty() { "UnknownError".into() } else { ty }, msg);
    }
    let v = xml_to_json(&r.body).unwrap_or(Value::Null);
    let err = v.pointer("/ErrorResponse/Error").or_else(|| v.get("Error")).cloned().unwrap_or(Value::Null);
    let code = err["Code"].as_str().map(str::to_string).unwrap_or_else(|| match r.status {
        404 => "NotFound".into(),
        403 => "Forbidden".into(),
        _ => "UnknownError".into(),
    });
    (code, err["Message"].as_str().unwrap_or("").to_string())
}

/// One read-only call. Ok(result JSON with $metadata first) or Err((name, message, status)).
async fn call(svc: &str, op: &str, region: &str, params: &Value, c: &Creds) -> Result<std::result::Result<Value, (String, String, u16)>> {
    let (signing, host, proto) = service(svc)?;
    let host = format!("{host}.{region}.amazonaws.com");
    let (reply, is_json, parsed) = match proto {
        Proto::Json(ver, target) => {
            // A non-object params sends {}; the binding outlives the borrow.
            let empty = json!({});
            let body = serde_json::to_vec(if params.is_object() { params } else { &empty })?;
            let r = signed(
                "POST",
                &format!("https://{host}/"),
                vec![("content-type".into(), format!("application/x-amz-json-{ver}")), ("x-amz-target".into(), format!("{target}.{op}"))],
                body,
                signing,
                region,
                c,
            )
            .await?;
            let mut v = if r.body.is_empty() { json!({}) } else { serde_json::from_slice(&r.body).unwrap_or(json!({})) };
            js_dates(&mut v);
            (r, true, v)
        }
        Proto::Query(version) => {
            let mut fields = vec![("Action".to_string(), op.to_string()), ("Version".to_string(), version.to_string())];
            flatten_query("", params, &mut fields);
            let body = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
            let r = signed(
                "POST",
                &format!("https://{host}/"),
                vec![("content-type".into(), "application/x-www-form-urlencoded; charset=utf-8".into())],
                body.into_bytes(),
                signing,
                region,
                c,
            )
            .await?;
            let v = xml_to_json(&r.body).unwrap_or(json!({}));
            let result = v.get(format!("{op}Response")).and_then(|x| x.get(format!("{op}Result"))).cloned().unwrap_or(json!({}));
            (r, false, if result.is_object() { result } else { json!({}) })
        }
        Proto::S3 => {
            let (method, url) = s3_request(op, region, params)?;
            let r = signed(&method, &url, Vec::new(), Vec::new(), signing, region, c).await?;
            let v = s3_result(op, &r);
            (r, false, v)
        }
    };
    if !(200..300).contains(&reply.status) {
        let (name, msg) = error_of(is_json, &reply);
        return Ok(Err((name, msg, reply.status)));
    }
    let mut out = Map::new();
    out.insert("$metadata".into(), json!({"httpStatusCode": reply.status}));
    if let Some(o) = parsed.as_object() {
        for (k, v) in o {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(Ok(Value::Object(out)))
}

/// The S3 reads: object and bucket GET/HEAD subresources.
fn s3_request(op: &str, region: &str, p: &Value) -> Result<(String, String)> {
    let bucket = p["Bucket"].as_str().unwrap_or("");
    let key = p["Key"].as_str().unwrap_or("");
    let path_key: String = key.split('/').map(js::encode_uri_component).collect::<Vec<_>>().join("/");
    let base = if bucket.is_empty() { format!("https://s3.{region}.amazonaws.com") } else { format!("https://{bucket}.s3.{region}.amazonaws.com") };
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    let mut add = |k: &str, field: &str| {
        if let Some(v) = p.get(field).filter(|v| !v.is_null()) {
            q.append_pair(k, &js::js_string(v));
        }
    };
    let (method, path, sub) = match op {
        "HeadObject" => {
            add("versionId", "VersionId");
            ("HEAD", format!("/{path_key}"), "")
        }
        "GetObject" => {
            add("versionId", "VersionId");
            ("GET", format!("/{path_key}"), "")
        }
        "GetObjectTagging" => {
            add("versionId", "VersionId");
            ("GET", format!("/{path_key}"), "tagging")
        }
        "GetObjectAcl" => ("GET", format!("/{path_key}"), "acl"),
        "HeadBucket" => ("HEAD", "/".to_string(), ""),
        "ListBuckets" => ("GET", "/".to_string(), ""),
        "ListObjectsV2" => {
            q.append_pair("list-type", "2");
            for (k, f) in [("prefix", "Prefix"), ("delimiter", "Delimiter"), ("max-keys", "MaxKeys"), ("continuation-token", "ContinuationToken"), ("start-after", "StartAfter")] {
                if let Some(v) = p.get(f).filter(|v| !v.is_null()) {
                    q.append_pair(k, &js::js_string(v));
                }
            }
            ("GET", "/".to_string(), "")
        }
        "ListObjectVersions" => {
            for (k, f) in [("prefix", "Prefix"), ("delimiter", "Delimiter"), ("max-keys", "MaxKeys"), ("key-marker", "KeyMarker"), ("version-id-marker", "VersionIdMarker")] {
                if let Some(v) = p.get(f).filter(|v| !v.is_null()) {
                    q.append_pair(k, &js::js_string(v));
                }
            }
            ("GET", "/".to_string(), "versions")
        }
        other => {
            let sub = match other {
                "GetBucketVersioning" => "versioning",
                "GetBucketPolicy" => "policy",
                "GetBucketPolicyStatus" => "policyStatus",
                "GetBucketLifecycleConfiguration" => "lifecycle",
                "GetBucketTagging" => "tagging",
                "GetBucketEncryption" => "encryption",
                "GetBucketCors" => "cors",
                "GetBucketLocation" => "location",
                "GetBucketAcl" => "acl",
                "GetBucketNotificationConfiguration" => "notification",
                "GetPublicAccessBlock" => "publicAccessBlock",
                "GetBucketOwnershipControls" => "ownershipControls",
                "GetBucketLogging" => "logging",
                "GetBucketWebsite" => "website",
                _ => bail!("service s3 has no operation {other}"),
            };
            ("GET", "/".to_string(), sub)
        }
    };
    let mut query = q.finish();
    if !sub.is_empty() {
        query = if query.is_empty() { sub.to_string() } else { format!("{sub}&{query}") };
    }
    Ok((method.to_string(), if query.is_empty() { format!("{base}{path}") } else { format!("{base}{path}?{query}") }))
}

fn s3_result(op: &str, r: &Reply) -> Value {
    let header = |n: &str| r.headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
    let mut out = Map::new();
    if matches!(op, "HeadObject" | "GetObject") {
        for (field, h) in [
            ("ContentLength", "content-length"),
            ("ContentType", "content-type"),
            ("ETag", "etag"),
            ("LastModified", "last-modified"),
            ("VersionId", "x-amz-version-id"),
            ("ServerSideEncryption", "x-amz-server-side-encryption"),
            ("CacheControl", "cache-control"),
            ("ContentEncoding", "content-encoding"),
        ] {
            if let Some(v) = header(h) {
                let v = match field {
                    "ContentLength" => v.parse::<i64>().map(|n| json!(n)).unwrap_or(json!(v)),
                    "LastModified" => chrono::DateTime::parse_from_rfc2822(&v)
                        .map(|d| json!(js::iso_from_millis(d.timestamp_millis())))
                        .unwrap_or(json!(v)),
                    _ => json!(v),
                };
                out.insert(field.to_string(), v);
            }
        }
        let meta: Map<String, Value> = r
            .headers
            .iter()
            .filter_map(|(k, v)| k.as_str().strip_prefix("x-amz-meta-").map(|m| (m.to_string(), json!(v.to_str().unwrap_or("")))))
            .collect();
        out.insert("Metadata".into(), Value::Object(meta));
        if op == "GetObject" {
            let cap = 1 << 20;
            out.insert(
                "Body".into(),
                json!({"$stream": true, "bytes": r.body.len(), "truncated": r.body.len() > cap, "body_b64": attest::b64(&r.body[..r.body.len().min(cap)])}),
            );
        }
        return Value::Object(out);
    }
    if op == "GetBucketPolicy" {
        return json!({"Policy": String::from_utf8_lossy(&r.body)});
    }
    if op == "HeadBucket" {
        return json!({});
    }
    let v = xml_to_json(&r.body).unwrap_or(json!({}));
    // The root element's children are the output members.
    v.as_object().and_then(|o| o.values().next().cloned()).filter(Value::is_object).unwrap_or(json!({}))
}

pub async fn attest_aws_resource(event: &Value) -> Result<Value> {
    let issue = js::metadata_value(js::coalesce(&[event.get("issue"), event.get("issue_number")]).or(Some(&json!("unknown"))), 64);
    let env_name = js::metadata_value(js::coalesce(&[event.get("env"), event.get("environment")]), 64);
    let target_sha = js::metadata_value(js::coalesce(&[event.get("target_sha"), event.get("targetSHA")]), 80);
    let region = js::metadata_value(event.get("region"), 32);
    let svc = js::metadata_value(event.get("service"), 64);
    let op = js::metadata_value(event.get("operation"), 96);
    let params = event.get("params").filter(|v| v.is_object()).cloned().unwrap_or(json!({}));
    let c = event.get("credentials").cloned().unwrap_or(json!({}));
    if env_name.is_empty() {
        bail!("attest-aws-resource requires env");
    }
    if region.is_empty() {
        bail!("attest-aws-resource requires region");
    }
    let field = |k: &str| c.get(k).filter(|v| js::truthy(Some(v))).map(js::js_string);
    let (Some(ak), Some(sk), Some(tok)) = (field("accessKeyId"), field("secretAccessKey"), field("sessionToken")) else {
        bail!("attest-aws-resource requires TEMPORARY credentials (accessKeyId, secretAccessKey, sessionToken)");
    };
    let creds = Creds { access_key_id: ak, secret_access_key: sk, session_token: tok };
    static SVC: OnceLock<Regex> = OnceLock::new();
    if !SVC.get_or_init(|| Regex::new(r"^[a-z0-9-]+$").unwrap()).is_match(&svc) {
        bail!("invalid service name {svc}");
    }
    service(&svc)?;
    if !read_only(&op) {
        bail!("operation {op} is not an allow-listed read-only operation");
    }
    let called_at = js::now_iso();

    // Who is asking, proven with the very credentials the call uses.
    let ident = match call("sts", "GetCallerIdentity", &region, &json!({}), &creds).await? {
        Ok(v) => v,
        Err((n, m, s)) => bail!("{n}: {m} (HTTP {s})"),
    };
    let caller = json!({
        "arn": ident["Arn"].as_str().unwrap_or(""),
        "account": ident["Account"].as_str().unwrap_or(""),
        "user_id": ident["UserId"].as_str().unwrap_or(""),
    });
    let result = match call(&svc, &op, &region, &params, &creds).await? {
        Ok(v) => v,
        Err((name, msg, status)) => {
            bail!("FAILURE TO ATTEST: {svc} {op} failed: {name}: {msg} (HTTP {status})");
        }
    };
    let http_status = result["$metadata"]["httpStatusCode"].as_u64().map(|n| n.to_string()).unwrap_or_default();
    let doc = json!({
        "v": 1, "service": svc, "operation": op, "region": region, "params": params,
        "caller": caller, "called_at_utc": called_at, "result": result, "error": null,
    });
    let key = attest::evidence_key(
        &env_name,
        &issue,
        &js::compact_stamp(&called_at),
        &format!("aws-{}-{}.json", attest::safe_segment(Some(&json!(svc))), attest::safe_segment(Some(&json!(op)))),
    );
    let metadata = [
        ("environment", env_name.clone()),
        ("issue-number", issue.clone()),
        ("evidence-type", "aws-resource-dump".to_string()),
        ("captured-by", "command-center evidence-attestor".to_string()),
        ("aws-service", svc.clone()),
        ("aws-operation", op.clone()),
    ];
    let vid = attest::put_evidence(&key, serde_json::to_vec_pretty(&doc)?, "application/json", &metadata).await?;
    let mut o = Map::new();
    for (k, v) in [
        ("aws.evidence-type", "aws-resource-dump".to_string()),
        ("aws.issue", issue.clone()),
        ("aws.service", svc.clone()),
        ("aws.operation", op.clone()),
        ("aws.region", region.clone()),
        ("aws.params-sha256", attest::sha256_hex(serde_json::to_string(&params)?.as_bytes())),
        ("aws.caller-arn", caller["arn"].as_str().unwrap_or("").to_string()),
        ("aws.caller-account", caller["account"].as_str().unwrap_or("").to_string()),
        ("aws.called-at-utc", called_at.clone()),
        ("aws.http-status", http_status),
        ("aws.error", String::new()),
    ] {
        o.insert(k.to_string(), json!(v));
    }
    if !target_sha.is_empty() {
        for (k, v) in ci::capture_dev_ci(&env_name, &target_sha).await {
            o.insert(k, v);
        }
    }
    crate::handler::add_attestation_context(&mut o, event);
    attest::attest(&key, &vid, o).await
}
