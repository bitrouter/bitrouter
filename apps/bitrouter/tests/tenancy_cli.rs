//! The tenancy CLI uses explicit HTTP targets and never assembles a local host.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
};
use bitrouter::contexts::{ContextStore, RemoteContext};
use serde_json::{Value, json};
use tokio::{net::TcpListener, process::Command, sync::Mutex, task::JoinHandle};

const TOKEN: &str = "fixture-tenancy-cli-process-0123456789abcdef";
const TOKEN_ENV: &str = "FIXTURE_TENANCY_CLI_TOKEN";

async fn invoke(home: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bro"));
    command
        .current_dir(home)
        .env("BITROUTER_HOME", home)
        .env(TOKEN_ENV, TOKEN)
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .env("RUST_LOG", "error")
        .args(args)
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .context("tenancy CLI timed out")?
        .context("running tenancy CLI")
}

fn draft() -> Value {
    json!({"installation_id":"install-A", "operation_id":"draft-1", "expected_authority_revision":17,
        "document":{"routes":[], "policy":null, "generation_defaults":{"max_output_tokens":2048,"temperature":0.2},
        "learning":{"mode":"off","max_evaluations":0}, "limits":{"active_executions":16,"queued_work":32,"learning_jobs":0}}})
}

#[tokio::test]
async fn local_shape_validation_ignores_host_configuration_and_context_store() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    std::fs::write(home.join("bitrouter.yaml"), "invalid: [SECRET_SENTINEL")?;
    std::fs::write(home.join("contexts.toml"), "invalid = [SECRET_SENTINEL")?;
    std::fs::write(home.join("draft.json"), serde_json::to_vec(&draft())?)?;
    for human in [false, true] {
        let mut args = vec![
            "tenancy",
            "configuration",
            "validate",
            "--input",
            "draft.json",
        ];
        if human {
            args.push("--human");
        }
        let result = invoke(home, &args).await?;
        ensure!(
            result.status.success(),
            "validation failed: {}",
            String::from_utf8_lossy(&result.stdout)
        );
        let text = String::from_utf8(result.stdout)?;
        if human {
            ensure!(text.contains("local only") && text.contains("No server authorization"));
        } else {
            let report: Value = serde_json::from_str(&text)?;
            ensure!(
                report["validation"] == "local_shape_only" && report["server_checked"] == false
            );
        }
        ensure!(!text.contains("SECRET_SENTINEL") && !text.contains(TOKEN));
    }
    let mut invalid = draft();
    invalid
        .as_object_mut()
        .context("draft object")?
        .remove("expected_authority_revision");
    std::fs::write(home.join("draft.json"), serde_json::to_vec(&invalid)?)?;
    let result = invoke(
        home,
        &[
            "tenancy",
            "configuration",
            "validate",
            "--input",
            "draft.json",
        ],
    )
    .await?;
    ensure!(!result.status.success());
    let report: Value = serde_json::from_slice(&result.stdout)?;
    ensure!(report["error"]["kind"] == "bad_request");
    let mut files = std::fs::read_dir(home)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.sort();
    let expected = ["bitrouter.yaml", "contexts.toml", "draft.json"].map(std::ffi::OsString::from);
    ensure!(files == expected);
    Ok(())
}

struct ServerTask(JoinHandle<std::io::Result<()>>);

impl Drop for ServerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Default)]
struct Requests {
    entries: Vec<(String, bool, Option<String>)>,
}

async fn reply(
    State(state): State<Arc<Mutex<Requests>>>,
    request: Request,
) -> axum::response::Response {
    let path = request.uri().path().to_owned();
    let authorized = request
        .headers()
        .get("authorization")
        .is_some_and(|value| *value == format!("Bearer {TOKEN}"));
    let installation = request
        .headers()
        .get("Bitrouter-Installation-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state
        .lock()
        .await
        .entries
        .push((path.clone(), authorized, installation));
    let body = match path.as_str() {
        "/control/v1/tenancy/capabilities" => {
            json!({"contract_version":1,"installation_id":"install-A","instance_id":"boot-A","capability_revision":"caps-A", "revision_bound_dispatch":false,"strict_receipts":false,"dispatch_enabled":false})
        }
        "/control/v1/tenancy/operations/draft-1" => {
            json!({"operation_id":"draft-1","status":"unknown", "authority_revision":null,"resource_revision":null,"resource_id":null,"error":null})
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    axum::Json(body).into_response()
}

#[tokio::test]
async fn named_context_preserves_disabled_capabilities_and_unknown_operation_exit_status()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let home = directory.path();
    std::fs::write(home.join("bitrouter.yaml"), "invalid: [SECRET_SENTINEL")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let state = Arc::new(Mutex::new(Requests::default()));
    let router = Router::new().fallback(reply).with_state(state.clone());
    let _server = ServerTask(tokio::spawn(
        async move { axum::serve(listener, router).await },
    ));
    let mut contexts = ContextStore::default();
    contexts.add("fixture", RemoteContext::new(&endpoint, TOKEN_ENV)?)?;
    contexts.save_to(&home.join("contexts.toml"))?;
    // Clap propagates a global option written before a subcommand after that
    // subcommand's local conflict checks. The runtime rejects the ambiguity
    // before token lookup or HTTP, including this argument placement.
    let conflicting = invoke(
        home,
        &[
            "--context",
            "fixture",
            "tenancy",
            "capabilities",
            "--endpoint",
            &endpoint,
            "--token-env",
            TOKEN_ENV,
        ],
    )
    .await?;
    ensure!(!conflicting.status.success());
    let conflict_report: Value = serde_json::from_slice(&conflicting.stdout)?;
    ensure!(conflict_report["error"]["kind"] == "bad_request");
    ensure!(state.lock().await.entries.is_empty());
    let caps = invoke(home, &["--context", "fixture", "tenancy", "capabilities"]).await?;
    ensure!(caps.status.success());
    let report: Value = serde_json::from_slice(&caps.stdout)?;
    ensure!(report["installation_id"] == "install-A" && report["dispatch_enabled"] == false);
    let unknown = invoke(
        home,
        &[
            "--context",
            "fixture",
            "tenancy",
            "operation",
            "--installation-id",
            "install-A",
            "--operation-id",
            "draft-1",
        ],
    )
    .await?;
    ensure!(!unknown.status.success());
    let report: Value = serde_json::from_slice(&unknown.stdout)?;
    ensure!(report["status"] == "unknown" && report["operation_id"] == "draft-1");
    let state = state.lock().await;
    ensure!(
        state.entries.len() == 2,
        "unexpected discovery or retry request"
    );
    ensure!(state.entries[0] == ("/control/v1/tenancy/capabilities".into(), true, None));
    ensure!(
        state.entries[1]
            == (
                "/control/v1/tenancy/operations/draft-1".into(),
                true,
                Some("install-A".into())
            )
    );
    ensure!(!home.join("bitrouter.db").exists());
    Ok(())
}
