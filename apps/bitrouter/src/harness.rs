//! Unified coding-agent **harness catalog** — the single source of truth for
//! how BitRouter finds, launches, and routes each supported harness.
//!
//! A harness is drivable in either of two facets:
//!
//! - **interactive** (`bitrouter launch`) — the harness's own native TUI
//!   (`claude`, `codex`), launched as a child with its LLM traffic env-wrapped
//!   to the daemon; the human drives it directly.
//! - **ACP** (`bitrouter spawn`) — a headless ACP adapter
//!   (`@zed-industries/claude-code-acp`, …) driven as a sub-agent by a program.
//!
//! The crucial fact this module encodes once: **both facets route through the
//! daemon with the same [`Routing`]**, because the interactive binary and the
//! ACP adapter of a given harness share a config/env surface (verified against
//! adapter source — see `SPAWN_SPEC.md` §6). So the routing knowledge that used
//! to live in `spawn::AgentSpec` *and* would have been duplicated onto the ACP
//! side lives here exactly once, and `launch`, `spawn`, and `agents install`
//! all read it.

/// BitRouter's own API-key env var (`brk_…`). When set, it is forwarded to the
/// harness as the gateway bearer credential.
pub const BITROUTER_API_KEY_ENV: &str = "BITROUTER_API_KEY";

/// Placeholder credential injected when no real key is available. Ignored by
/// the daemon under `skip_auth: true` (the `bitrouter init` default); the
/// harness merely needs *some* credential to start.
pub const PLACEHOLDER_API_KEY: &str = "bitrouter-local";

use anyhow::Context;

/// One catalog harness. Keyed by [`id`](Self::id), which is the ACP-facet
/// id used under `agents:` in `bitrouter.yaml` and by `bitrouter agents`.
#[derive(Debug, Clone, Copy)]
pub struct Harness {
    /// Catalog / ACP-config id (e.g. `claude-acp`). Also the `agents install`
    /// key.
    pub id: &'static str,
    /// One-line human description (shown in `agents list`).
    pub description: &'static str,
    /// Upstream project URL — the source of the recommended invocation.
    pub project_url: &'static str,
    /// The ACP adapter invocation (`command` + `args`) for `bitrouter spawn`.
    pub acp_command: &'static str,
    /// Args passed to [`acp_command`](Self::acp_command).
    pub acp_args: &'static [&'static str],
    /// A substring that identifies this harness inside a configured agent's
    /// invocation (command or any arg) — used to map a user-renamed
    /// `agents:` entry back to its catalog routing (invocation matching, so
    /// the YAML key carries no semantics). Usually the adapter package name.
    pub package_marker: &'static str,
    /// The interactive native-TUI binary for `bitrouter launch`, when the
    /// harness has one. `None` for adapter-only harnesses (gemini, pi).
    pub interactive_binary: Option<&'static str>,
    /// How this harness's LLM traffic is pointed at the daemon.
    pub routing: Routing,
}

/// How a harness's LLM traffic is redirected to the BitRouter gateway. One
/// value per harness, applied identically to both facets.
#[derive(Debug, Clone, Copy)]
pub enum Routing {
    /// Env-var redirection: set `base_url_env` to the gateway URL and
    /// `auth_env` to the bearer credential (BitRouter's inbound scheme is
    /// `Authorization: Bearer`; never a provider `x-api-key` var). Optional
    /// `model_env` pins the model; `extra` carries fixed vars the redirect
    /// needs. Used by claude-code-acp (and interactive Claude Code) and,
    /// best-effort, gemini-cli.
    Env {
        /// Var the harness reads its gateway base URL from.
        base_url_env: &'static str,
        /// Var the harness turns into the gateway credential.
        auth_env: &'static str,
        /// Whether the harness sends `auth_env` as `Authorization: Bearer`
        /// (BitRouter's inbound scheme). `false` means a provider-native
        /// header the daemon's auth hook rejects under `skip_auth: false`
        /// (e.g. gemini's `x-goog-api-key`) — routing then only works under
        /// `skip_auth: true`, and callers warn when auth is required.
        bearer_auth: bool,
        /// Var that pins the model, when the harness supports one.
        model_env: Option<&'static str>,
        /// Fixed vars required for the redirect to take effect.
        extra: &'static [(&'static str, &'static str)],
    },
    /// Codex `-c` one-shot config overrides (codex-acp forwards argv to codex
    /// core, so the same overrides work for both facets). The gateway must
    /// speak the OpenAI **Responses** API — pinned codex builds dropped
    /// `wire_api = "chat"`.
    CodexArgs,
    /// Config-file synthesis (opencode): the harness loads the JSON config
    /// `OPENCODE_CONFIG` points at, and routing, the default model, and MCP
    /// injection all ride that one synthesized file. There is no pure
    /// env/args overlay — headless spawn launches direct with a note; the
    /// interactive facet synthesizes via [`Harness::orchestrator_overlay`].
    OpencodeConfig,
    /// Config-dir synthesis (pi — SPAWN_SPEC §6.4): pi has no base-URL env
    /// var, so routing synthesizes a `models.json` in a per-launch dir and
    /// points `PI_CODING_AGENT_DIR` at it, selecting the provider/model by
    /// CLI flag. Interactive facet only; headless spawn launches direct
    /// with a note.
    PiConfigDir,
}

