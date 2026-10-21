use super::*;
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
};
use std::{collections::VecDeque, sync::Mutex, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;

struct Script {
    replies: Mutex<VecDeque<serde_json::Value>>,
    requests: Mutex<Vec<(axum::http::Method, String, serde_json::Value)>>,
}

struct Server {
    endpoint: String,
    script: Arc<Script>,
    stop: CancellationToken,
    task: Option<JoinHandle<std::io::Result<()>>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Server {
    async fn new(replies: Vec<serde_json::Value>) -> Result<Self> {
        let script = Arc::new(Script {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        });
        let router = Router::new().fallback(reply).with_state(script.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(cancelled.cancelled_owned())
                .await
        });
        Ok(Self {
            endpoint,
            script,
            stop,
            task: Some(task),
        })
    }

    fn count(&self) -> usize {
        self.script
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    async fn finish(mut self, result: Result<()>) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.as_mut() {
            // Borrow the retained task so a timeout cannot detach it.
            // https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html#cancel-safety
            match tokio::time::timeout(Duration::from_secs(3), &mut *task).await {
                Ok(joined) => {
                    self.task = None;
                    joined??;
                }
                Err(error) => {
                    task.abort();
                    let joined = (&mut *task).await;
                    self.task = None;
                    if let Err(joined) = joined
                        && !joined.is_cancelled()
                    {
                        return Err(joined.into());
                    }
                    return Err(error.into());
                }
            }
        }
        result
    }
}

async fn reply(State(script): State<Arc<Script>>, request: Request) -> Json<serde_json::Value> {
    let method = request.method().clone();
    let uri = request.uri().to_string();
    let authorized = request
        .headers()
        .get("authorization")
        .is_some_and(|value| *value == format!("Bearer {TOKEN}"));
    let bytes = to_bytes(request.into_body(), 2 * 1024 * 1024).await;
    let body = bytes
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(serde_json::Value::Null);
    script
        .requests
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push((method, uri, body));
    if !authorized {
        return Json(json!({"bad_auth":true}));
    }
    Json(
        script
            .replies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop_front()
            .unwrap_or(json!({"unexpected_request":true})),
    )
}

fn receipt() -> serde_json::Value {
    json!({"namespace":"fixture", "operator_id":"operator", "operation_id":"..", "committed_at_ms":42,
        "outcome":{"kind":"credential", "value":{"provider":"provider","account":"default","resource_pool_id":"pool","version":1}}})
}

#[tokio::test]
async fn parsed_recovery_commands_keep_leading_hyphen_query_handles() -> Result<()> {
    use clap::Parser as _;
    let ids = ["..", "-job", "--help"];
    let server = Server::new(
        ids.iter()
            .map(|id| json!({"status":"unresolved","operation_id":id}))
            .collect(),
    )
    .await?;
    let result = async {
        for id in ids {
            let argument = format!("--operation-id={id}");
            let cli = crate::Cli::try_parse_from([
                "bro",
                "shared",
                "operation",
                "--endpoint",
                &server.endpoint,
                "--token-env",
                "FIXTURE_OPERATOR",
                &argument,
            ])?;
            let Some(crate::Command::Shared {
                action: crate::SharedAction::Operator(action),
            }) = cli.command
            else {
                anyhow::bail!("operator action missing");
            };
            let report = execute(&action, |_| Some(TOKEN.into())).await?;
            ensure!(serde_json::to_value(&report)?["operation_id"] == id);
        }
        let requests = server
            .script
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure!(requests.len() == ids.len());
        for (request, id) in requests.iter().zip(ids) {
            ensure!(request.0 == axum::http::Method::GET);
            let url = url::Url::parse(&format!("http://localhost{}", request.1))?;
            ensure!(url.query_pairs().collect::<Vec<_>>() == [("operation_id".into(), id.into())]);
        }
        Ok(())
    }
    .await;
    server.finish(result).await
}

#[tokio::test]
async fn apply_is_once_and_recovery_is_an_explicit_separate_command() -> Result<()> {
    let server = Server::new(vec![
        receipt(),
        json!({"malformed":"PRIVATE_RESPONSE"}),
        json!({"status":"committed","receipt":receipt()}),
        json!({"status":"unresolved","operation_id":".."}),
    ])
    .await?;
    let result = async {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("input.json");
        tokio::fs::write(&path, input()?).await?;
        let action = Action::Apply {target:target(&server.endpoint), input:path.clone()};
        let report = execute(&action, |_| Some(TOKEN.into())).await?;
        ensure!(server.count() == 1);
        ensure!(serde_json::to_value(&report)? == receipt());
        let error = execute(&action, |_| Some(TOKEN.into())).await.err().context("accepted malformed receipt")?;
        ensure!(server.count() == 2);
        ensure!(matches!(error.downcast_ref::<bitrouter_sdk::error::Error>(), Some(bitrouter_sdk::error::Error::OperatorUnknown {operation_id,..}) if operation_id == ".."));
        let action = Action::Operation {target:target(&server.endpoint), operation_id:"..".into()};
        let report = execute(&action, |_| Some(TOKEN.into())).await?;
        ensure!(server.count() == 3);
        ensure!(serde_json::to_value(&report)?["receipt"] == receipt());
        let report = execute(&action, |_| Some(TOKEN.into())).await?;
        ensure!(server.count() == 4);
        ensure!(serde_json::to_value(&report)?["status"] == "unresolved");
        let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&report))?;
        ensure!(human.contains("does not prove rollback"));
        for format in [Format::Json, Format::Human] {
            let text = String::from_utf8(Output::new(format).render_to_vec(&envelope_from_anyhow(&error)))?;
            ensure!(text.contains("outcome is unknown") && !text.contains("PRIVATE_RESPONSE") && !text.contains("LITERAL_SECRET") && !text.contains(TOKEN));
        }
        {
        let requests = server.script.requests.lock().unwrap_or_else(|error| error.into_inner());
        ensure!(requests[0].2["command"]["input"]["credential"]["value"] == "${LITERAL_SECRET}");
        ensure!(requests[0].1 == "/operator/v1/mutations" && requests[1].1 == "/operator/v1/mutations");
        ensure!(requests[..2].iter().all(|request| request.0 == axum::http::Method::POST));
        for request in &requests[2..] {
            ensure!(request.0 == axum::http::Method::GET);
            let url = url::Url::parse(&format!("http://localhost{}", request.1))?;
            ensure!(url.path() == "/operator/v1/operations" && url.query_pairs().collect::<Vec<_>>() == [("operation_id".into(),"..".into())]);
        }
        }
        let mut invalid_id: serde_json::Value = serde_json::from_slice(&input()?)?;
        invalid_id["operation_id"] = json!("invalid handle");
        for invalid in [b"{}".to_vec(), vec![b' '; 2 * 1024 * 1024 + 1], serde_json::to_vec(&invalid_id)?] {
            tokio::fs::write(&path, invalid).await?;
            let action = Action::Apply { target:target(&server.endpoint), input:path.clone() };
            ensure!(execute(&action, |_| Some(TOKEN.into())).await.is_err());
            ensure!(server.count() == 4);
        }
        let action = Action::Apply { target:target(&server.endpoint), input: directory.path().join("absent") };
        ensure!(execute(&action, |_| Some(TOKEN.into())).await.is_err());
        ensure!(server.count() == 4);
        Ok(())
    }.await;
    server.finish(result).await
}

