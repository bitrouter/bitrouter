//! Persistent native model-picker integration, separate from one-shot launch.
mod claude;
mod codex;
mod files;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use serde::Serialize;

use crate::client::local_gateway;
use crate::output::human::Human;
use crate::output::{CliReport, Output};
use crate::providers::cli_install::resolve_binary;

const DEFAULT_MODEL: &str = "bitrouter/automatos";

fn default_model() -> String {
    DEFAULT_MODEL.into()
}

fn validate_display_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.trim() == name
            && name.len() <= 512
            && !name.chars().any(char::is_control),
        "display name must be nonempty, at most 512 bytes, without control characters or surrounding whitespace"
    );
    Ok(())
}

fn validate_model(model: &str) -> Result<()> {
    ensure!(
        !model.is_empty()
            && model.len() <= 512
            && !model.chars().any(|c| c.is_whitespace() || c.is_control()),
        "model must be a nonempty identifier of at most 512 bytes without whitespace or control characters"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Codex,
    Claude,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

#[derive(Debug, Args)]
pub struct Options {
    /// Omit for the interactive Codex / Claude menu.
    pub target: Option<Target>,
    /// BitRouter configuration used to find/start the local gateway.
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    /// Existing gateway root URL. Requires BITROUTER_API_KEY.
    #[arg(long, conflicts_with_all = ["remove", "config"])]
    pub base_url: Option<String>,
    /// Exact gateway model ID; new connections default to bitrouter/automatos.
    #[arg(long, conflicts_with = "remove")]
    pub model: Option<String>,
    /// Picker label; defaults to the exact gateway model ID.
    #[arg(long, conflicts_with = "remove")]
    pub display_name: Option<String>,
    /// Complete Codex ModelsResponse catalog; defaults to its current catalog/cache.
    #[arg(long, conflicts_with = "remove")]
    pub codex_catalog: Option<PathBuf>,
    /// Show destination files and effects without starting or changing anything.
    #[arg(long, conflicts_with = "remove")]
    pub dry_run: bool,
    /// Restore owned settings, preserving unrelated Desktop changes.
    #[arg(long, conflicts_with = "config")]
    pub remove: bool,
    /// Accept the displayed connection/restore plan without a prompt.
    #[arg(long)]
    pub yes: bool,
}

struct Paths {
    codex: PathBuf,
    claude: PathBuf,
    desktop: Option<PathBuf>,
    state: PathBuf,
}

impl Paths {
    fn discover(target: Target) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .context("cannot locate the user home")?
            .canonicalize()
            .context("cannot resolve the user home")?;
        let env_path = |key: &str, fallback: PathBuf| -> Result<PathBuf> {
            absolute(
                std::env::var_os(key)
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or(fallback),
            )
        };
        let codex = env_path("CODEX_HOME", home.join(".codex"))?;
        let claude = env_path("CLAUDE_CONFIG_DIR", home.join(".claude"))?;
        #[cfg(target_os = "macos")]
        let desktop_root = home.join("Library/Application Support/Claude-3p");
        #[cfg(target_os = "windows")]
        let desktop_root = env_path("LOCALAPPDATA", home.join("AppData/Local"))?.join("Claude-3p");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let desktop_root = env_path("XDG_CONFIG_HOME", home.join(".config"))?.join("Claude-3p");
        let desktop_root = env_path("CLAUDE_USER_DATA_DIR", desktop_root)?;
        let desktop = if matches!(target, Target::Claude) && desktop_detected(&home, &desktop_root)
        {
            Some(desktop_root)
        } else {
            None
        };
        Ok(Self {
            codex,
            claude,
            desktop,
            state: absolute(crate::paths::runtime_home()?)?.join("connections"),
        })
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn desktop_detected(home: &Path, config: &Path) -> bool {
    #[cfg(target_os = "macos")]
    if Path::new("/Applications/Claude.app").is_dir()
        || home.join("Applications/Claude.app").is_dir()
    {
        return true;
    }
    // An existing config library also supports nonstandard installations.
    let _ = home;
    config.join("configLibrary/_meta.json").is_file()
}

fn desktop_unmanaged() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let base = Path::new("/Library/Managed Preferences");
        let filename = "com.anthropic.claudefordesktop.plist";
        ensure!(
            !base.join(filename).exists(),
            "Claude Desktop has a managed profile; configure its gateway through the administrator"
        );
        if let Ok(user) = std::env::var("USER") {
            ensure!(
                !base.join(user).join(filename).exists(),
                "Claude Desktop has a managed user profile; configure its gateway through the administrator"
            );
        }
    }
    #[cfg(target_os = "windows")]
    for hive in ["HKLM", "HKCU"] {
        let result = std::process::Command::new("reg")
            .args(["query", &format!("{hive}\\SOFTWARE\\Policies\\Claude")])
            .output()
            .context("cannot check Claude Desktop managed settings")?;
        ensure!(
            !result.status.success(),
            "Claude Desktop has a managed policy; configure its gateway through the administrator"
        );
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    ensure!(
        !Path::new("/etc/claude-desktop/managed-settings.json").exists(),
        "Claude Desktop has managed settings; configure its gateway through the administrator"
    );
    Ok(())
}

#[derive(Serialize)]
struct Report {
    target: Target,
    model: String,
    display_name: String,
    endpoint: String,
    state: &'static str,
    files: Vec<PathBuf>,
    notes: Vec<String>,
    gateway_catalog_verified: bool,
    inference_verified: bool,
}

impl CliReport for Report {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "{}: {} (request: {}) — {}",
            self.target.name(),
            self.display_name,
            self.model,
            self.state
        ))?;
        for path in &self.files {
            h.line(&format!("  {}", path.display()))?;
        }
        for note in &self.notes {
            h.note(note)?;
        }
        Ok(())
    }
}