/// The child-process overrides a [`Routing`] contributes: env vars to set
/// (injection wins over inherited/config env) and args to append to the
/// harness invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingOverlay {
    /// Env vars to set on the child (each overrides any inherited value).
    pub env: Vec<(String, String)>,
    /// Args appended to the harness invocation.
    pub args: Vec<String>,
}

/// The bundled catalog. Limited to publicly-available, actively-maintained
/// harnesses. Extend by PR to bitrouter.
pub const CATALOG: &[Harness] = &[
    Harness {
        id: "claude-acp",
        description: "Anthropic Claude via Zed's `claude-code-acp`",
        project_url: "https://github.com/zed-industries/claude-code-acp",
        acp_command: "npx",
        acp_args: &["-y", "@zed-industries/claude-code-acp@latest"],
        // A substring of both the npm spec (`@zed-industries/claude-code-acp`)
        // and the globally-installed binary name (`claude-code-acp`), so a
        // `command: claude-code-acp` config still catalog-matches.
        package_marker: "claude-code-acp",
        interactive_binary: Some("claude"),
        // claude-code-acp passes process env through to the SDK-spawned CLI,
        // which honors these exactly as interactive Claude Code does.
        // ANTHROPIC_AUTH_TOKEN → `Authorization: Bearer` (also suppresses the
        // login requirement); ANTHROPIC_API_KEY would be `x-api-key`, not
        // BitRouter's inbound scheme, so we never touch it.
        routing: Routing::Env {
            base_url_env: "ANTHROPIC_BASE_URL",
            auth_env: "ANTHROPIC_AUTH_TOKEN",
            bearer_auth: true,
            model_env: Some("ANTHROPIC_MODEL"),
            extra: &[],
        },
    },
    Harness {
        id: "codex-acp",
        description: "OpenAI Codex via Zed's `codex-acp`",
        project_url: "https://github.com/zed-industries/codex-acp",
        acp_command: "npx",
        acp_args: &["-y", "@zed-industries/codex-acp@latest"],
        package_marker: "codex-acp",
        interactive_binary: Some("codex"),
        routing: Routing::CodexArgs,
    },
    Harness {
        id: "gemini-cli",
        description: "Google's Gemini CLI with `--experimental-acp` (best-effort routing)",
        project_url: "https://github.com/google-gemini/gemini-cli",
        acp_command: "npx",
        acp_args: &[
            "-y",
            "--",
            "@google/gemini-cli@latest",
            "--experimental-acp",
        ],
        // Substring of both `@google/gemini-cli` and the `gemini-cli` binary.
        package_marker: "gemini-cli",
        interactive_binary: None,
        // Best-effort: gemini-cli is deprecated upstream (Antigravity). Sends
        // GEMINI_API_KEY as `x-goog-api-key`, which the daemon accepts only
        // under skip_auth; GOOGLE_GEMINI_BASE_URL auto-selects GATEWAY auth.
        routing: Routing::Env {
            base_url_env: "GOOGLE_GEMINI_BASE_URL",
            auth_env: "GEMINI_API_KEY",
            // gemini sends GEMINI_API_KEY as `x-goog-api-key`, not Bearer —
            // the daemon accepts it only under `skip_auth: true`.
            bearer_auth: false,
            model_env: Some("GEMINI_MODEL"),
            extra: &[],
        },
    },
    Harness {
        id: "opencode",
        description: "sst's opencode via its native `opencode acp`",
        project_url: "https://github.com/sst/opencode",
        acp_command: "opencode",
        acp_args: &["acp"],
        package_marker: "opencode",
        interactive_binary: Some("opencode"),
        routing: Routing::OpencodeConfig,
    },
    Harness {
        id: "pi-acp",
        description: "pi coding agent via `pi-acp` (needs `pi` on PATH)",
        project_url: "https://github.com/svkozak/pi-acp",
        acp_command: "npx",
        acp_args: &["-y", "pi-acp@latest"],
        package_marker: "pi-acp",
        interactive_binary: Some("pi"),
        routing: Routing::PiConfigDir,
    },
];

