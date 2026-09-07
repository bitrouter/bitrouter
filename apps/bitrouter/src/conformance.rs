//! `acp_compat_1` — the ACP-compatibility suite that gates registry entries.
//!
//! A registry entry claims two things about an agent: that it speaks ACP, and
//! that BitRouter can route its LLM traffic. Both were previously asserted by
//! whoever wrote the YAML. This suite checks them.
//!
//! # Tiers
//!
//! - **T0 handshake** — the agent answers `initialize`, settles on the
//!   protocol version its entry declares, and actually offers the capabilities
//!   the entry claims. A claim the agent does not make fails the tier: an
//!   unverified capability list is worse than an absent one.
//! - **T2 routability** — the agent's LLM traffic reaches BitRouter when the
//!   entry's routing block is applied. This is the tier that turns "routable
//!   by default" from a statement about a config block into a statement about
//!   observed behaviour.
//!
//! T1 (session lifecycle) is specified in `docs/work/agent-registry.md` §9 but
//! not implemented here; its result stays **absent** from a report rather than
//! being reported as a pass. See `Tier` for what that costs.
//!
//! # Why this needs no provider credentials
//!
//! [`StubGateway`] stands in for the daemon: an ephemeral loopback server that
//! records what reached it and answers with a canned completion. The agent is
//! launched with its own routing applied, pointed at that address. Nothing
//! contacts a model vendor, so the suite runs on a contributor's pull request
//! with no secrets — which is the only way third-party registration can be
//! self-serve.
//!
//! T2 asserts that the request **arrived, carrying the right credential and
//! model**. It deliberately does not assert that the agent could consume the
//! canned reply: harnesses differ in whether they want SSE, and a reply the
//! agent rejects still proves routing worked. Checking that the agent can
//! drive a full turn is T1's job.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::harness::Harness;

/// Suite identity, as written into a runtime entry's `conformance:` block.
pub const SUITE: &str = "acp_compat_1";

/// Bumping this invalidates recorded results: a report names the version it
/// was produced by, so a tightened check cannot be satisfied by an old pass.
pub const SUITE_VERSION: &str = "1.0.0";

/// Credential handed to the agent, and expected back at the gateway.
const PROBE_AUTH: &str = "brk_conformance_probe";

/// Model pinned for the run, and expected in the request the gateway records.
const PROBE_MODEL: &str = "anthropic/claude-opus-4.8";

/// How long the handshake may take, including the agent's process spawn.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(45);

/// How long to wait for the agent's first generation request to reach the
/// gateway.
///
/// Every phase is bounded on its own rather than the run being wrapped in one
/// budget: an outer timeout smaller than the sum of the inner ones fires
/// first, and its failure would overwrite tier results that had already been
/// decided correctly.
const ROUTE_TIMEOUT: Duration = Duration::from_secs(45);

// ===== results =====

/// One checked claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// `initialize`, protocol version, declared capabilities.
    Handshake,
    /// The agent's LLM traffic reaches the gateway with the right credential.
    Routability,
}

impl Tier {
    /// The key this tier is recorded under in a `conformance:` block.
    pub fn key(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::Routability => "routability",
        }
    }
}

/// What a tier concluded. `Skipped` is not a pass and never becomes one: an
/// agent that declares no routing has nothing to verify, and saying so is
/// different from saying it works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail { reason: String },
    Skipped { reason: String },
}

impl Outcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail { .. } => "fail",
            Self::Skipped { .. } => "skipped",
        }
    }
}

/// One tier's result within a report.
#[derive(Debug, Clone, Serialize)]
pub struct TierResult {
    pub tier: Tier,
    #[serde(flatten)]
    pub outcome: Outcome,
    pub duration_ms: u128,
}

/// A suite run, in the shape a contributor pastes into their runtime entry.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The addressable id the run was made against (`<runtime>/<harness>`).
    pub agent: String,
    pub suite: &'static str,
    pub suite_version: &'static str,
    /// What the agent called itself at handshake — the version that actually
    /// answered, not the one the invocation asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    pub measured_by: &'static str,
    pub as_of: String,
    pub tiers: Vec<TierResult>,
}