#[tokio::test]
async fn every_metadata_action_preserves_typed_output_and_query_identity() -> Result<()> {
    const ID: &str = "pool/東京%&+?";
    let replies = vec![
        json!({"config_generation":1,"key_generation":2}),
        json!({"tenant_id":ID,"revision":1,"limits":{"concurrent_executions":2,"requests_per_minute":3,"resource_pools":[]}}),
        json!({"resource_pool_id":ID,"revision":1,"concurrent_attempts":2}),
        json!({"principal_id":ID,"tenant_id":"tenant","authorization_revision":"rev","active":true,"expires_at_ms":null,"requests_per_minute":null,"policy_id":null}),
        json!({"provider":ID,"account":"ac &/é","resource_pool_id":"pool","version":1}),
        json!({"execution_id":ID,"correlation_id":"correlation","revision":1,"operation":"generation","config_generation":1,"accepted_at_ms":1,"updated_at_ms":1,"state":{"status":"accepted"},"attempts":[]}),
    ];
    let server = Server::new(replies.clone()).await?;
    let result = async {
        let actions = [
            Action::Revision {
                target: target(&server.endpoint),
            },
            Action::Tenant {
                target: target(&server.endpoint),
                id: ID.into(),
            },
            Action::Resource {
                target: target(&server.endpoint),
                id: ID.into(),
            },
            Action::Principal {
                target: target(&server.endpoint),
                id: ID.into(),
            },
            Action::Credential {
                target: target(&server.endpoint),
                provider: ID.into(),
                account: "ac &/é".into(),
            },
            Action::Execution {
                target: target(&server.endpoint),
                id: ID.into(),
            },
        ];
        for (index, action) in actions.iter().enumerate() {
            let report = execute(action, |_| Some(TOKEN.into())).await?;
            ensure!(serde_json::to_value(&report)? == replies[index]);
            let output = Output::new(Format::Json).render_to_vec(&report);
            let values = serde_json::Deserializer::from_slice(&output)
                .into_iter::<serde_json::Value>()
                .collect::<std::result::Result<Vec<_>, _>>()?;
            ensure!(values.len() == 1 && values[0] == replies[index]);
            ensure!(!Output::new(Format::Human).render_to_vec(&report).is_empty());
        }
        ensure!(server.count() == 6);
        let requests = server
            .script
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (index, request) in requests.iter().enumerate().skip(1) {
            ensure!(request.0 == axum::http::Method::GET);
            let url = url::Url::parse(&format!("http://localhost{}", request.1))?;
            let query = url
                .query_pairs()
                .collect::<std::collections::BTreeMap<_, _>>();
            ensure!(
                query
                    .get(if index == 4 { "provider" } else { "id" })
                    .map(|value| value.as_ref())
                    == Some(ID)
            );
        }
        Ok(())
    }
    .await;
    server.finish(result).await
}
