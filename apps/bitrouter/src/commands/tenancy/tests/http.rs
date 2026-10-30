use super::*;
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::IntoResponse,
};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

struct Recorded {
    method: Method,
    path: String,
    installation: Option<String>,
    authenticated: bool,
    body: Value,
}

#[derive(Default)]
struct Script {
    replies: VecDeque<(StatusCode, Value)>,
    requests: Vec<Recorded>,
}

struct Server {
    endpoint: String,
    state: Arc<Mutex<Script>>,
    stop: CancellationToken,
    task: Option<JoinHandle<std::io::Result<()>>>,
}

impl Server {
    async fn new(replies: Vec<(StatusCode, Value)>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let state = Arc::new(Mutex::new(Script {
            replies: replies.into(),
            ..Script::default()
        }));
        let router = Router::new().fallback(reply).with_state(state.clone());
        let stop = CancellationToken::new();
        let cancelled = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(cancelled.cancelled_owned())
                .await
        });
        Ok(Self {
            endpoint,
            state,
            stop,
            task: Some(task),
        })
    }

    async fn finish(mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = &mut self.task {
            // Borrow the handle so timeout still leaves Drop able to abort it.
            // https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html#cancel-safety
            tokio::time::timeout(Duration::from_secs(3), task).await???;
        }
        self.task = None;
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn reply(
    State(state): State<Arc<Mutex<Script>>>,
    request: Request,
) -> axum::response::Response {
    let method = request.method().clone();
    let path = request.uri().to_string();
    let authenticated = request
        .headers()
        .get("authorization")
        .is_some_and(|value| *value == format!("Bearer {TOKEN}"));
    let installation = request
        .headers()
        .get("Bitrouter-Installation-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = match to_bytes(request.into_body(), INPUT_LIMIT as usize).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(body) => body,
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
    };
    let mut script = state.lock().await;
    script.requests.push(Recorded {
        method,
        path,
        installation,
        authenticated,
        body,
    });
    let (status, body) = match script.replies.pop_front() {
        Some(reply) => reply,
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"unexpected_request":true}),
        ),
    };
    (status, axum::Json(body)).into_response()
}

fn configuration() -> Value {
    json!({"tenant_id":"Tenant-A", "revision_id":"configuration-1", "schema_version":1,
        "compiler_version":"fixture-1", "content_digest":"a".repeat(64), "document":draft()["document"]})
}

fn applied(operation: &str) -> Value {
    json!({"operation_id":operation, "status":"applied", "authority_revision":19,
        "resource_revision":"configuration-1", "resource_id":"configuration-1", "error":null})
}

#[tokio::test]
async fn parsed_commands_preserve_body_cas_and_get_installation_without_hidden_requests()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let input = write_json(directory.path(), "draft.json", &draft())?;
    let activate = write_json(directory.path(), "activation.json", &activation())?;
    let caps = json!({"contract_version":1, "installation_id":"install-A", "instance_id":"boot-1", "capability_revision":"caps-1", "revision_bound_dispatch":false, "strict_receipts":false, "dispatch_enabled":false});
    let replies = vec![
        caps.clone(),
        configuration(),
        configuration(),
        applied("activate-1"),
        json!({"tenant_id":"Tenant-A", "revision_id":"configuration-1", "authority_revision":19}),
        applied("prepare-1"),
    ];
    let server = Server::new(
        replies
            .into_iter()
            .map(|body| (StatusCode::OK, body))
            .collect(),
    )
    .await?;
    let input = input.to_str().context("input path")?;
    let activate = activate.to_str().context("activation path")?;
    let leaves = [
        vec!["capabilities"],
        vec![
            "configuration",
            "apply",
            "--tenant",
            "Tenant-A",
            "--input",
            input,
        ],
        vec![
            "configuration",
            "read",
            "--tenant",
            "Tenant-A",
            "--installation-id",
            "install-A",
            "--revision",
            "configuration-1",
        ],
        vec![
            "configuration",
            "activate",
            "--tenant",
            "Tenant-A",
            "--input",
            activate,
        ],
        vec![
            "configuration",
            "active",
            "--tenant",
            "Tenant-A",
            "--installation-id",
            "install-A",
        ],
        vec![
            "operation",
            "--installation-id",
            "install-A",
            "--operation-id",
            "prepare-1",
        ],
    ];
    for (index, leaf) in leaves.into_iter().enumerate() {
        let mut args = vec!["bro", "tenancy"];
        args.extend(leaf);
        args.extend([
            "--endpoint",
            &server.endpoint,
            "--token-env",
            "FIXTURE_TENANCY_TOKEN",
        ]);
        let (action, context) = parsed(&args)?;
        let looked_up = std::cell::RefCell::new(Vec::new());
        let completion = execute(&action, context.as_deref(), |name| {
            looked_up.borrow_mut().push(name.to_owned());
            Some(TOKEN.into())
        })
        .await?;
        ensure!(*looked_up.borrow() == ["FIXTURE_TENANCY_TOKEN"]);
        ensure!(completion.report.exit_code() == 0);
        if index == 0 {
            ensure!(serde_json::to_value(&completion.report)? == caps);
            let human =
                String::from_utf8(Output::new(Format::Human).render_to_vec(&completion.report))?;
            ensure!(human.contains("dispatch is unavailable"));
        }
        ensure!(completion.recovery.is_some() == matches!(index, 1 | 3));
    }
    {
        let script = server.state.lock().await;
        ensure!(script.requests.len() == 6 && script.replies.is_empty());
        let expected = [
            (Method::GET, "/control/v1/tenancy/capabilities"),
            (Method::POST, "/control/v1/tenants/Tenant-A/configurations"),
            (
                Method::GET,
                "/control/v1/tenants/Tenant-A/configurations/configuration-1",
            ),
            (Method::POST, "/control/v1/tenants/Tenant-A/activation"),
            (Method::GET, "/control/v1/tenants/Tenant-A/activation"),
            (Method::GET, "/control/v1/tenancy/operations/prepare-1"),
        ];
        for (index, (request, (method, path))) in script.requests.iter().zip(expected).enumerate() {
            ensure!(request.authenticated && request.method == method && request.path == path);
            ensure!(
                request.installation.as_deref()
                    == if index == 0 { None } else { Some("install-A") }
            );
        }
        ensure!(script.requests[1].body == draft());
        ensure!(script.requests[3].body == activation());
    }
    server.finish().await
}