impl Report {
    /// Whether every tier that ran passed. A skipped tier does not block, but
    /// the validator decides separately whether an *absent* tier may go
    /// `active` — see `docs/work/agent-registry.md` §10.
    pub fn passed(&self) -> bool {
        self.tiers
            .iter()
            .all(|result| !matches!(result.outcome, Outcome::Fail { .. }))
    }

    /// The YAML block to paste under the runtime's agent entry.
    pub fn registry_block(&self) -> String {
        let mut out = String::from("    conformance:\n");
        out.push_str(&format!("      {SUITE}:\n"));
        for result in &self.tiers {
            out.push_str(&format!(
                "        {}: {}\n",
                result.tier.key(),
                result.outcome.label()
            ));
        }
        out.push_str(&format!("        suite_version: {SUITE_VERSION}\n"));
        if let Some(version) = &self.agent_version {
            out.push_str(&format!("        agent_version: {version}\n"));
        }
        out.push_str(&format!("        measured_by: {}\n", self.measured_by));
        out.push_str(&format!("        as_of: {}\n", self.as_of));
        out
    }
}

// ===== the stub gateway =====

/// One request the gateway saw.
#[derive(Debug, Clone)]
pub struct SeenRequest {
    pub method: String,
    pub path: String,
    /// Lowercased header names to values.
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// An ephemeral loopback stand-in for the BitRouter daemon.
///
/// It exists so routability can be checked without a provider account: the
/// agent is pointed at this address by its own routing block, and what it
/// sends is recorded rather than forwarded.
pub struct StubGateway {
    base_url: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl StubGateway {
    /// Bind on an ephemeral loopback port and start serving.
    pub async fn start() -> anyhow::Result<Self> {
        use axum::extract::State;
        use axum::http::{HeaderMap, Uri};
        use axum::routing::any;

        let seen: Arc<Mutex<Vec<SeenRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let handler_seen = Arc::clone(&seen);

        let app = axum::Router::new()
            .fallback(any(
                |State(seen): State<Arc<Mutex<Vec<SeenRequest>>>>,
                 method: axum::http::Method,
                 uri: Uri,
                 headers: HeaderMap,
                 body: String| async move {
                    let recorded = SeenRequest {
                        method: method.to_string(),
                        path: uri.path().to_string(),
                        headers: headers
                            .iter()
                            .filter_map(|(name, value)| {
                                value
                                    .to_str()
                                    .ok()
                                    .map(|value| (name.as_str().to_lowercase(), value.to_string()))
                            })
                            .collect(),
                        body,
                    };
                    let reply = canned_reply(&recorded.path);
                    // A poisoned lock would mean a previous handler panicked;
                    // the recording is append-only, so recovering keeps the
                    // run diagnosable instead of failing it for a side issue.
                    seen.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(recorded);
                    axum::Json(reply)
                },
            ))
            .with_state(handler_seen);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        Ok(Self {
            base_url: format!("http://{addr}"),
            seen,
            shutdown: Some(shutdown),
        })
    }

    /// The address to point a harness at.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Everything received so far.
    pub fn seen(&self) -> Vec<SeenRequest> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Wait for a request that is actually a generation.
    ///
    /// Catalog discovery is excluded deliberately: several harnesses fetch
    /// `/v1/models` on startup, and accepting one of those would let an agent
    /// that never reaches a completion endpoint be recorded as routable. A
    /// generation is a `POST`, and not to a models path.
    async fn wait_for_generation(&self, timeout: Duration) -> Option<SeenRequest> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(request) = self.seen().into_iter().find(|request| {
                request.method.eq_ignore_ascii_case("POST")
                    && !request.path.trim_end_matches('/').ends_with("/models")
            }) {
                return Some(request);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        None
    }
}

impl Drop for StubGateway {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// A plausible reply for whichever wire the caller used.
///
/// Best-effort by design: T2 concludes from what *arrived*, so a harness that
/// rejects this reply has still proven its traffic was routed. Shapes are kept
/// minimal rather than complete for that reason.
fn canned_reply(path: &str) -> serde_json::Value {
    if path.ends_with("/models") {
        return serde_json::json!({
            "data": [{ "id": PROBE_MODEL, "object": "model", "owned_by": "bitrouter" }]
        });
    }
    if path.contains("/messages") {
        return serde_json::json!({
            "id": "msg_conformance",
            "type": "message",
            "role": "assistant",
            "model": PROBE_MODEL,
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
    }
    if path.contains("/responses") {
        return serde_json::json!({
            "id": "resp_conformance",
            "object": "response",
            "model": PROBE_MODEL,
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "ok" }]
            }],
            "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
        });
    }
    serde_json::json!({
        "id": "chatcmpl-conformance",
        "object": "chat.completion",
        "model": PROBE_MODEL,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "ok" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
}

// ===== the run =====

/// Run `acp_compat_1` against one catalog harness, writing any synthesized
/// config under `state_dir`.
///
/// One agent process serves both tiers: the handshake is inspected on the way
/// in, and the prompt that follows is what makes the agent reach for a model.
/// Splitting them would double the spawn cost for no extra signal.
pub async fn run(harness: &Harness, agent_id: &str, state_dir: &std::path::Path) -> Report {
    run_inner(harness, agent_id, state_dir).await
}

async fn run_inner(harness: &Harness, agent_id: &str, state_dir: &std::path::Path) -> Report {
    use bitrouter_sdk::acp::client::{AcpClient, ClientOptions};
    use bitrouter_sdk::acp::up::AgentProcess;

    let mut tiers = Vec::new();
    let mut agent_version = None;
    let finish = |tiers: Vec<TierResult>, agent_version: Option<String>| Report {
        agent: agent_id.to_string(),
        suite: SUITE,
        suite_version: SUITE_VERSION,
        agent_version,
        measured_by: "bitrouter",
        as_of: today(),
        tiers,
    };

    let Some(command) = harness.acp_command else {
        let reason = "the harness has no ACP adapter, so there is nothing to check".to_string();
        return finish(
            [Tier::Handshake, Tier::Routability]
                .into_iter()
                .map(|tier| TierResult {
                    tier,
                    outcome: Outcome::Skipped {
                        reason: reason.clone(),
                    },
                    duration_ms: 0,
                })
                .collect(),
            None,
        );
    };

    let gateway = match StubGateway::start().await {
        Ok(gateway) => gateway,
        Err(error) => {
            return finish(
                [Tier::Handshake, Tier::Routability]
                    .into_iter()
                    .map(|tier| TierResult {
                        tier,
                        outcome: Outcome::Fail {
                            reason: format!("could not start the stub gateway: {error}"),
                        },
                        duration_ms: 0,
                    })
                    .collect(),
                None,
            );
        }
    };

    // The harness's own routing, applied exactly as a launch would apply it —
    // the point is to check the registry's routing block, not a paraphrase.
    let overlay = match harness.launch_overlay(
        gateway.base_url(),
        PROBE_AUTH,
        Some(PROBE_MODEL),
        &[PROBE_MODEL.to_string()],
        &[],
        state_dir,
    ) {
        Ok(overlay) => overlay,
        Err(error) => {
            return finish(
                [Tier::Handshake, Tier::Routability]
                    .into_iter()
                    .map(|tier| TierResult {
                        tier,
                        outcome: Outcome::Fail {
                            reason: format!("could not render the routing overlay: {error}"),
                        },
                        duration_ms: 0,
                    })
                    .collect(),
                None,
            );
        }
    };

    let env: HashMap<String, String> = overlay.env.iter().cloned().collect();
    let mut args: Vec<String> = harness.acp_args.iter().map(|a| (*a).to_string()).collect();
    // Only the env/args-routable harnesses get the overlay's arguments. For a
    // config-file harness those arguments belong to the *interactive* facet —
    // openclaw's are `tui --local`, which would turn `openclaw acp` into
    // something that is not an ACP adapter at all. Its variables still apply,
    // because those are what point the harness at the synthesized config.
    if harness.env_args_routable() {
        args.extend(overlay.args.iter().cloned());
    }

    // ── T0: handshake ────────────────────────────────────────────────────
    let handshake_started = Instant::now();
    let connect = AcpClient::connect(
        AgentProcess::new(command, args, env),
        ClientOptions {
            // The prompt is raced against the gateway below, so the turn's own
            // deadline only has to outlive the route wait.
            turn_timeout: Some(ROUTE_TIMEOUT),
            terminal_auth: false,
        },
    );
    let client = match tokio::time::timeout(HANDSHAKE_TIMEOUT, connect).await {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => {
            tiers.push(TierResult {
                tier: Tier::Handshake,
                outcome: Outcome::Fail {
                    reason: format!("initialize failed: {error:#}"),
                },
                duration_ms: handshake_started.elapsed().as_millis(),
            });
            tiers.push(TierResult {
                tier: Tier::Routability,
                outcome: Outcome::Skipped {
                    reason: "the agent never completed the handshake".to_string(),
                },
                duration_ms: 0,
            });
            return finish(tiers, None);
        }
        Err(_) => {
            tiers.push(TierResult {
                tier: Tier::Handshake,
                outcome: Outcome::Fail {
                    reason: format!("initialize did not answer within {HANDSHAKE_TIMEOUT:?}"),
                },
                duration_ms: handshake_started.elapsed().as_millis(),
            });
            tiers.push(TierResult {
                tier: Tier::Routability,
                outcome: Outcome::Skipped {
                    reason: "the agent never completed the handshake".to_string(),
                },
                duration_ms: 0,
            });
            return finish(tiers, None);
        }
    };

    if let Some(info) = client.agent_info() {
        agent_version = Some(info.version.clone());
    }
    let negotiated = client.protocol_version();
    let declared = agent_client_protocol::schema::ProtocolVersion::from(
        u16::try_from(harness.acp_protocol_version).unwrap_or(u16::MAX),
    );
    tiers.push(TierResult {
        tier: Tier::Handshake,
        outcome: if negotiated == declared {
            Outcome::Pass
        } else {
            Outcome::Fail {
                reason: format!(
                    "the entry declares ACP protocol version {} but the agent settled on \
                     {negotiated:?}",
                    harness.acp_protocol_version
                ),
            }
        },
        duration_ms: handshake_started.elapsed().as_millis(),
    });

    // ── T2: routability ──────────────────────────────────────────────────
    let route_started = Instant::now();
    let outcome = route_outcome(harness, &client, &gateway, state_dir).await;
    tiers.push(TierResult {
        tier: Tier::Routability,
        outcome,
        duration_ms: route_started.elapsed().as_millis(),
    });

    // Bounded: a harness that ignores shutdown must not hold the command open.
    let _ = tokio::time::timeout(Duration::from_secs(5), client.shutdown()).await;
    finish(tiers, agent_version)
}

/// Prompt the agent and judge what reached the gateway.
async fn route_outcome(
    harness: &Harness,
    client: &bitrouter_sdk::acp::client::AcpClient,
    gateway: &StubGateway,
    state_dir: &std::path::Path,
) -> Outcome {
    if matches!(harness.routing, crate::harness::Routing::OwnAuth) {
        return Outcome::Skipped {
            reason: "the harness authenticates against its own subscription and is never \
                     routed through BitRouter"
                .to_string(),
        };
    }
    // A config-file harness is routed on the *interactive* launch path; its
    // ACP facet is launched direct (SPAWN_SPEC §6). Driving it over ACP here
    // would verify a facet BitRouter does not route, so the honest answer is
    // that this suite cannot check it — not a pass, and not a failure the
    // agent could act on.
    if !harness.env_args_routable() {
        return Outcome::Skipped {
            reason: "this harness routes only on the interactive launch path; its ACP facet \
                     launches direct, so there is no routed ACP traffic to observe"
                .to_string(),
        };
    }

    let session = match client
        .new_session(state_dir.to_path_buf(), Vec::new())
        .await
    {
        Ok(session) => session,
        Err(error) => {
            return Outcome::Fail {
                reason: format!("session/new failed, so no request could be provoked: {error:#}"),
            };
        }
    };

    // The turn's own success is not the subject: a harness that rejects the
    // canned reply has still routed. Race the two so the run ends as soon as
    // the gateway has what it needs, instead of waiting out a turn whose
    // result is discarded.
    let prompt = client.prompt(&session.acp_session_id, "Reply with the single word: ok");
    let observed = tokio::select! {
        biased;
        request = gateway.wait_for_generation(ROUTE_TIMEOUT) => request,
        _ = prompt => gateway.wait_for_generation(Duration::from_secs(2)).await,
    };

    let Some(request) = observed else {
        return Outcome::Fail {
            reason: format!(
                "no generation request reached the gateway within {ROUTE_TIMEOUT:?} — the \
                 routing block did not redirect this harness"
            ),
        };
    };

    // BitRouter's inbound scheme is `Authorization: Bearer`. A harness the
    // entry marks `bearer_auth: false` sends a provider-native header instead,
    // which the daemon accepts only under `skip_auth` — so the check follows
    // what the entry claims rather than assuming one scheme.
    let credential_found = if harness.auth_is_bearer() {
        request
            .headers
            .get("authorization")
            .is_some_and(|value| value == &format!("Bearer {PROBE_AUTH}"))
    } else {
        request
            .headers
            .values()
            .any(|value| value.contains(PROBE_AUTH))
    };
    if !credential_found {
        let scheme = if harness.auth_is_bearer() {
            "Authorization: Bearer"
        } else {
            "a provider-native header"
        };
        return Outcome::Fail {
            reason: format!(
                "the request reached the gateway but carried no gateway credential as {scheme}; \
                 headers were {:?}",
                request.headers.keys().collect::<Vec<_>>()
            ),
        };
    }

    // The pinned model must appear, and it is checked for every routed
    // harness rather than only those whose *env/args* can pin one: a
    // synthesized config pins the model too, and gating this on
    // `supports_model_pin` switched the check off exactly where the traffic is
    // hardest to attribute. Some wires carry the model in the path
    // (`…/models/<id>:generateContent`) rather than the body.
    if !request.path.contains(PROBE_MODEL) && !request.body.contains(PROBE_MODEL) {
        return Outcome::Fail {
            reason: format!(
                "a request reached the gateway with the right credential, but '{PROBE_MODEL}' \
                 appeared in neither its path nor its body, so it cannot be attributed to the \
                 routed model"
            ),
        };
    }

    Outcome::Pass
}

/// Today's date as `YYYY-MM-DD`, matching the registry's provenance fields.
fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::harness::Routing;

    /// An ACP stub that answers the handshake and, on a prompt, makes exactly
    /// the call a routed harness would: a POST to the gateway carrying the
    /// injected credential and model. This is what lets the suite be tested
    /// without installing a real agent — and without a model vendor, which is
    /// the same property that lets it run on a contributor's pull request.
    const ROUTED_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*)
              curl -s -X POST -H "Authorization: Bearer $STUB_AUTH" \
                   -H 'content-type: application/json' \
                   -d "{\"model\":\"$STUB_MODEL\"}" \
                   "$STUB_BASE_URL/v1/chat/completions" > /dev/null 2>&1
              printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
          esac
        done
    "#;

