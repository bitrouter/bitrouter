//! Unified coding-agent **harness catalog** — the single source of truth for
//! how BitRouter finds, launches, and routes each supported harness.
//!
//! A harness is drivable in either of two facets:
//!
//! - **interactive** (`bitrouter launch`) — the harness's own native TUI
//!   (`claude`, `codex`, `opencode`, `pi`, `hermes`, `openclaw`, `grok`,
//!   `agy`), launched as a child with its LLM traffic pointed at the daemon —
//!   by env/args where that suffices, otherwise through a synthesized config
//!   file ([`Harness::launch_overlay`]); the human drives it directly.
//! - **ACP** (`bitrouter spawn`) — a headless ACP adapter
//!   (`@agentclientprotocol/claude-agent-acp`, …) driven as a sub-agent by a
//!   program.
//!
//! This module keeps the shared routing knowledge in one catalog. Interactive
//! launches render [`Routing`] through native CLI/config surfaces. Maintained
//! ACP adapters render a [`HarnessEndpointPlan`] through their documented
//! launch fallback and then apply that same plan through ACP provider setup;
//! other adapters retain the catalog's legacy env/args behavior.

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
    /// The ACP adapter invocation (`command` + `args`) for `bitrouter spawn`,
    /// when the harness has one. `None` for interactive-only harnesses
    /// (grok, antigravity) — they have no headless ACP adapter today.
    pub acp_command: Option<&'static str>,
    /// Args passed to [`acp_command`](Self::acp_command).
    pub acp_args: &'static [&'static str],
    /// A substring that identifies this harness inside a configured agent's
    /// invocation (command or any arg) — used to map a user-renamed
    /// `agents:` entry back to its catalog routing (invocation matching, so
    /// the YAML key carries no semantics). Usually the adapter package name.
    pub package_marker: &'static str,
    /// The interactive native-TUI binary for `bitrouter launch`, when the
    /// harness has one. `None` for adapter-only harnesses (gemini).
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
    /// interactive facets synthesize via [`Harness::launch_overlay`].
    OpencodeConfig,
    /// Config-dir synthesis (pi — SPAWN_SPEC §6.4): pi has no base-URL env
    /// var, so routing synthesizes a `models.json` in a per-launch dir and
    /// points `PI_CODING_AGENT_DIR` at it, selecting the provider/model by
    /// CLI flag. Interactive facet only; headless spawn launches direct
    /// with a note.
    PiConfigDir,
    /// Home-dir synthesis (hermes): routing synthesizes a `config.yaml`
    /// (`model.provider: custom` + a loopback `base_url` — hermes trusts
    /// loopback custom endpoints) in a per-launch dir, points `HERMES_HOME`
    /// at it, and passes the credential via `CUSTOM_API_KEY`. The same file
    /// carries the default model and `mcp_servers` (the gateway servers).
    /// Interactive facet only; headless spawn launches direct with a note
    /// (hermes then uses the user's own `~/.hermes` provider).
    HermesHome,
    /// Profile-dir synthesis (openclaw): the interactive facet synthesizes
    /// an isolated profile (`OPENCLAW_STATE_DIR`/`OPENCLAW_CONFIG_PATH`)
    /// whose `openclaw.json` declares a `bitrouter` custom provider and
    /// default model, and runs the embedded runtime (`tui --local`). The
    /// ACP facet (`openclaw acp`) bridges into the user's running gateway,
    /// which owns model routing — headless spawn launches direct with a
    /// note.
    OpenclawProfile,
    /// No gateway redirection, by design: the harness IS a subscription
    /// client whose session the daemon itself borrows as a provider (grok →
    /// `supergrok`, agy → `google-ai`) — routing it through the daemon would
    /// loop back to the same backend on the same credential. It launches
    /// with its own auth; `--model` forwards as the harness's native flag.
    OwnAuth,
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

/// Wire protocol advertised to an ACP adapter through its provider endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessProtocol {
    /// Anthropic Messages-compatible endpoint.
    Anthropic,
    /// OpenAI Responses-compatible endpoint.
    OpenAi,
}

impl HarnessProtocol {
    /// ACP provider protocol identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointFallback {
    Claude,
    Codex,
}

/// Process-scoped BitRouter endpoint configuration for a maintained ACP
/// adapter.
///
/// The controller applies this plan through ACP provider configuration when
/// the adapter advertises that capability. [`Self::fallback_overlay`] renders
/// the same plan through the adapter's documented environment fallback, so a
/// pinned adapter that loses provider support still routes deterministically.
#[derive(Clone, PartialEq, Eq)]
pub struct HarnessEndpointPlan {
    /// Catalog harness id included in every routed request.
    pub harness_id: &'static str,
    /// Exact upstream adapter package.
    pub adapter_package: &'static str,
    /// Exact upstream adapter version.
    pub adapter_version: &'static str,
    /// Adapter-native provider id selected through ACP.
    pub provider_id: &'static str,
    /// Model API protocol exposed by BitRouter for this adapter.
    pub protocol: HarnessProtocol,
    /// Adapter-facing endpoint base URL.
    pub base_url: String,
    /// Optional logical model pin.
    pub model: Option<String>,
    /// HTTP headers passed to the adapter endpoint. Names are canonicalized
    /// to lower case so observability and routing parse one stable shape.
    pub headers: std::collections::BTreeMap<String, String>,
    auth: String,
    fallback: EndpointFallback,
}

impl std::fmt::Debug for HarnessEndpointPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessEndpointPlan")
            .field("harness_id", &self.harness_id)
            .field("adapter_package", &self.adapter_package)
            .field("adapter_version", &self.adapter_version)
            .field("provider_id", &self.provider_id)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("auth", &"[REDACTED]")
            .field("fallback", &self.fallback)
            .finish()
    }
}

impl HarnessEndpointPlan {
    /// Replace only the gateway bearer with a daemon-issued controller
    /// credential. Static controller and harness headers remain unchanged.
    #[must_use]
    pub fn controller_credential(mut self, credential: &str) -> Self {
        self.auth = credential.to_string();
        self.headers
            .insert("authorization".to_string(), format!("Bearer {credential}"));
        self
    }