/// Look up a harness by its catalog id.
pub fn by_id(id: &str) -> Option<&'static Harness> {
    CATALOG.iter().find(|h| h.id == id)
}

/// Look up a harness by its interactive binary name (`launch` uses this to
/// share the ACP-side routing knowledge).
pub fn by_interactive_binary(binary: &str) -> Option<&'static Harness> {
    CATALOG
        .iter()
        .find(|h| h.interactive_binary == Some(binary))
}

/// Match a configured agent's invocation back to a catalog harness by its
/// [`package_marker`](Harness::package_marker), so routing follows the
/// *invocation*, not the user-chosen YAML key. Checks the command and every
/// arg for the marker substring.
pub fn match_invocation(command: &str, args: &[String]) -> Option<&'static Harness> {
    CATALOG.iter().find(|h| {
        command.contains(h.package_marker) || args.iter().any(|a| a.contains(h.package_marker))
    })
}

impl Harness {
    /// Compute the child-process overlay that routes this harness's LLM
    /// traffic through `base_url`, authenticating with `auth` (already
    /// resolved by precedence — see [`resolve_gateway_auth`]). `model` pins
    /// the model when the harness supports it. Returns an empty overlay for
    /// an [`Unroutable`](Routing::Unroutable) harness (the caller warns).
    pub fn routing_overlay(
        &self,
        base_url: &str,
        auth: &str,
        model: Option<&str>,
    ) -> RoutingOverlay {
        match &self.routing {
            Routing::Env {
                base_url_env,
                auth_env,
                model_env,
                extra,
                ..
            } => {
                let mut env = vec![
                    ((*base_url_env).to_string(), base_url.to_string()),
                    ((*auth_env).to_string(), auth.to_string()),
                ];
                for (k, v) in *extra {
                    env.push(((*k).to_string(), (*v).to_string()));
                }
                if let (Some(var), Some(m)) = (model_env, model) {
                    env.push(((*var).to_string(), m.to_string()));
                }
                RoutingOverlay {
                    env,
                    args: Vec::new(),
                }
            }
            Routing::CodexArgs => codex_overlay(base_url, auth, model),
            // Config-synthesis harnesses have no pure env/args overlay —
            // callers that can't synthesize launch direct (and say so).
            Routing::OpencodeConfig | Routing::PiConfigDir => RoutingOverlay::default(),
        }
    }

    /// Whether this harness routes through pure env/args injection (the
    /// headless-spawn facet). Config-synthesis harnesses (opencode, pi)
    /// route only through [`Self::orchestrator_overlay`] — headless callers
    /// launch them direct with a note.
    pub fn env_args_routable(&self) -> bool {
        matches!(self.routing, Routing::Env { .. } | Routing::CodexArgs)
    }

    /// Whether the harness sends its gateway credential as `Authorization:
    /// Bearer` (BitRouter's inbound scheme). `false` means a provider-native
    /// header the daemon rejects under `skip_auth: false` (gemini). Codex's
    /// `env_key` path is Bearer, so non-`Env` routings are bearer-compatible.
    pub fn auth_is_bearer(&self) -> bool {
        match self.routing {
            Routing::Env { bearer_auth, .. } => bearer_auth,
            _ => true,
        }
    }

    /// Whether `--model` can be applied to this harness on the pure
    /// env/args path. Config-synthesis harnesses pin their model through
    /// [`Self::orchestrator_overlay`] instead.
    pub fn supports_model_pin(&self) -> bool {
        match self.routing {
            Routing::Env { model_env, .. } => model_env.is_some(),
            Routing::CodexArgs => true,
            Routing::OpencodeConfig | Routing::PiConfigDir => false,
        }
    }