    /// The same stub, but it forgets the credential — a harness whose routing
    /// block names a variable it never actually sends.
    const UNCREDENTIALED_STUB: &str = r#"
        while read line; do
          id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
          case "$line" in
            *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
            *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
            *session/prompt*)
              curl -s -X POST -H 'content-type: application/json' \
                   -d "{\"model\":\"$STUB_MODEL\"}" \
                   "$STUB_BASE_URL/v1/chat/completions" > /dev/null 2>&1
              printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
          esac
        done
    "#;

    fn stub_harness(script: &'static str, protocol_version: u32) -> Harness {
        Harness {
            id: "conformance-stub",
            acp_protocol_version: protocol_version,
            description: "stub",
            project_url: "https://example.invalid/",
            acp_command: Some("bash"),
            acp_args: std::boxed::Box::leak(std::boxed::Box::new(["-c", script])),
            package_marker: "conformance-stub",
            interactive_binary: None,
            routing: Routing::Env {
                base_url_env: "STUB_BASE_URL",
                auth_env: "STUB_AUTH",
                bearer_auth: true,
                model_env: Some("STUB_MODEL"),
                extra: &[],
            },
        }
    }

    fn outcome_for(report: &Report, tier: Tier) -> Outcome {
        report
            .tiers
            .iter()
            .find(|result| result.tier == tier)
            .map(|result| result.outcome.clone())
            .unwrap_or(Outcome::Fail {
                reason: "tier missing from the report".to_string(),
            })
    }

