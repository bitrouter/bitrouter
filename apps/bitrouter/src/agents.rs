//! `bitrouter agents` — lifecycle CLI for upstream ACP agents.
//!
//! Three verbs:
//! - `list` — show the bundled catalog of well-known agents and which of
//!   them are currently configured under `agents:` in `bitrouter.yaml`;
//!   `--remote` additionally fetches the official ACP registry and lists
//!   its agents.
//! - `check` — spawn each configured agent and verify it answers
//!   `initialize`. Same shape as `bitrouter tools status` for MCP.
//! - `install <id>` — look up the agent in the compiled catalog, falling
//!   back to the ACP registry (`npx`/`uvx` distributions only), and emit a
//!   YAML stub the user can paste into the `agents:` block.
//!
//! The compiled-in catalog stays as the offline baseline; the registry
//! ([`crate::agent_registry`]) is the discovery tier on top. `binary`
//! registry distributions are listed but never auto-installed — the registry
//! carries no checksums, so those installs stay manual and user-verified.

use std::time::Duration;

use bitrouter_sdk::config::Config;

use crate::agent_registry::{InstallSupport, Registry, RegistryAgent};

/// One well-known ACP agent in the bundled catalog.
#[derive(Debug, Clone, Copy)]
pub struct KnownAgent {
    /// The recommended id (also the catalog key).
    pub id: &'static str,
    /// One-line human description.
    pub description: &'static str,
    /// Upstream project URL — the source of the recommended invocation.
    pub project_url: &'static str,
    /// Command to spawn.
    pub command: &'static str,
    /// Args to pass.
    pub args: &'static [&'static str],
}

/// The bundled v1.0 catalog. Limited to publicly-available, actively
/// maintained agents. Update by submitting a PR to bitrouter.
pub const CATALOG: &[KnownAgent] = &[
    KnownAgent {
        id: "claude-acp",
        description: "Anthropic Claude via Zed's `claude-code-acp`",
        project_url: "https://github.com/zed-industries/claude-code-acp",
        command: "npx",
        args: &["-y", "@zed-industries/claude-code-acp@latest"],
    },
    KnownAgent {
        id: "codex-acp",
        description: "OpenAI Codex via Zed's `codex-acp`",
        project_url: "https://github.com/zed-industries/codex-acp",
        command: "npx",
        args: &["-y", "@zed-industries/codex-acp@latest"],
    },
    KnownAgent {
        id: "gemini-cli",
        description: "Google's Gemini CLI with `--experimental-acp`",
        project_url: "https://github.com/google-gemini/gemini-cli",
        command: "npx",
        args: &[
            "-y",
            "--",
            "@google/gemini-cli@latest",
            "--experimental-acp",
        ],
    },
    KnownAgent {
        id: "pi-acp",
        description: "pi coding agent via `pi-acp` (needs `pi` on PATH)",
        project_url: "https://github.com/svkozak/pi-acp",
        command: "npx",
        args: &["-y", "pi-acp@latest"],
    },
];

fn lookup_catalog(id: &str) -> Option<&'static KnownAgent> {
    CATALOG.iter().find(|a| a.id == id)
}

/// One row in `bitrouter agents list`.
#[derive(Debug, Clone)]
pub struct ListRow {
    /// Agent id.
    pub id: String,
    /// Whether the id is present under `agents:` in the loaded config.
    pub configured: bool,
    /// Whether the id is present in the bundled catalog.
    pub in_catalog: bool,
    /// One-line description (catalog if known; configured invocation
    /// otherwise).
    pub description: String,
}

/// One row in `bitrouter agents check`.
#[derive(Debug, Clone)]
pub struct CheckRow {
    /// Agent id.
    pub id: String,
    /// `Ok(latency)` on a successful `initialize` round-trip; `Err(msg)`
    /// on spawn/transport/JSON-RPC failure.
    pub outcome: Result<Duration, String>,
}