pub async fn run(options: Options, output: &Output) -> Result<()> {
    let target = match options.target {
        Some(target) => target,
        None => {
            ensure!(!options.yes, "--yes requires a target: codex or claude");
            match crate::prompt::select(
                "Connect BitRouter",
                "Add the selected gateway model to your native model picker.",
                vec![
                    crate::prompt::Item::new("Codex", "CLI + Desktop share one configuration"),
                    crate::prompt::Item::new("Claude", "Claude Code CLI + detected Claude Desktop"),
                ],
                0,
            )
            .await?
            {
                0 => Target::Codex,
                _ => Target::Claude,
            }
        }
    };
    ensure!(
        matches!(target, Target::Codex) || options.codex_catalog.is_none(),
        "--codex-catalog requires the codex target"
    );
    let paths = Paths::discover(target)?;
    let receipt_path = paths.state.join(format!("{}.json", target.name()));
    // A dry run takes no filesystem lock because it writes no files.
    let _lock = if options.dry_run {
        None
    } else {
        Some(files::lock(
            &paths.state.join(format!("{}.lock", target.name())),
        )?)
    };
    if let Some(receipt) = files::Receipt::load(&receipt_path)? {
        let mut report = report(
            target,
            &receipt,
            if options.remove {
                "restored"
            } else {
                "already_configured_restart_required"
            },
        );
        if options.remove {
            preview(&report);
            if approved(&options, "Restore these client settings?").await? {
                receipt.restore(&receipt_path)?;
            } else {
                report.state = "cancelled";
            }
        } else {
            ensure!(
                options
                    .model
                    .as_ref()
                    .is_none_or(|model| model == &receipt.model),
                "already connected to another model; use --remove before changing it"
            );
            ensure!(
                options
                    .display_name
                    .as_ref()
                    .is_none_or(|name| name == receipt.display_name()),
                "already connected with another display name; use --remove before changing it"
            );
            ensure!(
                options
                    .base_url
                    .as_ref()
                    .is_none_or(|url| endpoint(url).ok().as_ref() == Some(&receipt.endpoint)),
                "already connected to another gateway; use --remove before changing it"
            );
            ensure!(
                receipt.installed()?,
                "connection files changed or setup was interrupted; use --remove to recover before reconnecting"
            );
        }
        return output.emit(&report).map_err(Into::into);
    }
    ensure!(
        !options.remove,
        "no saved {} connection to remove",
        target.name()
    );
    let model = options.model.as_deref().unwrap_or(DEFAULT_MODEL);
    validate_model(model)?;
    let display_name = options.display_name.as_deref().unwrap_or(model);
    validate_display_name(display_name)?;
    // Explicit gateways require neither local configuration nor a local daemon.
    let local = if options.base_url.is_none() {
        let source = crate::paths::resolve_config(options.config.as_deref())?;
        let config = crate::paths::load_config(&source).await?;
        Some((source, config))
    } else {
        None
    };
    let endpoint = match (&options.base_url, &local) {
        (Some(url), _) => endpoint(url)?,
        (_, Some((_, config))) => endpoint(&local_gateway::derive_base_url(&config.server.listen))?,
        _ => anyhow::bail!("no gateway was selected"),
    };
    let token = match local_gateway::nonempty_env(local_gateway::BITROUTER_API_KEY_ENV) {
        Some(token) => token,
        None if local
            .as_ref()
            .is_some_and(|(_, config)| config.server.skip_auth)
            && loopback(&endpoint)? =>
        {
            local_gateway::PLACEHOLDER_API_KEY.into()
        }
        None => anyhow::bail!(
            "set BITROUTER_API_KEY to this gateway's credential, or run bro init for a local skip_auth gateway"
        ),
    };
    let mut notes = vec![
        "Requests for all selected client models will use the BitRouter gateway.".into(),
        format!(
            "Fully restart the client, then select {display_name} (request model: {model}). Project, profile or managed overrides may take precedence."
        ),
        "Gateway credentials and restore receipts are stored in private local files.".into(),
    ];
    let changes = match target {
        Target::Codex => {
            ensure!(
                codex::binary().is_some() || paths.codex.is_dir(),
                "Codex was not detected; install/open Codex first"
            );
            notes.push("The current default model and existing catalog entries are retained. Reconnect after client/catalog upgrades to refresh the local catalog.".into());
            let catalog = options.codex_catalog.clone().map(absolute).transpose()?;
            let bundled = if catalog.is_none() && !paths.codex.join("models_cache.json").exists() {
                codex::bundled_catalog().await
            } else {
                None
            };
            codex::prepare(
                &paths.codex,
                &endpoint,
                &token,
                model,
                options.display_name.as_deref(),
                catalog.as_deref(),
                bundled,
            )?
        }
        Target::Claude => {
            claude::check_environment()?;
            let mut changes = vec![];
            if resolve_binary("claude").is_some() || paths.claude.join("settings.json").exists() {
                changes.push(claude::cli(
                    &paths.claude,
                    &endpoint,
                    &token,
                    model,
                    display_name,
                )?);
            } else {
                notes.push("Claude Code CLI was not detected; skipped.".into());
            }
            if let Some(home) = &paths.desktop {
                desktop_unmanaged()?;
                notes.push("Claude Desktop accepts recognizable Claude route IDs. A display name does not bypass model validation or certify backend compatibility.".into());
                changes.extend(claude::desktop(
                    home,
                    &endpoint,
                    &token,
                    model,
                    display_name,
                )?);
                notes.push("Claude Desktop switches to a separate BitRouter gateway profile; previous profiles are retained. Cloud environments and Remote Control are unavailable in this mode.".into());
            } else {
                notes.push("Claude Desktop was not detected; skipped. Run connect --remove and reconnect after installing/opening it.".into());
            }
            ensure!(
                !changes.is_empty(),
                "neither Claude Code CLI nor Claude Desktop was detected"
            );
            changes
        }
    };
    let receipt = files::Receipt::new(endpoint.clone(), model.into(), changes)
        .with_display_name(options.display_name.clone());
    let mut report = report(
        target,
        &receipt,
        if options.dry_run {
            "preview"
        } else {
            "configured_restart_required"
        },
    );
    report.notes = notes;
    preview(&report);
    if options.dry_run {
        return output.emit(&report).map_err(Into::into);
    }
    if !approved(&options, "Connect these clients to BitRouter?").await? {
        report.state = "cancelled";
        return output.emit(&report).map_err(Into::into);
    }
    if let Some((source, config)) = &local
        && local_gateway::listen_is_local(&config.server.listen)
    {
        local_gateway::ensure_local_daemon(source, config, false).await;
    }
    verify_gateway(&endpoint, &token, model).await?;
    report.gateway_catalog_verified = true;
    receipt.apply(&receipt_path)?;
    output.emit(&report).map_err(Into::into)
}