    /// Render the documented process-environment fallback for this exact
    /// adapter version. No routing configuration is added to adapter argv.
    pub fn fallback_overlay(&self) -> anyhow::Result<RoutingOverlay> {
        let static_headers = self
            .headers
            .iter()
            .filter(|(name, _)| name.as_str() != "authorization")
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();

        match self.fallback {
            EndpointFallback::Claude => {
                let mut env = vec![
                    ("ANTHROPIC_BASE_URL".to_string(), self.base_url.clone()),
                    ("ANTHROPIC_AUTH_TOKEN".to_string(), self.auth.clone()),
                    (
                        "ANTHROPIC_CUSTOM_HEADERS".to_string(),
                        static_headers
                            .iter()
                            .map(|(name, value)| format!("{name}: {value}"))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                ];
                if let Some(model) = &self.model {
                    env.push(("ANTHROPIC_MODEL".to_string(), model.clone()));
                }
                Ok(RoutingOverlay {
                    env,
                    args: Vec::new(),
                })
            }
            EndpointFallback::Codex => {
                let provider = serde_json::json!({
                    "name": "BitRouter",
                    "base_url": self.base_url,
                    "wire_api": "responses",
                    "env_key": BITROUTER_API_KEY_ENV,
                    "http_headers": static_headers,
                });
                let mut config = serde_json::json!({
                    "model_provider": "bitrouter",
                    "model_providers": { "bitrouter": provider },
                });
                if let Some(model) = &self.model {
                    config["model"] = serde_json::Value::String(model.clone());
                }
                let encoded = serde_json::to_string(&config)
                    .context("serializing the Codex ACP endpoint fallback")?;
                Ok(RoutingOverlay {
                    env: vec![
                        (BITROUTER_API_KEY_ENV.to_string(), self.auth.clone()),
                        ("CODEX_CONFIG".to_string(), encoded),
                        ("MODEL_PROVIDER".to_string(), "bitrouter".to_string()),
                    ],
                    args: Vec::new(),
                })
            }
        }
    }
}

/// The bundled catalog. Limited to publicly-available, actively-maintained
/// harnesses. Extend by PR to bitrouter.
pub const CATALOG: &[Harness] = &[
    Harness {
        id: "claude-acp",
        description: "Anthropic Claude via the maintained Claude Agent ACP adapter",
        project_url: "https://github.com/agentclientprotocol/claude-agent-acp",
        acp_command: Some("npx"),
        acp_args: &["-y", "@agentclientprotocol/claude-agent-acp@0.70.0"],
        // A substring of both the pinned npm spec and the globally-installed
        // binary name, so either invocation catalog-matches.
        package_marker: "claude-agent-acp",
        interactive_binary: Some("claude"),
        // claude-agent-acp passes process env through to the SDK-spawned CLI,
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
        description: "OpenAI Codex via the Agent Client Protocol adapter",
        project_url: "https://github.com/agentclientprotocol/codex-acp",
        acp_command: Some("npx"),
        acp_args: &["-y", "@agentclientprotocol/codex-acp@1.7.0"],
        package_marker: "codex-acp",
        interactive_binary: Some("codex"),
        routing: Routing::CodexArgs,
    },
    Harness {
        id: "gemini-cli",
        description: "Google's Gemini CLI with `--experimental-acp` (best-effort routing)",
        project_url: "https://github.com/google-gemini/gemini-cli",
        acp_command: Some("npx"),
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
        acp_command: Some("opencode"),
        acp_args: &["acp"],
        package_marker: "opencode",
        interactive_binary: Some("opencode"),
        routing: Routing::OpencodeConfig,
    },
    Harness {
        id: "pi-acp",
        description: "pi coding agent via `pi-acp` (needs `pi` on PATH)",
        project_url: "https://github.com/svkozak/pi-acp",
        acp_command: Some("npx"),
        acp_args: &["-y", "pi-acp@latest"],
        package_marker: "pi-acp",
        interactive_binary: Some("pi"),
        routing: Routing::PiConfigDir,
    },
    Harness {
        id: "hermes-acp",
        description: "Nous Research's Hermes Agent via its native `hermes acp`",
        project_url: "https://github.com/NousResearch/hermes-agent",
        acp_command: Some("hermes"),
        acp_args: &["acp"],
        package_marker: "hermes",
        interactive_binary: Some("hermes"),
        routing: Routing::HermesHome,
    },
    Harness {
        id: "openclaw",
        description: "OpenClaw assistant via its gateway ACP bridge `openclaw acp`",
        project_url: "https://github.com/openclaw/openclaw",
        acp_command: Some("openclaw"),
        acp_args: &["acp"],
        package_marker: "openclaw",
        interactive_binary: Some("openclaw"),
        routing: Routing::OpenclawProfile,
    },
    Harness {
        id: "grok",
        description: "xAI's Grok CLI (interactive only; own SuperGrok auth)",
        project_url: "https://x.ai/",
        acp_command: None,
        acp_args: &[],
        package_marker: "grok-cli",
        interactive_binary: Some("grok"),
        routing: Routing::OwnAuth,
    },
    Harness {
        id: "antigravity",
        description: "Google's Antigravity CLI `agy` (interactive only; own Google auth)",
        project_url: "https://antigravity.google/",
        acp_command: None,
        acp_args: &[],
        package_marker: "antigravity-cli",
        interactive_binary: Some("agy"),
        routing: Routing::OwnAuth,
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
    let catalog_match = CATALOG.iter().find(|h| {
        command.contains(h.package_marker) || args.iter().any(|a| a.contains(h.package_marker))
    });
    catalog_match.or_else(|| {
        let legacy_claude = command.contains("claude-code-acp")
            || args.iter().any(|arg| arg.contains("claude-code-acp"));
        if legacy_claude {
            by_id("claude-acp")
        } else {
            None
        }
    })
}

impl Harness {
    /// Exact maintained ACP adapter identity for controller diagnostics.
    ///
    /// `None` means the catalog entry has no pinned, provider-configurable ACP
    /// adapter contract yet. Callers must identify such invocations as
    /// configured/custom rather than attributing them to a maintained pin.
    pub fn maintained_adapter_identity(&self) -> Option<(&'static str, &'static str)> {
        match self.id {
            "claude-acp" => Some(("@agentclientprotocol/claude-agent-acp", "0.70.0")),
            "codex-acp" => Some(("@agentclientprotocol/codex-acp", "1.7.0")),
            _ => None,
        }
    }

    /// Whether a configured invocation uses this catalog entry's exact
    /// maintained adapter pin.
    pub fn uses_maintained_adapter(&self, command: &str, args: &[String]) -> bool {
        let Some((package, version)) = self.maintained_adapter_identity() else {
            return false;
        };
        let exact = format!("{package}@{version}");
        command == exact || args.iter().any(|arg| arg == &exact)
    }

    /// Build the controller's process-scoped endpoint plan for a maintained
    /// ACP adapter. Other catalog entries keep their existing launch routing
    /// until their provider contracts are pinned and tested.
    pub fn endpoint_plan(
        &self,
        base_url: &str,
        auth: &str,
        model: Option<&str>,
        controller_id: &str,
    ) -> Option<HarnessEndpointPlan> {
        let mut headers = std::collections::BTreeMap::from([
            ("authorization".to_string(), format!("Bearer {auth}")),
            (
                "x-bitrouter-controller-id".to_string(),
                controller_id.to_string(),
            ),
            ("x-bitrouter-harness".to_string(), self.id.to_string()),
        ]);
        // Keep construction explicit: any future header added here becomes a
        // reviewed part of the router/controller contract.
        headers.retain(|name, value| !name.is_empty() && !value.is_empty());

        let (adapter_package, adapter_version) = self.maintained_adapter_identity()?;
        let (provider_id, protocol, base_url, fallback) = match self.id {
            "claude-acp" => (
                "main",
                HarnessProtocol::Anthropic,
                base_url.trim_end_matches('/').to_string(),
                EndpointFallback::Claude,
            ),
            "codex-acp" => (
                "openai",
                HarnessProtocol::OpenAi,
                v1_base_url(base_url),
                EndpointFallback::Codex,
            ),
            _ => return None,
        };

        Some(HarnessEndpointPlan {
            harness_id: self.id,
            adapter_package,
            adapter_version,
            provider_id,
            protocol,
            base_url,
            model: model.map(str::to_string),
            headers,
            auth: auth.to_string(),
            fallback,
        })
    }

    /// Compute the child-process overlay that routes this harness's LLM
    /// traffic through `base_url`, authenticating with `auth` (already
    /// resolved by precedence — see [`resolve_gateway_auth`]). `model` pins
    /// the model when the harness supports it. Returns an empty overlay for
    /// harnesses env/args can't route ([`OpencodeConfig`](Routing::OpencodeConfig)
    /// / [`PiConfigDir`](Routing::PiConfigDir) / [`OwnAuth`](Routing::OwnAuth)
    /// — the caller warns and runs direct; see [`env_args_routable`](Self::env_args_routable)).
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
            // callers that can't synthesize launch direct (and say so) —
            // and own-auth harnesses are never redirected at all.
            Routing::OpencodeConfig
            | Routing::PiConfigDir
            | Routing::HermesHome
            | Routing::OpenclawProfile
            | Routing::OwnAuth => RoutingOverlay::default(),
        }
    }