/// `bitrouter agents list` — show the catalog merged with the user's
/// configured agents. Sorted by id.
pub fn list(config: &Config) -> Vec<ListRow> {
    let mut ids: std::collections::BTreeMap<String, ListRow> = Default::default();
    // Catalog entries first.
    for a in CATALOG {
        ids.insert(
            a.id.to_string(),
            ListRow {
                id: a.id.to_string(),
                configured: false,
                in_catalog: true,
                description: a.description.to_string(),
            },
        );
    }
    // Configured entries — set the configured flag, override description
    // with the actual invocation when the id is custom (not in catalog).
    for (id, cfg) in &config.agents {
        ids.entry(id.clone())
            .and_modify(|row| row.configured = true)
            .or_insert_with(|| ListRow {
                id: id.clone(),
                configured: true,
                in_catalog: false,
                description: describe_invocation(cfg),
            });
    }
    ids.into_values().collect()
}

fn describe_invocation(cfg: &bitrouter_sdk::acp::AcpAgentConfig) -> String {
    use bitrouter_sdk::acp::AcpTransport;
    let full = match &cfg.transport {
        AcpTransport::Stdio { command, args, .. } => {
            if args.is_empty() {
                format!("stdio {command}")
            } else {
                format!("stdio {command} {}", args.join(" "))
            }
        }
    };
    // Keep the list table one row per agent: collapse embedded newlines/runs
    // of whitespace (inline scripts as args) and cap the length.
    let mut one_line = full.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 80;
    if one_line.chars().count() > MAX {
        one_line = one_line.chars().take(MAX - 1).collect::<String>() + "…";
    }
    one_line
}

/// `bitrouter agents check` — spawn each *configured* agent, send an
/// `initialize` request, and report whether the round-trip succeeded.
pub async fn check(config: &Config) -> Vec<CheckRow> {
    use bitrouter_sdk::acp::AcpTransport;
    let mut out = Vec::with_capacity(config.agents.len());
    let mut sorted: Vec<_> = config.agents.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (id, cfg) in sorted {
        let outcome = match &cfg.transport {
            AcpTransport::Stdio { command, args, env } => {
                bitrouter_substrate::up::health_check(command, args, env).await
            }
        };
        out.push(CheckRow {
            id: id.clone(),
            outcome,
        });
    }
    out
}

/// One row in the registry table of `bitrouter agents list --remote`.
#[derive(Debug, Clone)]
pub struct RemoteRow {
    pub id: String,
    pub version: String,
    /// How the entry installs: `npx` / `uvx` (stub-able), `manual`
    /// (binary-only), or `-` (no distribution).
    pub install: &'static str,
    pub description: String,
}