    /// Interactive-orchestrator overlay (TUI_SPEC §2): the routing overlay
    /// plus MCP-bridge injection, synthesizing config files under
    /// `state_dir` for the config-routed harnesses (opencode, pi).
    ///
    /// `catalog` is the daemon's advertised model ids — it fills the
    /// synthesized providers' model lists so the harness's own model picker
    /// shows what the daemon can serve. `model` pins the model (and becomes
    /// the synthesized default); when absent, the first catalog entry is
    /// the default. `mcp` is the bridge to inject where the harness has an
    /// MCP mechanism — pi has none, so its orchestrator runs without fleet
    /// tools.
    pub fn orchestrator_overlay(
        &self,
        base_url: &str,
        auth: &str,
        model: Option<&str>,
        catalog: &[String],
        mcp: Option<&McpServer>,
        state_dir: &std::path::Path,
    ) -> anyhow::Result<RoutingOverlay> {
        match self.id {
            // Claude Code loads extra MCP servers from `--mcp-config <file>`.
            "claude-acp" => {
                let mut overlay = self.routing_overlay(base_url, auth, model);
                if let Some(mcp) = mcp {
                    std::fs::create_dir_all(state_dir)
                        .with_context(|| format!("creating {}", state_dir.display()))?;
                    let path = state_dir.join("fleet-mcp.json");
                    let config = serde_json::json!({
                        "mcpServers": {
                            &mcp.name: { "command": mcp.command, "args": mcp.args }
                        }
                    });
                    std::fs::write(&path, serde_json::to_string_pretty(&config)?)
                        .context("writing claude MCP config")?;
                    overlay
                        .args
                        .extend(["--mcp-config".to_string(), path.display().to_string()]);
                }
                Ok(overlay)
            }
            // codex takes MCP servers as `-c mcp_servers.*` TOML overrides.
            "codex-acp" => {
                let mut overlay = self.routing_overlay(base_url, auth, model);
                if let Some(mcp) = mcp {
                    let items: Vec<String> = mcp.args.iter().map(|a| toml_string(a)).collect();
                    overlay.args.extend([
                        "-c".to_string(),
                        format!(
                            "mcp_servers.{}.command={}",
                            mcp.name,
                            toml_string(&mcp.command)
                        ),
                        "-c".to_string(),
                        format!("mcp_servers.{}.args=[{}]", mcp.name, items.join(",")),
                    ]);
                }
                Ok(overlay)
            }
            // opencode: one synthesized JSON config carries the provider,
            // the default model, and the MCP bridge; OPENCODE_CONFIG points
            // at it.
            "opencode" => {
                std::fs::create_dir_all(state_dir)
                    .with_context(|| format!("creating {}", state_dir.display()))?;
                let path = state_dir.join("opencode.json");
                let config = opencode_config(base_url, auth, model, catalog, mcp);
                std::fs::write(&path, serde_json::to_string_pretty(&config)?)
                    .context("writing opencode config")?;
                Ok(RoutingOverlay {
                    env: vec![("OPENCODE_CONFIG".to_string(), path.display().to_string())],
                    args: Vec::new(),
                })
            }
            // pi: synthesize `models.json` in a dir, point
            // PI_CODING_AGENT_DIR at it, and select provider/model by flag
            // (SPAWN_SPEC §6.4). No MCP mechanism — `mcp` is ignored.
            "pi-acp" => {
                let dir = state_dir.join("pi-agent");
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                let mut models: Vec<serde_json::Value> = catalog
                    .iter()
                    .map(|id| serde_json::json!({ "id": id }))
                    .collect();
                if let Some(m) = model
                    && !catalog.iter().any(|id| id == m)
                {
                    models.push(serde_json::json!({ "id": m }));
                }
                let config = serde_json::json!({
                    "providers": {
                        "bitrouter": {
                            "name": "BitRouter",
                            "baseUrl": v1_base_url(base_url),
                            "api": "openai-completions",
                            "apiKey": auth,
                            "models": models,
                        }
                    }
                });
                std::fs::write(
                    dir.join("models.json"),
                    serde_json::to_string_pretty(&config)?,
                )
                .context("writing pi models.json")?;
                let mut args = Vec::new();
                // Select the routed provider only when it has a model to
                // offer; otherwise pi falls back to its own defaults.
                if let Some(default) = model
                    .map(str::to_string)
                    .or_else(|| catalog.first().cloned())
                {
                    args.extend([
                        "--provider".to_string(),
                        "bitrouter".to_string(),
                        "--model".to_string(),
                        default,
                    ]);
                }
                Ok(RoutingOverlay {
                    env: vec![("PI_CODING_AGENT_DIR".to_string(), dir.display().to_string())],
                    args,
                })
            }
            // Unknown interactive harness: routing only, no MCP mechanism.
            _ => Ok(self.routing_overlay(base_url, auth, model)),
        }
    }
}

