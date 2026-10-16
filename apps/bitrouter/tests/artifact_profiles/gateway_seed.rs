//! Controlled publication and recorded history for real gateway profile tests.

use super::*;
use crate::capture_fixture::{FixtureScope, PersistedHistory};
use bitrouter::config::Config;
use bitrouter::gateway::routing::blocks::allocation::BanditConfig;
use bitrouter::gateway::routing::blocks::control::{BlockDefinition, BlockRule, BlockStatus};
use bitrouter::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml};
use bitrouter::storage::capture::{CaptureDirection, CaptureEvent, CaptureKind};
use std::collections::BTreeMap;

pub(super) fn configuration(fixture: &Fixture, upstream: &str, named: bool) -> Result<String> {
    let reservation = TcpListener::bind("127.0.0.1:0")?;
    let listen = reservation.local_addr()?;
    let mut document: Value = serde_saphyr::from_str(&format!(
        r#"
inherit_defaults: false
registry:
  enabled: false
server:
  listen: "{listen}"
  control_socket: control.sock
  skip_auth: true
database:
  url: "{}"
agent_api:
  enabled: false
policy:
  mode: frozen
providers:
  fixture:
    api_base: "{upstream}"
    api_key: fixture-only-key
    api_protocol:
      - "*": chat_completions
    models:
      - id: strong
        capabilities: [tools]
        pricing:
          input_micro_usd_per_token: 2
          output_micro_usd_per_token: 4
      - id: cheap
        capabilities: [tools]
        pricing:
          input_micro_usd_per_token: 1
          output_micro_usd_per_token: 2
models:
  coding:
    endpoints:
      - provider: fixture
        service_id: strong
  candidate:
    endpoints:
      - provider: fixture
        service_id: cheap
presets:
  coding:
    model: "fixture:strong"
    system_prompt: "Use recorded requirements."
  candidate:
    model: "fixture:cheap"
    system_prompt: "Use recorded requirements."
    params:
      temperature: 0.2
"#,
        database_url(fixture)
    ))?;
    if named {
        let mut lock = PolicyLock::default();
        for (name, tier) in [("coding", "strong"), ("candidate", "cheap")] {
            document["presets"][name]["policy"] = json!(name);
            lock.policies.insert(
                name.into(),
                PolicyDefinition {
                    tiers: BTreeMap::from([
                        ("strong".into(), "fixture:strong".into()),
                        ("cheap".into(), "fixture:cheap".into()),
                    ]),
                    default_tier: Some(tier.into()),
                    tool_use_tier: Some("strong".into()),
                    tool_safe_tiers: vec!["strong".into()],
                    ..PolicyDefinition::default()
                },
            );
        }
        fs::write(
            fixture.directory.path().join("policy-lock.yaml"),
            deterministic_yaml(&lock)?,
        )?;
    }
    let raw = serde_saphyr::to_string(&document)?;
    let config: Config = bitrouter::config::parse_with(&raw, |_| None)?;
    fs::write(&fixture.config, &raw)?;
    fs::set_permissions(&fixture.config, fs::Permissions::from_mode(0o600))?;
    fixture.runtime.block_on(async {
        let assembled = bitrouter::assemble::build_app_with_path(&config, Some(&fixture.config)).await?;
        let definition = BlockDefinition {
            block_id: "coding-block".into(),
            source: "fixture".into(),
            rationale: "Compare complete compatible routes.".into(),
            rules: vec![BlockRule {
                selector: selector(named).into(),
                fingerprint: None,
                baseline_route: selector(named).into(),
                challenger_route: if named { "@candidate" } else { "candidate" }.into(),
            }],
            independence_rationale: "One controlled block.".into(),
            dependencies: BTreeMap::new(),
            measurement_contract: "fixture-measurement".into(),
            batch_sessions: 32,
            bandit: BanditConfig::default(),
        };
        // The real core registration computes the production route digest.
        // Adoption below is a controlled serving fixture, not learned promotion.
        assembled.evolution.register("local", definition).await?;
        for id in ["baseline", "full", "minimum", "restored", "changed"] {
            let history = PersistedHistory::new(assembled.db.clone(), FixtureScope {
                owner: "local".into(),
                source: "fixture".into(),
                controller_instance_id: Some("controller".into()),
                route_scope_id: Some("local".into()),
            }).await?;
            assembled.gateway_requests.inventory()
                .register_capture(history.connection_id(), "local", "controller").await?;
            for (kind, method, call_id, payload) in [
                (CaptureKind::Request, "session/new", 1, json!({"cwd":"/fixture"})),
                (CaptureKind::Response, "session/new", 1, json!({"result":{"sessionId":id}})),
                (CaptureKind::Request, "session/prompt", 2,
                    json!({"sessionId":id,"prompt":[{"type":"text","text":"Make a small change."}]})),
            ] {
                history.append(id, CaptureEvent {
                    direction: CaptureDirection::Client, kind, call_id: Some(call_id),
                    method: method.into(), payload,
                }).await?;
            }
        }
        let db = assembled.db.clone();
        drop(assembled);
        db.close().await?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(reservation);
    Ok(raw)
}

pub(super) fn adopt(fixture: &Fixture) -> Result<Value> {
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        let store = EvolutionStore::new(db.clone(), "local")?;
        let (revision, _): (_, ControlState) = store
            .get("control", "owner")
            .await?
            .context("control missing")?;
        store
            .update::<ControlState, _>("control", "owner", revision, |state| {
                let block = state
                    .blocks
                    .get_mut("coding-block")
                    .context("block missing")?;
                block.parent_revision = Some(block.revision.clone());
                block.revision = "controlled-adoption".into();
                block.status = BlockStatus::Adopted;
                block.plan = None;
                state.generation += 1;
                Ok(())
            })
            .await?;
        let state =
            serde_json::to_value(BlockPolicyStore::new(db.clone(), "local")?.state().await?)?;
        db.close().await?;
        Ok(state)
    })
}
