//! Standalone local connector commands, independent of the gateway lifecycle.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Subcommand;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use bitrouter::connector::config::ConnectorConfig;
use bitrouter::connector::observer::Observer;
use bitrouter::connector::report::dry_run;
use bitrouter::connector::request::LocalRequest;
use bitrouter::connector::{executor::bounded_body, server};
use bitrouter::output::reports::connector::ControlReport;
use bitrouter::output::{CliReport, Output};

const MAX_CONFIG_BYTES: u64 = 65_536;

#[derive(Subcommand)]
pub enum Action {
    /// Validate the standalone configuration and report the observation plan.
    Check {
        /// Explicit standalone connector YAML; no gateway config discovery.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Read one bounded live metrics snapshot without sending inference.
    Observe {
        /// Explicit standalone connector YAML; no gateway config discovery.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Run the authenticated loopback pilot service with local durable state.
    Serve {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
        #[arg(long, default_value = "127.0.0.1:4367")]
        listen: SocketAddr,
        /// Initialize a new persistent authority once; never resets existing state.
        #[arg(long)]
        initialize_state: bool,
    },
    /// Read current local policy, observation and conservative occupancy.
    Status {
        #[command(flatten)]
        client: ClientArgs,
    },
    /// Stop new admissions and sends; existing work remains tracked.
    Pause {
        #[command(flatten)]
        client: ClientArgs,
    },
    /// Remove manual pause; health and all local bounds still apply.
    Resume {
        #[command(flatten)]
        client: ClientArgs,
    },
    /// Submit a strict pilot request file, writing its JSON or SSE response.
    Request {
        #[command(flatten)]
        client: ClientArgs,
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
    },
    /// Read a retained execution record without replaying its response.
    RequestStatus {
        #[command(flatten)]
        client: ClientArgs,
        #[arg(long)]
        request_id: String,
    },
    /// Cancel only if generation has not entered durable send intent.
    Cancel {
        #[command(flatten)]
        client: ClientArgs,
        #[arg(long)]
        request_id: String,
    },
    /// Record a trusted operator's evidence that one unknown attempt stopped.
    Reconcile {
        #[command(flatten)]
        client: ClientArgs,
        #[arg(long)]
        request_id: String,
        #[arg(long, value_name = "PATH")]
        file: PathBuf,
    },
}

#[derive(clap::Args)]
pub struct ClientArgs {
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
    #[arg(long, default_value = "http://127.0.0.1:4367")]
    endpoint: String,
}

pub async fn run(action: &Action, output: &Output) -> Result<()> {
    let path = match action {
        Action::Check { config } | Action::Observe { config } | Action::Serve { config, .. } => {
            config
        }
        Action::Status { client }
        | Action::Pause { client }
        | Action::Resume { client }
        | Action::Request { client, .. }
        | Action::RequestStatus { client, .. }
        | Action::Cancel { client, .. }
        | Action::Reconcile { client, .. } => &client.config,
    };
    let config = read_config(path).await?;
    match action {
        Action::Check { .. } => output.emit(&dry_run(&config)?)?,
        Action::Observe { .. } => {
            let snapshot = Observer::new(config)?.observe().await;
            let exit_code = snapshot.exit_code();
            output.emit(&snapshot)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Action::Serve {
            listen,
            initialize_state,
            ..
        } => server::serve(config, *listen, *initialize_state).await?,
        _ => client_action(action, &config, output).await?,
    }
    Ok(())
}

async fn client_action(action: &Action, config: &ConnectorConfig, output: &Output) -> Result<()> {
    let (client, method, suffix, file) = match action {
        Action::Status { client } => (client, reqwest::Method::GET, "status".to_owned(), None),
        Action::Pause { client } => (client, reqwest::Method::POST, "pause".to_owned(), None),
        Action::Resume { client } => (client, reqwest::Method::POST, "resume".to_owned(), None),
        Action::Request { client, file } => (
            client,
            reqwest::Method::POST,
            "requests".to_owned(),
            Some(file),
        ),
        Action::RequestStatus { client, request_id } => (
            client,
            reqwest::Method::GET,
            request_path(request_id, false)?,
            None,
        ),
        Action::Cancel { client, request_id } => (
            client,
            reqwest::Method::POST,
            request_path(request_id, true)?,
            None,
        ),
        Action::Reconcile {
            client,
            request_id,
            file,
        } => (
            client,
            reqwest::Method::POST,
            format!("{}/reconcile", request_path(request_id, false)?),
            Some(file),
        ),
        _ => anyhow::bail!("unsupported connector control operation"),
    };
    let execution = config
        .execution
        .as_ref()
        .context("execution_not_configured")?;
    let mut endpoint = url::Url::parse(&client.endpoint)
        .map_err(|_| anyhow::anyhow!("invalid local connector endpoint"))?;
    let loopback = match endpoint.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    anyhow::ensure!(
        endpoint.scheme() == "http"
            && loopback
            && endpoint.username().is_empty()
            && endpoint.password().is_none()
            && endpoint.query().is_none()
            && endpoint.fragment().is_none()
            && matches!(endpoint.path(), "" | "/"),
        "connector endpoint must be a loopback HTTP origin"
    );
    endpoint.set_path(&format!("/v1/connector/{suffix}"));
    let bearer = server::bearer(config).map_err(anyhow::Error::msg)?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_millis(
            execution.max_deadline_ms.saturating_add(1000),
        ))
        .build()
        .map_err(|_| anyhow::anyhow!("could not create local connector client"))?;
    let mut call = http.request(method, endpoint).bearer_auth(bearer);
    let mut request_id = None;
    if let Some(path) = file {
        let input = tokio::fs::File::open(path)
            .await
            .context("opening local connector request file")?;
        let mut body = Vec::new();
        input
            .take(execution.max_request_bytes as u64 + 1)
            .read_to_end(&mut body)
            .await
            .context("reading local connector request file")?;
        anyhow::ensure!(
            body.len() <= execution.max_request_bytes,
            "connector request body limit exceeded"
        );
        if matches!(action, Action::Request { .. }) {
            let request: LocalRequest = serde_json::from_slice(&body)
                .map_err(|_| anyhow::anyhow!("unsupported connector request shape"))?;
            // Active target/limits belong to the service after retained-ID
            // lookup; a valid old request may now be a state-only replay.
            request_path(&request.request_id, false)?;
            request_id = Some(request.request_id);
        }
        call = call
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let mut response = call
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("local connector service unavailable"))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let bytes = bounded_body(response, 4096)
            .await
            .map_err(anyhow::Error::msg)?;
        let code = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("code"))
                    .and_then(|code| code.as_str())
                    .map(str::to_owned)
            })
            .filter(|code| {
                !code.is_empty()
                    && code.len() <= 64
                    && code
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            });
        let code = match code {
            Some(code) => code,
            None => "operation_refused".to_owned(),
        };
        anyhow::bail!("local connector operation refused: {code} (HTTP {status})");
    }
    if matches!(action, Action::Request { .. }) {
        let id = request_id.context("connector request identity unavailable")?;
        let mut stdout = tokio::io::stdout();
        let mut total = 0usize;
        // SSE framing adds bounded overhead to the already-bounded upstream.
        let limit = execution
            .max_response_bytes
            .saturating_mul(2)
            .saturating_add(4096);
        let copied = async {
            while let Some(chunk) = response.chunk().await.map_err(|_| "response_interrupted")? {
                if chunk.len() > limit.saturating_sub(total) {
                    return Err("response_body_limit");
                }
                total += chunk.len();
                stdout
                    .write_all(&chunk)
                    .await
                    .map_err(|_| "response_delivery_failed")?;
                stdout
                    .flush()
                    .await
                    .map_err(|_| "response_delivery_failed")?;
            }
            Ok(())
        }
        .await;
        let confirmed = confirm_succeeded(&http, client, config, &id).await;
        if let Some(reason) = copied.err().or_else(|| confirmed.err()) {
            // Inference stdout is already JSON/SSE. Report final uncertainty on
            // stderr instead of appending a second ordinary CLI JSON envelope.
            eprintln!(
                "connector request {id}: {reason}; query connector request-status --request-id {id}"
            );
            std::process::exit(1);
        }
    } else {
        let bytes = bounded_body(response, 4_194_304)
            .await
            .map_err(anyhow::Error::msg)?;
        let report = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid local connector control response"))?;
        output.emit(&ControlReport(report))?;
    }
    Ok(())
}