    /// Pin only the model while preserving the agent's own provider and
    /// subscription authority. Used by explicitly direct ACP sessions.
    pub fn direct_model_overlay(&self, model: &str) -> RoutingOverlay {
        match &self.routing {
            Routing::Env {
                model_env: Some(model_env),
                ..
            } => RoutingOverlay {
                env: vec![((*model_env).to_string(), model.to_string())],
                args: Vec::new(),
            },
            Routing::CodexArgs => RoutingOverlay {
                env: Vec::new(),
                args: vec!["-c".to_string(), codex_config_string("model", model)],
            },
            Routing::Env {
                model_env: None, ..
            }
            | Routing::OpencodeConfig
            | Routing::PiConfigDir
            | Routing::HermesHome
            | Routing::OpenclawProfile
            | Routing::OwnAuth => RoutingOverlay::default(),
        }
    }

    /// Whether this harness routes through pure env/args injection (the
    /// headless-spawn facet). Config-synthesis harnesses (opencode, pi)
    /// route only through [`Self::launch_overlay`] — headless callers launch
    /// them direct
    /// with a note.
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
    /// env/args path (the headless-spawn / direct-pin facet). Config-synthesis
    /// harnesses pin their model through [`Self::launch_overlay`] instead, so
    /// an interactive caller must
    /// not read this as "cannot pin".
    pub fn supports_model_pin(&self) -> bool {
        match self.routing {
            Routing::Env { model_env, .. } => model_env.is_some(),
            Routing::CodexArgs => true,
            Routing::OpencodeConfig
            | Routing::PiConfigDir
            | Routing::HermesHome
            | Routing::OpenclawProfile
            | Routing::OwnAuth => false,
        }
    }

    /// Whether `bitrouter launch` will run this harness: exactly those with an
    /// [`interactive_binary`](Self::interactive_binary).
    ///
    /// The cut to four ids was made when `launch` also promised a *hosted
    /// terminal*, and that promise was the expensive one — an emulator has to
    /// be verified per harness against upstream releases nobody here controls.
    /// With hosting gone `launch` promises only what the overlay already gives
    /// every interactive entry in the catalog: it execs the binary with the
    /// routing overlay applied, and states before handover what the harness
    /// actually got (see [`Self::injects_mcp`], which is a real per-harness
    /// ceiling and stays one).
    ///
    /// `grok` and `agy` are launchable *and* remain **providers** — the daemon
    /// borrows their subscription sessions to serve other requests. Their own
    /// traffic is unrouted and unmetered, which the startup line says.
    pub fn launch_supported(&self) -> bool {
        self.interactive_binary.is_some()
    }

    /// Whether [`Self::launch_overlay`] can inject MCP servers (the
    /// `bitrouter_tools` / `bitrouter_skills` gateways) into this harness.
    ///
    /// This is a *ceiling of the harness*, not of BitRouter: `pi` and
    /// `openclaw` expose no MCP mechanism to inject into, and the own-auth
    /// clients are not routed at all. `launch` states it before handover so a
    /// user does not discover the gap when the tools simply aren't there.
    ///
    /// Kept in lockstep with `launch_overlay` by
    /// `injects_mcp_matches_the_launch_overlay` — the predicate is declarative,
    /// and the test proves it agrees with what the overlay actually does.
    pub fn injects_mcp(&self) -> bool {
        matches!(
            self.id,
            "claude-acp" | "codex-acp" | "opencode" | "hermes-acp"
        )
    }