/// Rows for the ACP registry table, sorted by id.
pub fn registry_rows(registry: &Registry) -> Vec<RemoteRow> {
    let mut rows: Vec<RemoteRow> = registry
        .agents
        .iter()
        .map(|a| RemoteRow {
            id: a.id.clone(),
            version: a.version.clone().unwrap_or_else(|| "-".to_string()),
            install: match a.install_support() {
                InstallSupport::Stub(runner) => runner,
                InstallSupport::Manual => "manual",
                InstallSupport::None => "-",
            },
            description: a
                .description
                .clone()
                .or_else(|| a.name.clone())
                .unwrap_or_default(),
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// `bitrouter agents install <id>`, catalog tier — look up `id` in the
/// compiled catalog and emit a YAML stub the user can paste under `agents:`
/// in `bitrouter.yaml`. Returns an error if `id` is not a catalog entry (the
/// CLI then falls back to the registry tier).
pub fn install(id: &str) -> Result<String, String> {
    let agent = lookup_catalog(id)
        .ok_or_else(|| format!("'{id}' is not in the bundled catalog. Run `bitrouter agents list` (or `--remote` for the ACP registry) to see the available ids."))?;
    Ok(render_stub(&StubSpec {
        id: agent.id,
        comment: agent.description,
        source: agent.project_url,
        command: agent.command,
        args: agent.args.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
    }))
}

/// `bitrouter agents install <id>`, registry tier — emit a stub for a
/// registry entry whose distribution maps onto a package runner (`npx` /
/// `uvx`). Binary-only entries are refused with a pointer to the project
/// (no checksums in the registry — manual install stays user-verified).
pub fn install_from_registry(registry: &Registry, id: &str) -> Result<String, String> {
    let agent: &RegistryAgent = registry
        .agents
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| format!("'{id}' is not in the ACP registry either. Run `bitrouter agents list --remote` to see the available ids."))?;
    let Some(invocation) = agent.stdio_invocation() else {
        let hint = agent
            .repository
            .as_deref()
            .unwrap_or("the project's documentation");
        return Err(format!(
            "'{id}' is distributed as a platform binary; the registry carries no checksums, so install it manually from {hint} and add a stdio `agents:` entry pointing at the installed command."
        ));
    };
    let comment = agent
        .description
        .clone()
        .or_else(|| agent.name.clone())
        .unwrap_or_else(|| agent.id.clone());
    Ok(render_stub(&StubSpec {
        id: &agent.id,
        comment: &comment,
        source: agent.repository.as_deref().unwrap_or("ACP registry"),
        command: invocation.command,
        args: invocation.args,
        env: invocation.env.into_iter().collect(),
    }))
}

/// Everything the YAML stub renderer needs, whichever tier it came from.
struct StubSpec<'a> {
    id: &'a str,
    comment: &'a str,
    source: &'a str,
    command: &'a str,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

fn render_stub(spec: &StubSpec<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} — paste under `agents:` in bitrouter.yaml.\n",
        spec.comment
    ));
    out.push_str(&format!("# Source: {}\n", spec.source));
    out.push_str(&format!("{}:\n", spec.id));
    out.push_str(&format!("  name: {}\n", spec.id));
    out.push_str("  transport:\n");
    out.push_str("    type: stdio\n");
    out.push_str(&format!("    command: {}\n", yaml_scalar(spec.command)));
    out.push_str("    args:\n");
    for a in &spec.args {
        out.push_str(&format!("      - {}\n", yaml_scalar(a)));
    }
    if !spec.env.is_empty() {
        out.push_str("    env:\n");
        for (k, v) in &spec.env {
            out.push_str(&format!("      {}: {}\n", yaml_scalar(k), yaml_scalar(v)));
        }
    }
    out
}