async fn confirm_succeeded(
    http: &reqwest::Client,
    client: &ClientArgs,
    config: &ConnectorConfig,
    id: &str,
) -> Result<(), &'static str> {
    let mut endpoint = url::Url::parse(&client.endpoint).map_err(|_| "status_unavailable")?;
    endpoint.set_path(&format!("/v1/connector/requests/{id}"));
    let credential = server::bearer(config).map_err(|_| "status_unavailable")?;
    let response = http
        .get(endpoint)
        .bearer_auth(credential)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|_| "status_unavailable")?;
    if !response.status().is_success() {
        return Err("status_unavailable");
    }
    let bytes = bounded_body(response, 16_384)
        .await
        .map_err(|_| "status_unavailable")?;
    let record: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "status_unavailable")?;
    if record.get("request_id").and_then(serde_json::Value::as_str) != Some(id) {
        return Err("status_unavailable");
    }
    match record.get("state").and_then(serde_json::Value::as_str) {
        Some("succeeded") => Ok(()),
        Some("reserved") => Err("reserved"),
        Some("sending") => Err("sending"),
        Some("running") => Err("running"),
        Some("unknown") => Err("unknown"),
        Some("cancelled") => Err("cancelled"),
        Some("failed") => Err("failed"),
        Some("reconciled") => Err("reconciled"),
        _ => Err("status_unavailable"),
    }
}

fn request_path(id: &str, cancel: bool) -> Result<String> {
    anyhow::ensure!(
        !id.is_empty()
            && !matches!(id, "." | "..")
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
        "invalid connector request identity"
    );
    Ok(format!(
        "requests/{id}{}",
        if cancel { "/cancel" } else { "" }
    ))
}

async fn read_config(path: &Path) -> Result<ConnectorConfig> {
    let file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening connector configuration {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .context("reading connector configuration")?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "connector configuration exceeds the 64 KiB limit"
    );
    let source = std::str::from_utf8(&bytes).context("connector configuration must be UTF-8")?;
    ConnectorConfig::from_yaml(source).context("invalid connector configuration")
}