fn report(target: Target, receipt: &files::Receipt, state: &'static str) -> Report {
    Report {
        target,
        model: receipt.model.clone(),
        display_name: receipt.display_name().into(),
        endpoint: receipt.endpoint.clone(),
        state,
        files: receipt
            .files
            .iter()
            .map(|change| change.path.clone())
            .collect(),
        notes: vec![
            "Fully restart the affected clients. No inference request was submitted.".into(),
        ],
        gateway_catalog_verified: false,
        inference_verified: false,
    }
}

fn preview(report: &Report) {
    eprintln!(
        "\n{} — {} via {}",
        report.target.name(),
        report.model,
        report.endpoint
    );
    for path in &report.files {
        eprintln!("  {}", path.display());
    }
    for note in &report.notes {
        eprintln!("  {note}");
    }
}

async fn approved(options: &Options, question: &str) -> Result<bool> {
    if options.yes {
        Ok(true)
    } else {
        ensure!(
            crate::prompt::terminal(),
            "noninteractive connection requires --yes (or --dry-run)"
        );
        crate::prompt::confirm(question, true).await
    }
}

fn endpoint(input: &str) -> Result<String> {
    let mut url = url::Url::parse(input).context("invalid gateway URL")?;
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "gateway URL must not contain credentials, query or fragment"
    );
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback(input)?),
        "remote gateway credentials require HTTPS; HTTP is allowed only on loopback"
    );
    let path = url.path().trim_end_matches('/');
    let prefix = path.strip_suffix("/v1").unwrap_or(path).to_string();
    url.set_path(&prefix);
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn loopback(input: &str) -> Result<bool> {
    Ok(match url::Url::parse(input)?.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    })
}