/// Quote a YAML scalar when it starts with a character YAML treats as
/// special at the start of a plain scalar (`@`, `` ` ``, `!`, `&`, `*`, `|`,
/// `>`, `'`, `"`, `%`, `#`, `?`, `:`, `-`, `,`, `{`, `}`, `[`, `]`), or
/// contains a space or `#`. Conservative: when in doubt, double-quote.
fn yaml_scalar(s: &str) -> String {
    if s.is_empty() {
        // An unquoted empty plain scalar parses as YAML null, which would
        // silently corrupt a command/arg field. Always quote.
        return "\"\"".to_string();
    }
    let first_special = s
        .chars()
        .next()
        .map(|c| {
            matches!(
                c,
                '@' | '`'
                    | '!'
                    | '&'
                    | '*'
                    | '|'
                    | '>'
                    | '\''
                    | '"'
                    | '%'
                    | '#'
                    | '?'
                    | ':'
                    | '-'
                    | ','
                    | '{'
                    | '}'
                    | '['
                    | ']'
            )
        })
        .unwrap_or(false);
    let needs_quotes = first_special || s.contains(' ') || s.contains('#') || s.contains(':');
    if needs_quotes {
        // Escape backslashes and double quotes so the result is a valid
        // YAML / JSON double-quoted scalar.
        let escaped = s.replace('\\', r"\\").replace('"', r#"\""#);
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::acp::{AcpAgentConfig, AcpTransport};
    use std::collections::HashMap;

    fn agent(name: &str, cmd: &str) -> AcpAgentConfig {
        agent_with_args(name, cmd, &[])
    }

    fn agent_with_args(name: &str, cmd: &str, args: &[&str]) -> AcpAgentConfig {
        AcpAgentConfig {
            name: name.into(),
            transport: AcpTransport::Stdio {
                command: cmd.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
                env: HashMap::new(),
            },
        }
    }

    #[test]
    fn list_returns_catalog_when_no_agents_configured() {
        let cfg = Config::default();
        let rows = list(&cfg);
        assert!(rows.len() >= 3, "catalog should have at least 3 agents");
        assert!(rows.iter().all(|r| r.in_catalog));
        assert!(rows.iter().all(|r| !r.configured));
        assert!(rows.iter().any(|r| r.id == "claude-acp"));
    }

    #[test]
    fn list_marks_configured_catalog_entries() {
        let mut cfg = Config::default();
        cfg.agents
            .insert("claude-acp".into(), agent("claude-acp", "npx"));
        let rows = list(&cfg);
        let claude = rows.iter().find(|r| r.id == "claude-acp").unwrap();
        assert!(claude.configured);
        assert!(claude.in_catalog);
    }

    #[test]
    fn list_includes_custom_agents_not_in_catalog() {
        let mut cfg = Config::default();
        cfg.agents.insert("my-bot".into(), agent("my-bot", "./bot"));
        let rows = list(&cfg);
        let custom = rows.iter().find(|r| r.id == "my-bot").unwrap();
        assert!(custom.configured);
        assert!(!custom.in_catalog);
        assert!(custom.description.contains("stdio ./bot"));
    }

    /// A bash stub that answers `initialize` with a JSON-RPC error so the
    /// health-check fails fast (no process-exit hang, no timeout wait).
    #[cfg(unix)]
    const ERROR_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*) printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32600,"message":"not supported"}}\n' "$id";;
          esac
        done
    "#;

    #[cfg(unix)]
    #[tokio::test]
    async fn check_reports_per_agent_failure_independently() {
        let mut cfg = Config::default();
        cfg.agents.insert(
            "a".into(),
            agent_with_args("a", "bash", &["-c", ERROR_STUB]),
        );
        cfg.agents.insert(
            "b".into(),
            agent_with_args("b", "bash", &["-c", ERROR_STUB]),
        );
        let rows = check(&cfg).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "a");
        assert_eq!(rows[1].id, "b");
        assert!(rows[0].outcome.is_err());
        assert!(rows[1].outcome.is_err());
    }

    #[test]
    fn install_unknown_id_errors_with_hint() {
        let err = install("nonexistent").unwrap_err();
        assert!(err.contains("not in the bundled catalog"));
        assert!(err.contains("bitrouter agents list"));
    }

    /// Registry fixture mirroring the real document shape: npx (stub-able),
    /// uvx with env (stub-able), binary-only (manual).
    const REGISTRY_FIXTURE: &str = r#"{
      "agents": [
        {
          "id": "gemini",
          "name": "Gemini CLI",
          "version": "0.50.0",
          "description": "Google's official CLI for Gemini",
          "repository": "https://github.com/google-gemini/gemini-cli",
          "distribution": { "npx": { "package": "@google/gemini-cli@0.50.0", "args": ["--acp"] } }
        },
        {
          "id": "pytool",
          "name": "PyTool",
          "distribution": { "uvx": { "package": "pytool-acp@1.2.0", "env": { "PYTOOL_API_KEY": "" } } }
        },
        {
          "id": "opencode",
          "name": "OpenCode",
          "version": "1.17.15",
          "repository": "https://github.com/anomalyco/opencode",
          "distribution": { "binary": { "darwin-aarch64": { "archive": "https://example.com/o.zip", "cmd": "./opencode", "args": ["acp"] } } }
        }
      ]
    }"#;

    fn fixture_registry() -> crate::agent_registry::Registry {
        crate::agent_registry::parse(REGISTRY_FIXTURE).expect("fixture parses")
    }

    #[test]
    fn registry_rows_classify_install_support() {
        let rows = registry_rows(&fixture_registry());
        assert_eq!(rows.len(), 3);
        let by_id = |id: &str| rows.iter().find(|r| r.id == id).expect("row");
        assert_eq!(by_id("gemini").install, "npx");
        assert_eq!(by_id("gemini").version, "0.50.0");
        assert_eq!(by_id("pytool").install, "uvx");
        assert_eq!(by_id("opencode").install, "manual");
    }

    #[test]
    fn install_from_registry_emits_schema_valid_stub_with_env() {
        let out = install_from_registry(&fixture_registry(), "pytool").expect("stub");
        let body: HashMap<String, AcpAgentConfig> =
            serde_saphyr::from_str(&out).expect("stub must deserialise into the config schema");
        let entry = body.get("pytool").expect("pytool key present");
        match &entry.transport {
            AcpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "uvx");
                assert_eq!(args, &["pytool-acp@1.2.0".to_string()]);
                assert!(env.contains_key("PYTOOL_API_KEY"));
            }
        }
    }

    #[test]
    fn install_from_registry_npx_pins_version() {
        let out = install_from_registry(&fixture_registry(), "gemini").expect("stub");
        assert!(out.contains("@google/gemini-cli@0.50.0"));
        assert!(
            out.contains("- \"--acp\"") || out.contains("- --acp"),
            "{out}"
        );
    }

    #[test]
    fn install_from_registry_refuses_binary_only_with_pointer() {
        let err = install_from_registry(&fixture_registry(), "opencode").unwrap_err();
        assert!(err.contains("platform binary"));
        assert!(err.contains("github.com/anomalyco/opencode"));
    }

    #[test]
    fn install_from_registry_unknown_id_hints_remote_list() {
        let err = install_from_registry(&fixture_registry(), "nope").unwrap_err();
        assert!(err.contains("agents list --remote"));
    }

    #[test]
    fn install_known_id_emits_parseable_yaml_stub() {
        let out = install("claude-acp").unwrap();
        assert!(out.contains("claude-acp:"));
        assert!(out.contains("type: stdio"));
        assert!(out.contains("command: npx"));
        assert!(out.contains("@zed-industries/claude-code-acp@latest"));
        // The stub should be a single top-level key parseable as a fragment.
        let parsed: serde_json::Value =
            serde_saphyr::from_str(&out).expect("stub must parse as YAML");
        assert!(parsed.is_object(), "expected mapping, got {parsed:?}");
    }

    #[test]
    fn install_emits_project_url_for_attribution() {
        let out = install("gemini-cli").unwrap();
        assert!(out.contains("github.com"));
    }

    #[test]
    fn catalog_includes_pi_acp() {
        let a = lookup_catalog("pi-acp").expect("pi-acp is in the bundled catalog");
        assert_eq!(a.command, "npx");
        assert_eq!(a.args, &["-y", "pi-acp@latest"]);
        let out = install("pi-acp").unwrap();
        assert!(out.contains("pi-acp:"));
        assert!(out.contains("pi-acp@latest"));
        assert!(out.contains("github.com/svkozak/pi-acp"));
    }

    #[test]
    fn yaml_scalar_quotes_empty_string() {
        // Empty unquoted plain scalar would parse as YAML null and silently
        // corrupt a command / arg field. Regression check: the empty case
        // must be double-quoted.
        assert_eq!(yaml_scalar(""), "\"\"");
    }

    #[test]
    fn install_round_trips_through_acp_agent_config_schema() {
        // The stub should deserialise into a real `(String, AcpAgentConfig)`
        // pair without further editing, so users can paste-and-go.
        let out = install("codex-acp").unwrap();
        // strip the leading `# ...` comment lines so serde_yml only sees
        // the YAML body. (Comments are preserved on round-trip but
        // serde_saphyr::from_str handles them fine; the comment stripping is
        // belt-and-braces.)
        let body: HashMap<String, AcpAgentConfig> =
            serde_saphyr::from_str(&out).expect("stub must deserialise into the config schema");
        let entry = body.get("codex-acp").expect("codex-acp key present");
        match &entry.transport {
            AcpTransport::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert!(args.iter().any(|a| a.contains("codex-acp")));
            }
        }
    }
}