    /// Interactive-launch overlay (`bitrouter launch`): the full routing
    /// overlay for *any* interactive harness — including the ones env/args
    /// cannot route, whose config files are synthesized under `state_dir`
    /// (opencode's `OPENCODE_CONFIG`, pi's `PI_CODING_AGENT_DIR`, hermes's
    /// `HERMES_HOME`, openclaw's profile) — plus injection of the `mcp`
    /// gateway servers, where the harness has an MCP mechanism.
    ///
    /// `catalog` is the daemon's advertised model ids; it fills the
    /// synthesized providers' model lists so the harness's own model picker
    /// shows what the daemon can serve. `model` pins the model (and becomes
    /// the synthesized default); when absent, the first catalog entry is the
    /// default. Own-auth harnesses (grok, agy) are never redirected — they
    /// only get `--model` as their native flag.
    ///
    /// MCP injection rides the *same* synthesized artifact as routing for the
    /// config-file harnesses (one `OPENCODE_CONFIG` JSON, one
    /// `HERMES_HOME/config.yaml`), so the two facets cannot be layered into
    /// separate methods — an empty `mcp` is simply a launch with no gateways.
    /// `pi`, `openclaw`, `grok`, and `antigravity` have no injectable MCP
    /// mechanism at all and ignore `mcp` entirely.
    pub fn launch_overlay(
        &self,
        base_url: &str,
        auth: &str,
        model: Option<&str>,
        catalog: &[String],
        mcp: &[McpServer],
        state_dir: &std::path::Path,
    ) -> anyhow::Result<RoutingOverlay> {
        match self.id {
            // Claude Code loads extra MCP servers from `--mcp-config <file>`.
            // Stdio entries are `{command, args}`; HTTP entries need an
            // explicit `"type": "http"` (a `url` without `type` is a hard
            // error) plus a `headers` object.
            "claude-acp" => {
                let mut overlay = self.routing_overlay(base_url, auth, model);
                if !mcp.is_empty() {
                    std::fs::create_dir_all(state_dir)
                        .with_context(|| format!("creating {}", state_dir.display()))?;
                    let path = state_dir.join("mcp.json");
                    let mut servers = serde_json::Map::new();
                    for s in mcp {
                        let entry = match &s.transport {
                            McpTransport::Stdio { command, args } => {
                                serde_json::json!({ "command": command, "args": args })
                            }
                            McpTransport::Http { url, headers } => serde_json::json!({
                                "type": "http",
                                "url": url,
                                "headers": headers_map(headers),
                            }),
                        };
                        servers.insert(s.name.clone(), entry);
                    }
                    let config = serde_json::json!({ "mcpServers": servers });
                    std::fs::write(&path, serde_json::to_string_pretty(&config)?)
                        .context("writing claude MCP config")?;
                    overlay
                        .args
                        .extend(["--mcp-config".to_string(), path.display().to_string()]);
                }
                Ok(overlay)
            }
            // codex takes MCP servers as `-c mcp_servers.*` TOML overrides.
            // `url` presence selects the streamable-HTTP transport (codex
            // 0.144+); auth rides `http_headers.<Name>` overrides.
            "codex-acp" => {
                let mut overlay = self.routing_overlay(base_url, auth, model);
                for s in mcp {
                    match &s.transport {
                        McpTransport::Stdio { command, args } => {
                            let items: Vec<String> = args.iter().map(|a| toml_string(a)).collect();
                            overlay.args.extend([
                                "-c".to_string(),
                                format!("mcp_servers.{}.command={}", s.name, toml_string(command)),
                                "-c".to_string(),
                                format!("mcp_servers.{}.args=[{}]", s.name, items.join(",")),
                            ]);
                        }
                        McpTransport::Http { url, headers } => {
                            overlay.args.extend([
                                "-c".to_string(),
                                format!("mcp_servers.{}.url={}", s.name, toml_string(url)),
                            ]);
                            for (name, value) in headers {
                                overlay.args.extend([
                                    "-c".to_string(),
                                    format!(
                                        "mcp_servers.{}.http_headers.{}={}",
                                        s.name,
                                        name,
                                        toml_string(value)
                                    ),
                                ]);
                            }
                        }
                    }
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
            // hermes: synthesize an isolated `HERMES_HOME` whose config.yaml
            // routes via a `custom` loopback provider (hermes trusts loopback
            // custom endpoints) and carries the injected MCP servers; the
            // credential rides `CUSTOM_API_KEY`. The file is written as JSON
            // — hermes parses config.yaml with a YAML 1.2 loader, and JSON
            // is a YAML subset — so no YAML serializer dependency is needed.
            "hermes-acp" => {
                let dir = state_dir.join("hermes");
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                let default = model
                    .map(str::to_string)
                    .or_else(|| catalog.first().cloned());
                let mut config = serde_json::json!({
                    "model": {
                        "provider": "custom",
                        "base_url": v1_base_url(base_url),
                    }
                });
                if let Some(default) = default {
                    config["model"]["default"] = serde_json::Value::String(default);
                }
                if !mcp.is_empty() {
                    let mut entries = serde_json::Map::new();
                    for s in mcp {
                        let entry = match &s.transport {
                            McpTransport::Stdio { command, args } => {
                                serde_json::json!({ "command": command, "args": args })
                            }
                            // hermes selects the HTTP transport by `url`
                            // presence.
                            McpTransport::Http { url, headers } => serde_json::json!({
                                "url": url,
                                "headers": headers_map(headers),
                            }),
                        };
                        entries.insert(s.name.clone(), entry);
                    }
                    config["mcp_servers"] = serde_json::Value::Object(entries);
                }
                std::fs::write(
                    dir.join("config.yaml"),
                    serde_json::to_string_pretty(&config)?,
                )
                .context("writing hermes config.yaml")?;
                Ok(RoutingOverlay {
                    env: vec![
                        ("HERMES_HOME".to_string(), dir.display().to_string()),
                        ("CUSTOM_API_KEY".to_string(), auth.to_string()),
                    ],
                    args: Vec::new(),
                })
            }
            // openclaw: synthesize an isolated profile (state dir + config)
            // whose `openclaw.json` declares a `bitrouter` custom provider,
            // and run the embedded local runtime (`tui --local` — no gateway
            // needed). Model entries need the full schema (name/cost/window)
            // or config validation rejects the file. No MCP injection yet —
            // openclaw's MCP surface is gateway-scoped.
            "openclaw" => {
                let dir = state_dir.join("openclaw");
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                let ids: Vec<&str> = model
                    .into_iter()
                    .chain(catalog.iter().map(String::as_str))
                    .collect();
                let models: Vec<serde_json::Value> = ids
                    .iter()
                    .map(|id| {
                        serde_json::json!({
                            "id": id,
                            "name": id,
                            "reasoning": false,
                            "input": ["text"],
                            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                            "contextWindow": 200000,
                            "maxTokens": 8192,
                        })
                    })
                    .collect();
                let mut config = serde_json::json!({
                    "gateway": { "mode": "local" },
                    "models": {
                        "mode": "merge",
                        "providers": {
                            "bitrouter": {
                                "baseUrl": v1_base_url(base_url),
                                "apiKey": auth,
                                "api": "openai-completions",
                                "models": models,
                            }
                        }
                    }
                });
                if let Some(default) = ids.first() {
                    config["agents"] = serde_json::json!({
                        "defaults": { "model": format!("bitrouter/{default}") }
                    });
                }
                std::fs::write(
                    dir.join("openclaw.json"),
                    serde_json::to_string_pretty(&config)?,
                )
                .context("writing openclaw.json")?;
                Ok(RoutingOverlay {
                    env: vec![
                        ("OPENCLAW_STATE_DIR".to_string(), dir.display().to_string()),
                        (
                            "OPENCLAW_CONFIG_PATH".to_string(),
                            dir.join("openclaw.json").display().to_string(),
                        ),
                    ],
                    args: vec!["tui".to_string(), "--local".to_string()],
                })
            }
            // Own-auth harnesses (grok, agy): no redirection, no MCP
            // mechanism we can inject non-invasively — hosted with their own
            // subscription auth; `--model` forwards as their native flag.
            "grok" => Ok(RoutingOverlay {
                env: Vec::new(),
                args: model
                    .map(|m| vec!["-m".to_string(), m.to_string()])
                    .unwrap_or_default(),
            }),
            "antigravity" => Ok(RoutingOverlay {
                env: Vec::new(),
                args: model
                    .map(|m| vec!["--model".to_string(), m.to_string()])
                    .unwrap_or_default(),
            }),
            // Unknown interactive harness: routing only, no MCP mechanism.
            _ => Ok(self.routing_overlay(base_url, auth, model)),
        }
    }
}

/// An MCP server to inject into a launched harness — the gateway servers
/// (`bitrouter_tools` / `bitrouter_skills`, see `crate::gateways`).
#[derive(Debug, Clone)]
pub struct McpServer {
    /// Server name as the harness will list it (e.g. `bitrouter_tools`).
    pub name: String,
    /// How the harness dials it.
    pub transport: McpTransport,
}

/// How a harness dials an injected MCP server. Headers are an ordered list
/// (not a map) so synthesized configs and `-c` overrides stay deterministic.
#[derive(Debug, Clone)]
pub enum McpTransport {
    /// Spawn `command` with `args` and exchange JSON-RPC over stdio.
    Stdio { command: String, args: Vec<String> },
    /// Streamable HTTP at `url`, sending the static request `headers`
    /// (e.g. `Authorization`) on every call.
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// Render ordered header pairs as the JSON object shape harness config files
/// expect (`{"Authorization": "Bearer …"}`).
fn headers_map(headers: &[(String, String)]) -> serde_json::Map<String, serde_json::Value> {
    headers
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect()
}

/// The synthesized opencode config: a `bitrouter` provider over the OpenAI
/// wire, the daemon's catalog as its model list, an optional default model,
/// and the injected MCP servers.
fn opencode_config(
    base_url: &str,
    auth: &str,
    model: Option<&str>,
    catalog: &[String],
    mcp: &[McpServer],
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
    if !mcp.is_empty() {
        let mut entries = serde_json::Map::new();
        for s in mcp {
            let entry = match &s.transport {
                // opencode folds command+args into one invocation array.
                McpTransport::Stdio { command, args } => {
                    let mut invocation = vec![command.clone()];
                    invocation.extend(args.iter().cloned());
                    serde_json::json!({ "type": "local", "command": invocation, "enabled": true })
                }
                McpTransport::Http { url, headers } => serde_json::json!({
                    "type": "remote",
                    "url": url,
                    "enabled": true,
                    "headers": headers_map(headers),
                }),
            };
            entries.insert(s.name.clone(), entry);
        }
        config["mcp"] = serde_json::Value::Object(entries);
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

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct AdapterContract {
        harness_id: String,
        package: String,
        version: String,
        git_head: String,
        provider_id: String,
        protocol: String,
        fallback_env: Vec<String>,
        native_identity_headers: Vec<String>,
    }

    fn adapter_contracts() -> anyhow::Result<Vec<AdapterContract>> {
        Ok(vec![
            serde_json::from_str(include_str!(
                "../tests/fixtures/acp_adapters/claude-agent-acp-0.70.0.json"
            ))?,
            serde_json::from_str(include_str!(
                "../tests/fixtures/acp_adapters/codex-acp-1.7.0.json"
            ))?,
        ])
    }

    #[test]
    fn catalog_ids_are_unique() {
        let mut ids: Vec<_> = CATALOG.iter().map(|h| h.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "catalog ids must be unique");
    }

    #[test]
    fn maintained_acp_adapters_are_exactly_pinned() -> anyhow::Result<()> {
        let claude = by_id("claude-acp")
            .ok_or_else(|| anyhow::anyhow!("claude-acp missing from catalog"))?;
        assert_eq!(
            claude.acp_args,
            &["-y", "@agentclientprotocol/claude-agent-acp@0.70.0"]
        );
        assert_eq!(
            claude.project_url,
            "https://github.com/agentclientprotocol/claude-agent-acp"
        );
        assert_eq!(claude.package_marker, "claude-agent-acp");

        let codex =
            by_id("codex-acp").ok_or_else(|| anyhow::anyhow!("codex-acp missing from catalog"))?;
        assert_eq!(
            codex.acp_args,
            &["-y", "@agentclientprotocol/codex-acp@1.7.0"]
        );
        Ok(())
    }

    #[test]
    fn maintained_provider_contract_requires_the_exact_invocation_pin() -> anyhow::Result<()> {
        let claude = by_id("claude-acp")
            .ok_or_else(|| anyhow::anyhow!("claude-acp missing from catalog"))?;
        assert!(claude.uses_maintained_adapter(
            "npx",
            &["@agentclientprotocol/claude-agent-acp@0.70.0".to_string()]
        ));
        assert!(!claude.uses_maintained_adapter(
            "npx",
            &["@agentclientprotocol/claude-agent-acp@0.69.0".to_string()]
        ));
        assert!(!claude.uses_maintained_adapter(
            "npx",
            &["@agentclientprotocol/claude-agent-acp@0.70.0-tampered".to_string()]
        ));
        assert!(!claude.uses_maintained_adapter(
            "npx",
            &["@zed-industries/claude-code-acp@latest".to_string()]
        ));
        Ok(())
    }

    #[test]
    fn endpoint_plan_matches_the_pinned_adapter_contracts() -> anyhow::Result<()> {
        for contract in adapter_contracts()? {
            let harness = by_id(&contract.harness_id)
                .ok_or_else(|| anyhow::anyhow!("missing harness {}", contract.harness_id))?;
            let plan = harness
                .endpoint_plan(
                    "http://127.0.0.1:4356/",
                    "brk_test",
                    Some("logical/model"),
                    "controller-test",
                )
                .ok_or_else(|| anyhow::anyhow!("missing endpoint plan"))?;

            assert_eq!(plan.provider_id, contract.provider_id);
            assert_eq!(plan.protocol.as_str(), contract.protocol);
            assert_eq!(plan.model.as_deref(), Some("logical/model"));
            assert_eq!(
                plan.headers.get("authorization").map(String::as_str),
                Some("Bearer brk_test")
            );
            assert_eq!(
                plan.headers
                    .get("x-bitrouter-controller-id")
                    .map(String::as_str),
                Some("controller-test")
            );
            assert_eq!(
                plan.headers.get("x-bitrouter-harness").map(String::as_str),
                Some(contract.harness_id.as_str())
            );

            let package_spec = format!("{}@{}", contract.package, contract.version);
            assert!(harness.acp_args.contains(&package_spec.as_str()));
            assert_eq!(contract.git_head.len(), 40);
            assert!(!contract.native_identity_headers.is_empty());
        }
        Ok(())
    }

    #[test]
    fn endpoint_plan_renders_documented_adapter_fallbacks() -> anyhow::Result<()> {
        for contract in adapter_contracts()? {
            let harness = by_id(&contract.harness_id)
                .ok_or_else(|| anyhow::anyhow!("missing harness {}", contract.harness_id))?;
            let plan = harness
                .endpoint_plan(
                    "http://127.0.0.1:4356",
                    "brk_test",
                    Some("logical/model"),
                    "controller-test",
                )
                .ok_or_else(|| anyhow::anyhow!("missing endpoint plan"))?;
            let overlay = plan.fallback_overlay()?;
            let env: std::collections::HashMap<_, _> = overlay.env.into_iter().collect();

            for key in contract.fallback_env {
                assert!(
                    env.contains_key(&key),
                    "{} missing {key}",
                    contract.harness_id
                );
            }
            assert!(
                overlay.args.is_empty(),
                "{} used ACP CLI args",
                contract.harness_id
            );

            match contract.harness_id.as_str() {
                "claude-acp" => {
                    assert_eq!(
                        env.get("ANTHROPIC_BASE_URL").map(String::as_str),
                        Some("http://127.0.0.1:4356")
                    );
                    let custom = env
                        .get("ANTHROPIC_CUSTOM_HEADERS")
                        .ok_or_else(|| anyhow::anyhow!("missing Claude custom headers"))?;
                    assert!(custom.contains("x-bitrouter-controller-id: controller-test"));
                    assert!(custom.contains("x-bitrouter-harness: claude-acp"));
                    assert!(!custom.to_ascii_lowercase().contains("authorization"));
                }
                "codex-acp" => {
                    assert_eq!(
                        env.get("MODEL_PROVIDER").map(String::as_str),
                        Some("bitrouter")
                    );
                    let config: serde_json::Value = serde_json::from_str(
                        env.get("CODEX_CONFIG")
                            .ok_or_else(|| anyhow::anyhow!("missing Codex config"))?,
                    )?;
                    assert_eq!(config["model_provider"], "bitrouter");
                    assert_eq!(
                        config["model_providers"]["bitrouter"]["base_url"],
                        "http://127.0.0.1:4356/v1"
                    );
                    assert_eq!(
                        config["model_providers"]["bitrouter"]["wire_api"],
                        "responses"
                    );
                    assert_eq!(config["model"], "logical/model");
                }
                other => return Err(anyhow::anyhow!("unexpected contract {other}")),
            }
        }
        Ok(())
    }

    #[test]
    fn by_interactive_binary_finds_the_launchable_harnesses() {
        assert_eq!(by_interactive_binary("claude").unwrap().id, "claude-acp");
        assert_eq!(by_interactive_binary("codex").unwrap().id, "codex-acp");
        assert_eq!(by_interactive_binary("opencode").unwrap().id, "opencode");
        assert_eq!(by_interactive_binary("pi").unwrap().id, "pi-acp");
        assert_eq!(by_interactive_binary("hermes").unwrap().id, "hermes-acp");
        assert_eq!(by_interactive_binary("openclaw").unwrap().id, "openclaw");
        assert_eq!(by_interactive_binary("grok").unwrap().id, "grok");
        assert_eq!(by_interactive_binary("agy").unwrap().id, "antigravity");
        assert!(by_interactive_binary("gemini").is_none());
    }

    #[test]
    fn own_auth_harnesses_never_redirect_and_forward_model_natively() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (id, flag) in [("grok", "-m"), ("antigravity", "--model")] {
            let h = by_id(id).unwrap();
            assert!(!h.env_args_routable(), "{id} launches with its own auth");
            assert!(h.acp_command.is_none(), "{id} has no ACP adapter");
            assert_eq!(
                h.routing_overlay("http://x:1", "t", None),
                RoutingOverlay::default()
            );
            // The launch overlay sets no env (no redirection) and
            // forwards --model as the harness's native flag.
            let o = h
                .launch_overlay(
                    "http://x:1",
                    "t",
                    Some("some-model"),
                    &[],
                    &[mcp()],
                    dir.path(),
                )
                .expect("overlay");
            assert!(o.env.is_empty(), "{id}: no redirection env");
            assert_eq!(o.args, vec![flag, "some-model"], "{id}");
            let bare = h
                .launch_overlay("http://x:1", "t", None, &[], &[], dir.path())
                .expect("overlay");
            assert_eq!(bare, RoutingOverlay::default(), "{id}: bare launch");
        }
    }

    #[test]
    fn match_invocation_maps_renamed_key_by_package_marker() {
        // A user-renamed entry ("my-claude") still maps to claude-acp routing.
        let h = match_invocation(
            "npx",
            &[
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp@0.70.0".to_string(),
            ],
        )
        .expect("matches claude-acp");
        assert_eq!(h.id, "claude-acp");
    }

    #[test]
    fn match_invocation_keeps_legacy_claude_adapter_configs_routable() -> anyhow::Result<()> {
        let harness = match_invocation(
            "npx",
            &[
                "-y".to_string(),
                "@zed-industries/claude-code-acp@latest".to_string(),
            ],
        )
        .ok_or_else(|| anyhow::anyhow!("legacy Claude adapter no longer catalog-matches"))?;
        assert_eq!(harness.id, "claude-acp");
        Ok(())
    }

    #[test]
    fn endpoint_plan_debug_redacts_credentials() -> anyhow::Result<()> {
        let harness =
            by_id("codex-acp").ok_or_else(|| anyhow::anyhow!("codex-acp missing from catalog"))?;
        let plan = harness
            .endpoint_plan("http://127.0.0.1:4356", "brk_secret", None, "brc_test")
            .ok_or_else(|| anyhow::anyhow!("Codex endpoint plan missing"))?;
        let debug = format!("{plan:?}");
        assert!(!debug.contains("brk_secret"), "credential leaked: {debug}");
        assert!(debug.contains("[REDACTED]"));
        Ok(())
    }

    #[test]
    fn controller_credential_updates_provider_and_fallback_auth() -> anyhow::Result<()> {
        let harness = by_id("claude-acp").ok_or_else(|| anyhow::anyhow!("claude-acp missing"))?;
        let plan = harness
            .endpoint_plan("http://127.0.0.1:4356", "old", None, "brc_test")
            .ok_or_else(|| anyhow::anyhow!("Claude endpoint plan missing"))?
            .controller_credential("brac_new");

        assert_eq!(
            plan.headers.get("authorization").map(String::as_str),
            Some("Bearer brac_new")
        );
        let env = plan
            .fallback_overlay()?
            .env
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN").map(String::as_str),
            Some("brac_new")
        );
        Ok(())
    }

    #[test]
    fn match_invocation_none_for_unknown_command() {
        assert!(match_invocation("./my-custom-agent", &[]).is_none());
    }

    #[test]
    fn match_invocation_matches_globally_installed_binary_command() {
        // A `command: claude-agent-acp` (npm -g binary) must catalog-match the
        // same as the pinned npx package form.
        assert_eq!(
            match_invocation("claude-agent-acp", &[]).unwrap().id,
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
    fn direct_model_overlay_pins_without_changing_provider_authority() -> anyhow::Result<()> {
        let codex = by_id("codex-acp")
            .ok_or_else(|| anyhow::anyhow!("codex-acp harness is unavailable"))?;
        let overlay = codex.direct_model_overlay("gpt-5.6");
        assert!(overlay.args.contains(&"model=\"gpt-5.6\"".to_string()));
        assert!(
            overlay
                .args
                .iter()
                .all(|arg| !arg.contains("model_provider"))
        );
        assert!(overlay.env.is_empty());

        let claude = by_id("claude-acp")
            .ok_or_else(|| anyhow::anyhow!("claude-acp harness is unavailable"))?;
        let overlay = claude.direct_model_overlay("claude-opus-5");
        assert_eq!(
            overlay.env,
            vec![("ANTHROPIC_MODEL".into(), "claude-opus-5".into())]
        );
        assert!(overlay.args.is_empty());
        Ok(())
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

    // ── Launch overlays (config synthesis + MCP injection). ──

    fn mcp() -> McpServer {
        McpServer {
            name: "bitrouter_stdio".into(),
            transport: McpTransport::Stdio {
                command: "/bin/bitrouter".into(),
                args: vec!["mcp".into(), "serve".into()],
            },
        }
    }

    /// An HTTP gateway server (the `bitrouter_tools` shape) for the
    /// synthesizer tests.
    fn tools() -> McpServer {
        McpServer {
            name: "bitrouter_tools".into(),
            transport: McpTransport::Http {
                url: "http://127.0.0.1:4356/mcp".into(),
                headers: vec![("Authorization".into(), "Bearer tok".into())],
            },
        }
    }

    #[test]
    fn claude_launch_overlay_writes_mcp_config_and_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("claude-acp").unwrap();
        let o = h
            .launch_overlay("http://x:1", "t", None, &[], &[mcp(), tools()], dir.path())
            .expect("overlay");
        assert_eq!(o.args[0], "--mcp-config");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&o.args[1]).expect("config written"))
                .expect("json");
        // Stdio entry: bare command/args.
        assert_eq!(
            config["mcpServers"]["bitrouter_stdio"]["command"],
            "/bin/bitrouter"
        );
        assert_eq!(config["mcpServers"]["bitrouter_stdio"]["args"][1], "serve");
        // HTTP entry: explicit type (a url without type is a hard error in
        // Claude Code) plus the auth header as an object.
        let tools = &config["mcpServers"]["bitrouter_tools"];
        assert_eq!(tools["type"], "http");
        assert_eq!(tools["url"], "http://127.0.0.1:4356/mcp");
        assert_eq!(tools["headers"]["Authorization"], "Bearer tok");
        // Routing env comes through unchanged.
        assert!(o.env.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn codex_launch_overlay_appends_mcp_toml_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("codex-acp").unwrap();
        let o = h
            .launch_overlay("http://x:1", "t", None, &[], &[mcp(), tools()], dir.path())
            .expect("overlay");
        assert!(
            o.args
                .contains(&"mcp_servers.bitrouter_stdio.command=\"/bin/bitrouter\"".to_string()),
            "{:?}",
            o.args
        );
        assert!(
            o.args
                .contains(&"mcp_servers.bitrouter_stdio.args=[\"mcp\",\"serve\"]".to_string()),
            "{:?}",
            o.args
        );
        // HTTP entry: url selects the streamable-HTTP transport, auth rides
        // http_headers.
        assert!(
            o.args.contains(
                &"mcp_servers.bitrouter_tools.url=\"http://127.0.0.1:4356/mcp\"".to_string()
            ),
            "{:?}",
            o.args
        );
        assert!(
            o.args.contains(
                &"mcp_servers.bitrouter_tools.http_headers.Authorization=\"Bearer tok\""
                    .to_string()
            ),
            "{:?}",
            o.args
        );
    }

    #[test]
    fn hermes_launch_overlay_synthesizes_home_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("hermes-acp").unwrap();
        let o = h
            .launch_overlay(
                "http://127.0.0.1:4356",
                "tok",
                Some("supergrok:grok-4.5"),
                &[],
                &[mcp(), tools()],
                dir.path(),
            )
            .expect("overlay");
        assert!(o.args.is_empty(), "hermes routes purely by config");
        let env: std::collections::HashMap<_, _> = o.env.iter().cloned().collect();
        assert_eq!(env["CUSTOM_API_KEY"], "tok");
        let home = std::path::PathBuf::from(&env["HERMES_HOME"]);
        // JSON body in config.yaml — YAML 1.2 parses JSON, no yaml dep needed.
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join("config.yaml")).expect("read"))
                .expect("json");
        assert_eq!(config["model"]["provider"], "custom");
        assert_eq!(config["model"]["base_url"], "http://127.0.0.1:4356/v1");
        assert_eq!(config["model"]["default"], "supergrok:grok-4.5");
        assert_eq!(
            config["mcp_servers"]["bitrouter_stdio"]["command"],
            "/bin/bitrouter"
        );
        // HTTP entry: url presence selects the transport; auth is a header.
        let tools = &config["mcp_servers"]["bitrouter_tools"];
        assert_eq!(tools["url"], "http://127.0.0.1:4356/mcp");
        assert_eq!(tools["headers"]["Authorization"], "Bearer tok");
        assert!(tools.get("command").is_none(), "http entry has no command");
    }