#[tokio::test]
async fn unknown_mutation_retains_recovery_identity_and_only_explicit_lookup_follows() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let input = write_json(directory.path(), "draft.json", &draft())?;
    let server = Server::new(vec![
        (StatusCode::OK, json!({"untrusted":"SECRET_SENTINEL"})),
        (StatusCode::OK, applied("prepare-1")),
    ])
    .await?;
    let action = Action::Configuration {
        action: ConfigurationAction::Apply {
            target: target(&server.endpoint),
            tenant: "Tenant-A".try_into()?,
            input,
        },
    };
    let error = execute(&action, None, |_| Some(TOKEN.into()))
        .await
        .err()
        .context("accepted malformed success")?;
    for format in [Format::Json, Format::Human] {
        let text =
            String::from_utf8(Output::new(format).render_to_vec(&envelope_from_anyhow(&error)))?;
        for expected in [
            "outcome is unknown",
            "--installation-id install-A",
            "--operation-id prepare-1",
            "do not retry with a new operation ID",
        ] {
            ensure!(text.contains(expected));
        }
        ensure!(!text.contains("SECRET_SENTINEL") && !text.contains(TOKEN));
    }
    ensure!(server.state.lock().await.requests.len() == 1);
    let lookup = Action::Operation {
        target: target(&server.endpoint),
        installation_id: "install-A".try_into()?,
        operation_id: "prepare-1".try_into()?,
    };
    let result = execute(&lookup, None, |_| Some(TOKEN.into())).await?;
    ensure!(result.report.exit_code() == 0);
    {
        let script = server.state.lock().await;
        ensure!(script.requests.len() == 2);
        ensure!(
            script.requests[1].method == Method::GET
                && script.requests[1].path == "/control/v1/tenancy/operations/prepare-1"
        );
    }
    server.finish().await
}

#[tokio::test]
async fn activation_conflict_is_not_rebased_or_retried_and_read_identity_is_checked() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let mut rollback = activation();
    rollback["expected_active_revision"] = json!("previous-observation");
    let input = write_json(directory.path(), "activation.json", &rollback)?;
    let mut wrong = configuration();
    wrong["tenant_id"] = json!("Tenant-B");
    let server = Server::new(vec![
        (StatusCode::CONFLICT, json!({"code":"revision_conflict", "message":"SECRET_SENTINEL", "dispatch_state":"not_started", "operation_id":"activate-1", "execution_id":null})),
        (StatusCode::OK, wrong),
    ]).await?;
    let action = Action::Configuration {
        action: ConfigurationAction::Activate {
            target: target(&server.endpoint),
            tenant: "Tenant-A".try_into()?,
            input,
        },
    };
    let error = execute(&action, None, |_| Some(TOKEN.into()))
        .await
        .err()
        .context("accepted stale activation")?;
    let text = format!("{error:#}");
    ensure!(
        text.contains("RevisionConflict")
            && !text.contains("SECRET_SENTINEL")
            && !text.contains("outcome is unknown")
    );
    {
        let script = server.state.lock().await;
        ensure!(script.requests.len() == 1 && script.requests[0].body == rollback);
    }
    let read = Action::Configuration {
        action: ConfigurationAction::Read {
            request: TenantRead {
                target: target(&server.endpoint),
                installation_id: "install-A".try_into()?,
                tenant: "Tenant-A".try_into()?,
            },
            revision: "configuration-1".try_into()?,
        },
    };
    let error = execute(&read, None, |_| Some(TOKEN.into()))
        .await
        .err()
        .context("accepted wrong tenant response")?;
    ensure!(!format!("{error:#}").contains("Tenant-B"));
    ensure!(server.state.lock().await.requests.len() == 2);
    server.finish().await
}