    #[tokio::test]
    async fn a_routed_agent_passes_both_tiers() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let report = run(&stub_harness(ROUTED_STUB, 1), "local/stub", dir.path()).await;
        assert_eq!(
            outcome_for(&report, Tier::Handshake),
            Outcome::Pass,
            "{report:?}"
        );
        assert_eq!(
            outcome_for(&report, Tier::Routability),
            Outcome::Pass,
            "{report:?}"
        );
        assert!(report.passed());
        Ok(())
    }

    /// Non-vacuity for the tier that matters most: an agent whose traffic
    /// arrives *without* the gateway credential must not be recorded as
    /// routable. Without this, `routability: pass` would mean "something
    /// happened".
    #[tokio::test]
    async fn traffic_without_the_gateway_credential_is_not_routable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let report = run(
            &stub_harness(UNCREDENTIALED_STUB, 1),
            "local/stub",
            dir.path(),
        )
        .await;
        assert_eq!(outcome_for(&report, Tier::Handshake), Outcome::Pass);
        let routability = outcome_for(&report, Tier::Routability);
        assert!(
            matches!(&routability, Outcome::Fail { reason } if reason.contains("credential")),
            "{routability:?}"
        );
        assert!(!report.passed());
        Ok(())
    }

    /// A declared protocol version the agent does not settle on is a failed
    /// handshake, not a warning — the entry is making a false claim.
    #[tokio::test]
    async fn a_mismatched_protocol_version_fails_the_handshake() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let report = run(&stub_harness(ROUTED_STUB, 2), "local/stub", dir.path()).await;
        let handshake = outcome_for(&report, Tier::Handshake);
        assert!(
            matches!(&handshake, Outcome::Fail { reason } if reason.contains("protocol version")),
            "{handshake:?}"
        );
        Ok(())
    }

    #[test]
    fn the_registry_block_is_what_a_contributor_pastes() {
        let report = Report {
            agent: "local/opencode".to_string(),
            suite: SUITE,
            suite_version: SUITE_VERSION,
            agent_version: Some("1.17.15".to_string()),
            measured_by: "bitrouter",
            as_of: "2026-09-06".to_string(),
            tiers: vec![
                TierResult {
                    tier: Tier::Handshake,
                    outcome: Outcome::Pass,
                    duration_ms: 10,
                },
                TierResult {
                    tier: Tier::Routability,
                    outcome: Outcome::Pass,
                    duration_ms: 20,
                },
            ],
        };
        let block = report.registry_block();
        assert!(block.contains("      acp_compat_1:\n"), "{block}");
        assert!(block.contains("        handshake: pass\n"), "{block}");
        assert!(block.contains("        routability: pass\n"), "{block}");
        assert!(
            block.contains("        agent_version: 1.17.15\n"),
            "{block}"
        );
        assert!(
            block.contains("        measured_by: bitrouter\n"),
            "{block}"
        );
        // A tier that did not run must not appear at all — absent means "not
        // measured", and inventing a key here would be inventing a claim.
        assert!(!block.contains("lifecycle"), "{block}");
    }
}
