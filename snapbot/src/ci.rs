//! DEV CI capture: read the env's ci-orchestrator through the narrow
//! cross-account role and report the LATEST execution exactly as found.
//!
//! Never fails on CI state (owner 2026-08-26: "never refuse, only capture!");
//! an unreachable account or malformed history is itself captured as ci.error
//! so the signature covers "could not read CI". No derived verdict field
//! (owner 2026-09-13: "snapbot does not judge, it only snaps.").

use crate::{config, js};
use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

fn execution_input_sha(input: Option<&str>) -> String {
    input
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("sha").map(|s| match s {
            Value::Null => String::new(),
            other => js::js_string(other),
        }))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// {mode, exitCode} from a finalizer's scheduled input, when it carries mode.
fn parse_finalizer_payload(raw: &str) -> Option<(String, f64)> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let payload = if js::truthy(parsed.get("Payload")) { &parsed["Payload"] } else { &parsed };
    let obj = payload.as_object()?;
    if !obj.contains_key("mode") {
        return None;
    }
    let mode = match obj.get("mode") {
        Some(v) if js::truthy(Some(v)) => js::js_string(v),
        _ => String::new(),
    };
    Some((mode, js::js_number(obj.get("exitCode"))))
}

async fn sfn_client(env_name: &str) -> Result<(aws_sdk_sfn::Client, Value)> {
    let cfg = config::cfg()?;
    let env = cfg.dev_ci.get(env_name).cloned().ok_or_else(|| anyhow!("no DEV CI target configured for env {env_name}"))?;
    let account = env["account"].as_str().map(str::to_string).unwrap_or_else(|| js::js_string(&env["account"]));
    let session: String = env_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "+=,.@-".contains(c) { c } else { '-' })
        .collect();
    let assumed = config::sts()
        .await
        .assume_role()
        .role_arn(format!("arn:aws:iam::{account}:role/{}", cfg.ci_read_role_name))
        .role_session_name(format!("r66-evidence-attestor-{session}"))
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_sts::error::DisplayErrorContext(&e)))?;
    let c = assumed.credentials().ok_or_else(|| anyhow!("AssumeRole returned no credentials for {env_name}"))?;
    let creds = aws_credential_types::Credentials::new(
        c.access_key_id(),
        c.secret_access_key(),
        Some(c.session_token().to_string()),
        None,
        "ci-read-role",
    );
    let region = env["region"].as_str().unwrap_or("").to_string();
    let conf = aws_sdk_sfn::config::Builder::from(config::sdk().await)
        .region(aws_sdk_sfn::config::Region::new(region))
        .credentials_provider(creds)
        .build();
    Ok((aws_sdk_sfn::Client::from_conf(conf), env))
}

async fn read_finalizer(client: &aws_sdk_sfn::Client, arn: &str) -> Result<Option<(String, f64)>> {
    let mut token: Option<String> = None;
    loop {
        let resp = client
            .get_execution_history()
            .execution_arn(arn)
            .reverse_order(true)
            .include_execution_data(true)
            .set_next_token(token.clone())
            .send()
            .await
            .map_err(|e| anyhow!("{}", aws_sdk_sfn::error::DisplayErrorContext(&e)))?;
        for ev in resp.events() {
            let raw = match ev.r#type().as_str() {
                "TaskScheduled" => ev.task_scheduled_event_details().map(|d| d.parameters().to_string()),
                "LambdaFunctionScheduled" => ev.lambda_function_scheduled_event_details().and_then(|d| d.input()).map(str::to_string),
                _ => None,
            };
            if let Some(parsed) = raw.filter(|r| !r.is_empty()).and_then(|r| parse_finalizer_payload(&r)) {
                return Ok(Some(parsed));
            }
        }
        match resp.next_token() {
            Some(t) => token = Some(t.to_string()),
            None => return Ok(None),
        }
    }
}

/// The ci.* snapshot for env@target_sha.
pub async fn capture_dev_ci(env_name: &str, target_sha: &str) -> Map<String, Value> {
    let mut ci = Map::new();
    ci.insert("ci.env".into(), json!(env_name));
    ci.insert("ci.target-sha".into(), json!(target_sha));
    ci.insert("ci.checked-at-utc".into(), json!(js::now_iso()));
    if let Err(e) = fill(&mut ci, env_name).await {
        // A failed read is itself the snapshot: ci.error alone, no verdict field.
        ci.insert("ci.error".into(), json!(format!("{e:#}")));
    }
    ci
}

async fn fill(ci: &mut Map<String, Value>, env_name: &str) -> Result<()> {
    let (client, env) = sfn_client(env_name).await?;
    let region = env["region"].as_str().unwrap_or("");
    let account = env["account"].as_str().map(str::to_string).unwrap_or_else(|| js::js_string(&env["account"]));
    let sm = format!("arn:aws:states:{region}:{account}:stateMachine:{env_name}-ci-orchestrator");
    // Newest first: the FIRST execution defines DEV right now, whatever its state.
    let listed = client
        .list_executions()
        .state_machine_arn(&sm)
        .max_results(1)
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_sfn::error::DisplayErrorContext(&e)))?;
    let Some(arn) = listed.executions().first().map(|x| x.execution_arn().to_string()).filter(|a| !a.is_empty()) else {
        ci.insert("ci.error".into(), json!(format!("no execution found for {sm}")));
        return Ok(());
    };
    let d = client
        .describe_execution()
        .execution_arn(&arn)
        .send()
        .await
        .map_err(|e| anyhow!("{}", aws_sdk_sfn::error::DisplayErrorContext(&e)))?;
    ci.insert("ci.execution-arn".into(), json!(arn));
    ci.insert("ci.executed-sha".into(), json!(execution_input_sha(d.input())));
    ci.insert("ci.sfn-status".into(), json!(d.status().as_str()));
    ci.insert("ci.start-date".into(), json!(js::iso_from_smithy(d.start_date())));
    ci.insert("ci.stop-date".into(), json!(d.stop_date().map(js::iso_from_smithy).unwrap_or_default()));
    let finalizer = match read_finalizer(&client, &arn).await {
        Ok(f) => f,
        Err(e) => {
            ci.insert("ci.error".into(), json!(format!("history: {e:#}")));
            None
        }
    };
    ci.insert("ci.finalizer-mode".into(), json!(finalizer.as_ref().map(|f| f.0.clone()).unwrap_or_default()));
    ci.insert(
        "ci.worker-exit-code".into(),
        json!(finalizer.filter(|f| f.1.is_finite()).map(|f| js::num_to_string(f.1)).unwrap_or_default()),
    );
    Ok(())
}