async fn verify_gateway(endpoint: &str, token: &str, model: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .get(format!("{endpoint}/v1/models"))
        .bearer_auth(token)
        .send()
        .await
        .context("gateway is unreachable; client files were not changed")?;
    ensure!(
        response.status().is_success(),
        "gateway catalog returned {}; client files were not changed",
        response.status()
    );
    let catalog: serde_json::Value = response.json().await.context("invalid gateway catalog")?;
    ensure!(
        catalog
            .get("data")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|models| models
                .iter()
                .any(|m| m.get("id").and_then(serde_json::Value::as_str) == Some(model))),
        "gateway does not advertise {model}; choose a model from its /v1/models catalog before connecting"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_do_not_leak_credentials_or_downgrade_transport() -> Result<()> {
        assert_eq!(
            endpoint("http://127.0.0.1:4356/v1/")?,
            "http://127.0.0.1:4356"
        );
        assert_eq!(
            endpoint("https://gateway.example/v1")?,
            "https://gateway.example"
        );
        assert_eq!(
            endpoint("https://gateway.example/proxy/team/v1/")?,
            "https://gateway.example/proxy/team"
        );
        assert_eq!(
            endpoint("https://gateway.example/proxy/team")?,
            "https://gateway.example/proxy/team"
        );
        for input in [
            "http://gateway.example",
            "https://user:secret@example.org",
            "https://example.org/?key=secret",
            "https://example.org/#fragment",
        ] {
            assert!(endpoint(input).is_err());
        }
        Ok(())
    }

    #[test]
    fn model_ids_are_not_restricted_to_automatos() -> Result<()> {
        for model in [
            DEFAULT_MODEL,
            "vendor/coder:latest",
            "company/team/model-v2",
            "plain-model",
        ] {
            validate_model(model)?;
        }
        for model in ["", "bad\nmodel", " leading", "control\u{1b}"] {
            assert!(validate_model(model).is_err());
        }
        assert!(validate_model(&"a".repeat(513)).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn gateway_requires_exact_alias_and_does_not_follow_redirects() -> Result<()> {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        let model = "company/coding-v2";
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"data":[{"id": model}]})),
            )
            .mount(&server)
            .await;
        verify_gateway(&server.uri(), "test", model).await?;
        assert!(
            verify_gateway(&server.uri(), "test", DEFAULT_MODEL)
                .await
                .is_err()
        );
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("Location", "https://example.org"),
            )
            .mount(&server)
            .await;
        assert!(verify_gateway(&server.uri(), "test", model).await.is_err());
        Ok(())
    }
}