    #[test]
    fn openclaw_launch_overlay_synthesizes_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("openclaw").unwrap();
        let o = h
            .launch_overlay(
                "http://127.0.0.1:4356",
                "tok",
                Some("supergrok:grok-4.5"),
                &["x-ai/grok-4.5".to_string()],
                &[],
                dir.path(),
            )
            .expect("overlay");
        assert_eq!(
            o.args,
            vec!["tui".to_string(), "--local".to_string()],
            "embedded runtime, no gateway"
        );
        let env: std::collections::HashMap<_, _> = o.env.iter().cloned().collect();
        let config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&env["OPENCLAW_CONFIG_PATH"]).expect("read"),
        )
        .expect("json");
        let provider = &config["models"]["providers"]["bitrouter"];
        assert_eq!(provider["baseUrl"], "http://127.0.0.1:4356/v1");
        assert_eq!(provider["apiKey"], "tok");
        assert_eq!(provider["api"], "openai-completions");
        // Full model entries (config validation rejects bare ids), pinned
        // model first and default.
        assert_eq!(provider["models"][0]["id"], "supergrok:grok-4.5");
        assert_eq!(provider["models"][1]["id"], "x-ai/grok-4.5");
        assert!(provider["models"][0]["cost"].is_object());
        assert_eq!(
            config["agents"]["defaults"]["model"],
            "bitrouter/supergrok:grok-4.5"
        );
        assert!(env.contains_key("OPENCLAW_STATE_DIR"));
    }

    #[test]
    fn opencode_launch_overlay_synthesizes_one_config_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("opencode").unwrap();
        let catalog = vec!["x-ai/grok-4.5".to_string()];
        let o = h
            .launch_overlay(
                "http://127.0.0.1:4356",
                "tok",
                Some("supergrok:grok-4.5"),
                &catalog,
                &[mcp(), tools()],
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
        // The MCP bridge rides the same file, command+args folded into one
        // invocation array.
        assert_eq!(config["mcp"]["bitrouter_stdio"]["type"], "local");
        assert_eq!(
            config["mcp"]["bitrouter_stdio"]["command"][0],
            "/bin/bitrouter"
        );
        // HTTP entry: opencode's remote type with the auth header.
        let tools = &config["mcp"]["bitrouter_tools"];
        assert_eq!(tools["type"], "remote");
        assert_eq!(tools["url"], "http://127.0.0.1:4356/mcp");
        assert_eq!(tools["enabled"], true);
        assert_eq!(tools["headers"]["Authorization"], "Bearer tok");
    }

    #[test]
    fn opencode_overlay_defaults_model_to_catalog_head() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("opencode").unwrap();
        let catalog = vec!["a/one".to_string(), "b/two".to_string()];
        let o = h
            .launch_overlay("http://x:1", "t", None, &catalog, &[], dir.path())
            .expect("overlay");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&o.env[0].1).expect("written"))
                .expect("json");
        assert_eq!(config["model"], "bitrouter/a/one");
        assert!(config.get("mcp").is_none(), "no servers requested");
    }

    #[test]
    fn pi_launch_overlay_synthesizes_agent_dir_and_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("pi-acp").unwrap();
        let catalog = vec!["x-ai/grok-4.5".to_string()];
        let o = h
            .launch_overlay(
                "http://127.0.0.1:4356",
                "tok",
                Some("supergrok:grok-4.5"),
                &catalog,
                &[mcp(), tools()], // pi has no MCP mechanism — ignored
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
            .launch_overlay("http://x:1", "t", None, &[], &[], dir.path())
            .expect("overlay");
        assert!(
            o.args.is_empty(),
            "no routable model — pi keeps its own defaults"
        );
    }

    // ── Launch overlays (`bitrouter launch` facet with no gateways wired). ──

    #[test]
    fn opencode_launch_overlay_routes_to_the_gateway_without_mcp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("opencode").unwrap();
        let catalog = vec!["x-ai/grok-4.5".to_string()];
        let o = h
            .launch_overlay(
                "http://127.0.0.1:4356",
                "tok",
                None,
                &catalog,
                &[],
                dir.path(),
            )
            .expect("overlay");
        let (key, path) = &o.env[0];
        assert_eq!(key, "OPENCODE_CONFIG");
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("written")).expect("json");
        assert_eq!(
            config["provider"]["bitrouter"]["options"]["baseURL"],
            "http://127.0.0.1:4356/v1"
        );
        assert_eq!(config["provider"]["bitrouter"]["options"]["apiKey"], "tok");
        assert_eq!(config["model"], "bitrouter/x-ai/grok-4.5");
        // No gateways passed in — no `mcp` block synthesized.
        assert!(config.get("mcp").is_none(), "no servers, no mcp block");
    }

    #[test]
    fn pi_launch_overlay_pins_the_requested_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = by_id("pi-acp").unwrap();
        let catalog = vec!["x-ai/grok-4.5".to_string()];
        let o = h
            .launch_overlay(
                "http://127.0.0.1:4356",
                "tok",
                Some("supergrok:grok-4.5"),
                &catalog,
                &[],
                dir.path(),
            )
            .expect("overlay");
        // `supports_model_pin()` is false for pi (no env/args path), yet the
        // synthesis path pins the model — an interactive caller must not warn.
        assert!(!h.supports_model_pin());
        assert_eq!(
            o.args,
            vec!["--provider", "bitrouter", "--model", "supergrok:grok-4.5"]
        );
        let (key, agent_dir) = &o.env[0];
        assert_eq!(key, "PI_CODING_AGENT_DIR");
        let models: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(std::path::Path::new(agent_dir).join("models.json"))
                .expect("models.json written"),
        )
        .expect("json");
        let provider = &models["providers"]["bitrouter"];
        assert_eq!(provider["baseUrl"], "http://127.0.0.1:4356/v1");
        assert_eq!(provider["models"][1]["id"], "supergrok:grok-4.5");
    }

    #[test]
    fn claude_launch_overlay_injects_no_mcp_config_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        for id in ["claude-acp", "codex-acp"] {
            let h = by_id(id).unwrap();
            let o = h
                .launch_overlay("http://x:1", "t", None, &[], &[], dir.path())
                .expect("overlay");
            assert_eq!(
                o,
                h.routing_overlay("http://x:1", "t", None),
                "{id}: launch with no gateways is the plain routing overlay"
            );
            assert!(
                o.args.iter().all(|a| !a.contains("mcp")),
                "{id}: no MCP injection"
            );
        }
    }

    #[test]
    fn own_auth_launch_overlay_never_redirects() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (id, flag) in [("grok", "-m"), ("antigravity", "--model")] {
            let h = by_id(id).unwrap();
            let o = h
                .launch_overlay(
                    "http://127.0.0.1:4356",
                    "tok",
                    Some("some-model"),
                    &["x-ai/grok-4.5".to_string()],
                    &[],
                    dir.path(),
                )
                .expect("overlay");
            assert!(o.env.is_empty(), "{id}: never redirected to the gateway");
            assert_eq!(o.args, vec![flag, "some-model"], "{id}");
        }
    }

    #[test]
    fn gateway_injection_is_the_only_difference_the_servers_make() {
        // One synthesis, one method: passing gateway servers adds their
        // injection and changes nothing else about the routing overlay.
        for id in ["claude-acp", "codex-acp", "opencode", "hermes-acp", "grok"] {
            let h = by_id(id).unwrap();
            let a = tempfile::tempdir().expect("tempdir");
            let b = tempfile::tempdir().expect("tempdir");
            let bare = h
                .launch_overlay("http://x:1", "t", Some("m"), &[], &[], a.path())
                .expect("bare overlay");
            let with_gateways = h
                .launch_overlay("http://x:1", "t", Some("m"), &[], &[mcp()], b.path())
                .expect("gateway overlay");
            // Paths differ by state dir; compare env keys.
            let keys = |o: &RoutingOverlay| -> Vec<String> {
                o.env.iter().map(|(k, _)| k.clone()).collect()
            };
            assert_eq!(keys(&bare), keys(&with_gateways), "{id}");
            assert!(
                with_gateways.args.len() >= bare.args.len(),
                "{id}: injection only ever adds args"
            );
        }
    }

    /// Every file written under `dir`, as `relative path → contents`, with
    /// `dir`'s own path scrubbed out so two runs in different tempdirs
    /// compare equal.
    fn synthesized(dir: &std::path::Path) -> Vec<(String, String)> {
        fn walk(root: &std::path::Path, at: &std::path::Path, out: &mut Vec<(String, String)>) {
            let Ok(entries) = std::fs::read_dir(at) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else if let (Ok(rel), Ok(body)) =
                    (path.strip_prefix(root), std::fs::read_to_string(&path))
                {
                    out.push((rel.display().to_string(), body));
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        for (_, body) in &mut out {
            *body = body.replace(&dir.display().to_string(), "<STATE_DIR>");
        }
        out.sort();
        out
    }

    #[test]
    fn injects_mcp_matches_the_launch_overlay() {
        // The startup line (#796) tells the user whether the gateways reached
        // the harness. `injects_mcp` is the declarative answer; this proves it
        // agrees with what `launch_overlay` actually does — including for the
        // config-synthesis harnesses, whose injection lands inside a written
        // file rather than in the overlay itself.
        for h in CATALOG.iter().filter(|h| h.interactive_binary.is_some()) {
            let bare_dir = tempfile::tempdir().expect("tempdir");
            let with_dir = tempfile::tempdir().expect("tempdir");
            let scrub = |args: Vec<String>, dir: &std::path::Path| -> Vec<String> {
                args.into_iter()
                    .map(|a| a.replace(&dir.display().to_string(), "<STATE_DIR>"))
                    .collect()
            };
            let bare = h
                .launch_overlay("http://x:1", "t", Some("m"), &[], &[], bare_dir.path())
                .expect("bare overlay");
            let with = h
                .launch_overlay("http://x:1", "t", Some("m"), &[], &[mcp()], with_dir.path())
                .expect("gateway overlay");

            let differs = scrub(bare.args, bare_dir.path()) != scrub(with.args, with_dir.path())
                || synthesized(bare_dir.path()) != synthesized(with_dir.path());

            assert_eq!(
                differs,
                h.injects_mcp(),
                "{}: injects_mcp() says {} but the overlay {} when gateways are supplied",
                h.id,
                h.injects_mcp(),
                if differs { "changed" } else { "did not change" },
            );
        }
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