/// A stdio MCP server to inject into an orchestrator harness (the TUI's
/// fleet bridge).
#[derive(Debug, Clone)]
pub struct McpServer {
    /// Server name as the harness will list it (e.g. `bitrouter_fleet`).
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

/// The synthesized opencode config: a `bitrouter` provider over the OpenAI
/// wire, the daemon's catalog as its model list, an optional default model,
/// and the MCP bridge.
fn opencode_config(
    base_url: &str,
    auth: &str,
    model: Option<&str>,
    catalog: &[String],
    mcp: Option<&McpServer>,
) -> serde_json::Value {
    let mut models = serde_json::Map::new();
    for id in catalog {
        models.insert(id.clone(), serde_json::json!({}));
    }
    if let Some(m) = model {
        models
            .entry(m.to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    let mut config = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "bitrouter": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "BitRouter",
                "options": { "baseURL": v1_base_url(base_url), "apiKey": auth },
                "models": serde_json::Value::Object(models),
            }
        },
    });
    if let Some(default) = model
        .map(str::to_string)
        .or_else(|| catalog.first().cloned())
    {
        config["model"] = serde_json::json!(format!("bitrouter/{default}"));
    }
    if let Some(mcp) = mcp {
        let mut command = vec![mcp.command.clone()];
        command.extend(mcp.args.iter().cloned());
        config["mcp"] = serde_json::json!({
            &mcp.name: { "type": "local", "command": command, "enabled": true }
        });
    }
    config
}

/// Resolve the gateway credential by precedence: a real `BITROUTER_API_KEY`
/// (`brk_…`) when exported, else the local placeholder. Returns `None` only
/// when `require_key` is set (daemon auth is on) and no real key is present —
/// the caller then fails fast (`SPAWN_SPEC` §5.4).
pub fn resolve_gateway_auth(bitrouter_key: Option<String>, require_key: bool) -> Option<String> {
    match bitrouter_key {
        Some(key) => Some(key),
        None if require_key => None,
        None => Some(PLACEHOLDER_API_KEY.to_string()),
    }
}

/// The Codex `-c` provider-override overlay. Mirrors the interactive
/// `bitrouter launch` codex wiring so both facets route identically.
fn codex_overlay(base_url: &str, auth: &str, model: Option<&str>) -> RoutingOverlay {
    let mut env = Vec::new();
    let mut args = vec![
        "-c".to_string(),
        codex_config_string("model_provider", "bitrouter"),
        "-c".to_string(),
        codex_config_string("model_providers.bitrouter.name", "BitRouter"),
        "-c".to_string(),
        codex_config_string("model_providers.bitrouter.base_url", &v1_base_url(base_url)),
        "-c".to_string(),
        codex_config_string("model_providers.bitrouter.wire_api", "responses"),
    ];

    // Real key → env_key indirection (keeps the secret out of argv/process
    // listing); placeholder → inline experimental_bearer_token.
    if auth == PLACEHOLDER_API_KEY {
        args.push("-c".to_string());
        args.push(codex_config_string(
            "model_providers.bitrouter.experimental_bearer_token",
            PLACEHOLDER_API_KEY,
        ));
    } else {
        env.push((BITROUTER_API_KEY_ENV.to_string(), auth.to_string()));
        args.push("-c".to_string());
        args.push(codex_config_string(
            "model_providers.bitrouter.env_key",
            BITROUTER_API_KEY_ENV,
        ));
    }

    if let Some(m) = model {
        args.push("-c".to_string());
        args.push(codex_config_string("model", m));
    }

    RoutingOverlay { env, args }
}

