//! `bitrouter agents` — lifecycle CLI for upstream ACP agents.
//!
//! Three verbs:
//! - `list` — show the bundled catalog of well-known agents and which of
//!   them are currently configured under `agents:` in `bitrouter.yaml`.
//! - `check` — spawn each configured agent and verify it answers
//!   `initialize`. Same shape as `bitrouter tools status` for MCP.
//! - `install <id>` — look up the agent in the catalog and emit a YAML
//!   stub the user can paste into the `agents:` block.
//!
//! v1.0 deliberately ships a compiled-in catalog rather than fetching an
//! external registry: the well-known ACP agents are all npm packages
//! invoked via `npx -y`, so there is no binary to download or checksum.
//! External-registry + binary-install support is tracked as a follow-up.

use std::time::{Duration, Instant};

use bitrouter_sdk::acp::{AcpStdioExecutor, AcpTarget, Executor};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::config::Config;

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
    match &cfg.transport {
        AcpTransport::Stdio { command, args, .. } => {
            if args.is_empty() {
                format!("stdio {command}")
            } else {
                format!("stdio {command} {}", args.join(" "))
            }
        }
    }
}

/// `bitrouter agents check` — spawn each *configured* agent, send an
/// `initialize` request, and report whether the round-trip succeeded.
pub async fn check(config: &Config) -> Vec<CheckRow> {
    let executor = AcpStdioExecutor::new();
    let mut out = Vec::with_capacity(config.agents.len());
    let mut sorted: Vec<_> = config.agents.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (id, cfg) in sorted {
        let target = AcpTarget {
            agent_name: id.clone(),
            transport: cfg.transport.clone(),
        };
        let request = bitrouter_sdk::acp::AcpRequest::new(
            id.clone(),
            "initialize",
            serde_json::json!({}),
            CallerContext::local(),
        );
        let started = Instant::now();
        let outcome = match executor.execute(&target, &request).await {
            Ok(_) => Ok(started.elapsed()),
            Err(e) => Err(e.to_string()),
        };
        out.push(CheckRow {
            id: id.clone(),
            outcome,
        });
    }
    out
}

/// `bitrouter agents install <id>` — look up `id` in the catalog and emit
/// a YAML stub the user can paste under `agents:` in `bitrouter.yaml`.
/// Returns an error if `id` is not a catalog entry.
pub fn install(id: &str) -> Result<String, String> {
    let agent = lookup_catalog(id)
        .ok_or_else(|| format!("'{id}' is not in the bundled v1.0 catalog. Run `bitrouter agents list` to see the available ids."))?;
    Ok(render_install_stub(agent))
}

fn render_install_stub(agent: &KnownAgent) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {} — paste under `agents:` in bitrouter.yaml.\n",
        agent.description
    ));
    out.push_str(&format!("# Source: {}\n", agent.project_url));
    out.push_str(&format!("{}:\n", agent.id));
    out.push_str(&format!("  name: {}\n", agent.id));
    out.push_str("  transport:\n");
    out.push_str("    type: stdio\n");
    out.push_str(&format!("    command: {}\n", yaml_scalar(agent.command)));
    out.push_str("    args:\n");
    for a in agent.args {
        out.push_str(&format!("      - {}\n", yaml_scalar(a)));
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
        AcpAgentConfig {
            name: name.into(),
            transport: AcpTransport::Stdio {
                command: cmd.into(),
                args: vec![],
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

    #[tokio::test]
    async fn check_reports_per_agent_failure_independently() {
        let mut cfg = Config::default();
        cfg.agents.insert("a".into(), agent("a", "/bin/false"));
        cfg.agents.insert("b".into(), agent("b", "/bin/false"));
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
        assert!(err.contains("not in the bundled v1.0 catalog"));
        assert!(err.contains("bitrouter agents list"));
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