/// `/v1`-suffixed base URL — the shape codex custom providers, opencode's
/// openai-compatible provider, and pi's `baseUrl` all expect.
fn v1_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn codex_config_string(key: &str, value: &str) -> String {
    format!("{key}={}", toml_string(value))
}

/// Quote a value as a TOML basic string for a `-c key=value` override.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_unique() {
        let mut ids: Vec<_> = CATALOG.iter().map(|h| h.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "catalog ids must be unique");
    }

    #[test]
    fn by_interactive_binary_finds_the_orchestrator_harnesses() {
        assert_eq!(by_interactive_binary("claude").unwrap().id, "claude-acp");
        assert_eq!(by_interactive_binary("codex").unwrap().id, "codex-acp");
        assert_eq!(by_interactive_binary("opencode").unwrap().id, "opencode");
        assert_eq!(by_interactive_binary("pi").unwrap().id, "pi-acp");
        assert!(by_interactive_binary("gemini").is_none());
    }

    #[test]
    fn match_invocation_maps_renamed_key_by_package_marker() {
        // A user-renamed entry ("my-claude") still maps to claude-acp routing.
        let h = match_invocation(
            "npx",
            &[
                "-y".to_string(),
                "@zed-industries/claude-code-acp@latest".to_string(),
            ],
        )
        .expect("matches claude-acp");
        assert_eq!(h.id, "claude-acp");
    }

    #[test]
    fn match_invocation_none_for_unknown_command() {
        assert!(match_invocation("./my-custom-agent", &[]).is_none());
    }

    #[test]
    fn match_invocation_matches_globally_installed_binary_command() {
        // A `command: claude-code-acp` (npm -g binary) must catalog-match the
        // same as the `npx @zed-industries/claude-code-acp` package form.
        assert_eq!(
            match_invocation("claude-code-acp", &[]).unwrap().id,
            "claude-acp"
        );
        assert_eq!(
            match_invocation("gemini-cli", &[]).unwrap().id,
            "gemini-cli"
        );
        assert_eq!(match_invocation("codex-acp", &[]).unwrap().id, "codex-acp");
    }

    #[test]
    fn catalog_markers_are_mutually_non_substrings() {
        // A false-positive would mis-route one harness as another.
        for a in CATALOG {
            for b in CATALOG {
                if a.id != b.id {
                    assert!(
                        !a.package_marker.contains(b.package_marker),
                        "marker '{}' contains '{}'",
                        a.package_marker,
                        b.package_marker
                    );
                }
            }
        }
    }

    #[test]
    fn auth_is_bearer_flags_gemini_as_non_bearer() {
        assert!(by_id("claude-acp").unwrap().auth_is_bearer());
        assert!(by_id("codex-acp").unwrap().auth_is_bearer());
        assert!(!by_id("gemini-cli").unwrap().auth_is_bearer());
    }

    #[test]
    fn supports_model_pin_reflects_catalog() {
        assert!(by_id("claude-acp").unwrap().supports_model_pin());
        assert!(by_id("codex-acp").unwrap().supports_model_pin());
        assert!(by_id("gemini-cli").unwrap().supports_model_pin());
        assert!(!by_id("pi-acp").unwrap().supports_model_pin());
    }

    #[test]
    fn claude_overlay_sets_base_url_and_bearer_token() {
        let h = by_id("claude-acp").unwrap();
        let o = h.routing_overlay("http://127.0.0.1:4356", "brk_real", None);
        assert!(o.env.contains(&(
            "ANTHROPIC_BASE_URL".to_string(),
            "http://127.0.0.1:4356".to_string()
        )));
        assert!(
            o.env
                .contains(&("ANTHROPIC_AUTH_TOKEN".to_string(), "brk_real".to_string()))
        );
        // Never the x-api-key var.
        assert!(o.env.iter().all(|(k, _)| k != "ANTHROPIC_API_KEY"));
        assert!(o.args.is_empty());
    }

    #[test]
    fn claude_overlay_pins_model_when_given() {
        let h = by_id("claude-acp").unwrap();
        let o = h.routing_overlay("http://x:1", "t", Some("claude-sonnet-5"));
        assert!(
            o.env
                .contains(&("ANTHROPIC_MODEL".to_string(), "claude-sonnet-5".to_string()))
        );
        // Absent when no model requested.
        let o2 = h.routing_overlay("http://x:1", "t", None);
        assert!(o2.env.iter().all(|(k, _)| k != "ANTHROPIC_MODEL"));
    }

    #[test]
    fn codex_overlay_routes_responses_to_v1_and_uses_env_key_for_real_key() {
        let h = by_id("codex-acp").unwrap();
        let o = h.routing_overlay("http://127.0.0.1:4356", "brk_real", None);
        assert!(o.args.contains(&"model_provider=\"bitrouter\"".to_string()));
        assert!(o.args.contains(
            &"model_providers.bitrouter.base_url=\"http://127.0.0.1:4356/v1\"".to_string()
        ));
        assert!(
            o.args
                .contains(&"model_providers.bitrouter.wire_api=\"responses\"".to_string())
        );
        assert!(
            o.args
                .contains(&"model_providers.bitrouter.env_key=\"BITROUTER_API_KEY\"".to_string())
        );
        assert!(
            o.env
                .contains(&("BITROUTER_API_KEY".to_string(), "brk_real".to_string()))
        );
        assert!(
            o.args
                .iter()
                .all(|a| !a.contains("experimental_bearer_token"))
        );
    }

    #[test]
    fn codex_overlay_uses_inline_token_for_placeholder() {
        let h = by_id("codex-acp").unwrap();
        let o = h.routing_overlay("http://127.0.0.1:4356", PLACEHOLDER_API_KEY, None);
        assert!(o.args.contains(
            &"model_providers.bitrouter.experimental_bearer_token=\"bitrouter-local\"".to_string()
        ));
        assert!(o.env.iter().all(|(k, _)| k != "BITROUTER_API_KEY"));
    }

    #[test]
    fn codex_overlay_pins_model() {
        let h = by_id("codex-acp").unwrap();
        let o = h.routing_overlay("http://x:1", PLACEHOLDER_API_KEY, Some("gpt-5.2"));
        assert!(o.args.contains(&"model=\"gpt-5.2\"".to_string()));
    }

    #[test]
    fn config_synthesis_harnesses_have_no_pure_overlay() {
        for id in ["pi-acp", "opencode"] {
            let h = by_id(id).unwrap();
            assert!(!h.env_args_routable(), "{id} routes by synthesis only");
            assert_eq!(
                h.routing_overlay("http://x:1", "t", None),
                RoutingOverlay::default(),
                "{id}"
            );
        }
        assert!(by_id("claude-acp").unwrap().env_args_routable());
        assert!(by_id("codex-acp").unwrap().env_args_routable());
    }

    // ── Orchestrator overlays (TUI facet). ──

    fn mcp() -> McpServer {
        McpServer {
            name: "bitrouter_fleet".into(),
            command: "/bin/bitrouter".into(),
            args: vec![
                "mcp".into(),
                "serve".into(),
                "--backend".into(),
                "fleet".into(),
            ],
        }
    }

    #[test]
    fn claude_orchestrator_overlay_writes_mcp_config_and_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("claude-acp").unwrap();
        let o = h
            .orchestrator_overlay("http://x:1", "t", None, &[], Some(&mcp()), dir.path())
            .expect("overlay");
        assert_eq!(o.args[0], "--mcp-config");
        let written = std::fs::read_to_string(&o.args[1]).expect("config written");
        assert!(written.contains("bitrouter_fleet"), "{written}");
        assert!(written.contains("\"fleet\""), "fleet backend: {written}");
        // Routing env comes through unchanged.
        assert!(o.env.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn codex_orchestrator_overlay_appends_mcp_toml_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("codex-acp").unwrap();
        let o = h
            .orchestrator_overlay("http://x:1", "t", None, &[], Some(&mcp()), dir.path())
            .expect("overlay");
        assert!(
            o.args
                .contains(&"mcp_servers.bitrouter_fleet.command=\"/bin/bitrouter\"".to_string()),
            "{:?}",
            o.args
        );
        assert!(
            o.args.contains(
                &"mcp_servers.bitrouter_fleet.args=[\"mcp\",\"serve\",\"--backend\",\"fleet\"]"
                    .to_string()
            ),
            "{:?}",
            o.args
        );
    }

    #[test]
    fn opencode_orchestrator_overlay_synthesizes_one_config_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("opencode").unwrap();
        let catalog = vec!["x-ai/grok-4.5".to_string()];
        let o = h
            .orchestrator_overlay(
                "http://127.0.0.1:4356",
                "tok",
                Some("supergrok:grok-4.5"),
                &catalog,
                Some(&mcp()),
                dir.path(),
            )
            .expect("overlay");
        let (key, path) = &o.env[0];
        assert_eq!(key, "OPENCODE_CONFIG");
        assert!(o.args.is_empty(), "opencode routes purely by config file");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("written")).expect("json");
        assert_eq!(
            config["provider"]["bitrouter"]["options"]["baseURL"],
            "http://127.0.0.1:4356/v1"
        );
        // The pinned model is the default and joins the catalog list.
        assert_eq!(config["model"], "bitrouter/supergrok:grok-4.5");
        assert!(config["provider"]["bitrouter"]["models"]["supergrok:grok-4.5"].is_object());
        assert!(config["provider"]["bitrouter"]["models"]["x-ai/grok-4.5"].is_object());
        // The MCP bridge rides the same file.
        assert_eq!(config["mcp"]["bitrouter_fleet"]["type"], "local");
        assert_eq!(
            config["mcp"]["bitrouter_fleet"]["command"][0],
            "/bin/bitrouter"
        );
    }

    #[test]
    fn opencode_overlay_defaults_model_to_catalog_head() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("opencode").unwrap();
        let catalog = vec!["a/one".to_string(), "b/two".to_string()];
        let o = h
            .orchestrator_overlay("http://x:1", "t", None, &catalog, None, dir.path())
            .expect("overlay");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&o.env[0].1).expect("written"))
                .expect("json");
        assert_eq!(config["model"], "bitrouter/a/one");
        assert!(config.get("mcp").is_none(), "no bridge requested");
    }

    #[test]
    fn pi_orchestrator_overlay_synthesizes_agent_dir_and_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("pi-acp").unwrap();
        let catalog = vec!["x-ai/grok-4.5".to_string()];
        let o = h
            .orchestrator_overlay(
                "http://127.0.0.1:4356",
                "tok",
                Some("supergrok:grok-4.5"),
                &catalog,
                Some(&mcp()), // pi has no MCP mechanism — ignored
                dir.path(),
            )
            .expect("overlay");
        let (key, agent_dir) = &o.env[0];
        assert_eq!(key, "PI_CODING_AGENT_DIR");
        let models: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(std::path::Path::new(agent_dir).join("models.json"))
                .expect("models.json written"),
        )
        .expect("json");
        let provider = &models["providers"]["bitrouter"];
        assert_eq!(provider["baseUrl"], "http://127.0.0.1:4356/v1");
        assert_eq!(provider["api"], "openai-completions");
        assert_eq!(provider["apiKey"], "tok");
        // Catalog + the pinned model, deduped.
        assert_eq!(provider["models"][0]["id"], "x-ai/grok-4.5");
        assert_eq!(provider["models"][1]["id"], "supergrok:grok-4.5");
        assert_eq!(
            o.args,
            vec!["--provider", "bitrouter", "--model", "supergrok:grok-4.5"]
        );
    }

    #[test]
    fn pi_overlay_without_any_model_selects_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("pi-acp").unwrap();
        let o = h
            .orchestrator_overlay("http://x:1", "t", None, &[], None, dir.path())
            .expect("overlay");
        assert!(
            o.args.is_empty(),
            "no routable model — pi keeps its own defaults"
        );
    }

    #[test]
    fn resolve_gateway_auth_precedence() {
        // Real key wins.
        assert_eq!(
            resolve_gateway_auth(Some("brk_x".into()), false).as_deref(),
            Some("brk_x")
        );
        // No key, auth off → placeholder.
        assert_eq!(
            resolve_gateway_auth(None, false).as_deref(),
            Some(PLACEHOLDER_API_KEY)
        );
        // No key, auth required → None (caller fails fast).
        assert_eq!(resolve_gateway_auth(None, true), None);
        // Real key satisfies required auth.
        assert_eq!(
            resolve_gateway_auth(Some("brk_x".into()), true).as_deref(),
            Some("brk_x")
        );
    }
}
