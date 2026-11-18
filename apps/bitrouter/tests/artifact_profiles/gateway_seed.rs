//! Controlled publication and schema initialization for real gateway profile tests.

use super::*;
use bitrouter::policy_lock::deterministic_yaml;
use bitrouter_ai::policy::{Document, Policy, SpecVersion, Target};
use bitrouter_ai::types::ReasoningEffort;
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
providers:
  fixture:
    api_base: "{upstream}"
    api_key: fixture-only-key
    api_protocol:
      - "*": chat_completions
    models:
      - id: strong
        capabilities: [tools, reasoning]
        reasoning_effort: {{levels: [low, medium, high]}}
        pricing:
          input_micro_usd_per_token: 2
          output_micro_usd_per_token: 4
      - id: cheap
        capabilities: [tools, reasoning]
        reasoning_effort: {{levels: [low, medium, high]}}
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
routers:
  coding:
    selection:
      kind: model
      model: "fixture:strong"
    defaults:
      system_prompt: "Use recorded requirements."
      params:
        temperature: 0.7
        max_tokens: 96
  candidate:
    selection:
      kind: model
      model: "fixture:cheap"
    defaults:
      system_prompt: "Unrelated Block candidate defaults."
      params:
        temperature: 0.2
"#,
        database_url(fixture)
    ))?;
    if named {
        document["policy"] = json!({"path":"./policy.yaml"});
        for (name, tier) in [("coding", "strong"), ("candidate", "cheap")] {
            document["routers"][name]["selection"] = json!({
                "kind": "policy", "policy": name, "base_model": format!("fixture:{tier}"),
            });
        }
        fs::write(
            fixture.directory.path().join("policy.yaml"),
            deterministic_yaml(&source("strong")?)?,
        )?;
    }
    let raw = serde_saphyr::to_string(&document)?;
    fs::write(&fixture.config, &raw)?;
    fs::set_permissions(&fixture.config, fs::Permissions::from_mode(0o600))?;
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        bitrouter::db::run_migrations(&db).await?;
        db.close().await?;
        Ok::<_, anyhow::Error>(())
    })?;
    drop(reservation);
    Ok(raw)
}

pub(super) fn source(default_tier: &str) -> Result<Document> {
    ensure!(matches!(default_tier, "strong" | "cheap"));
    let mut policies = BTreeMap::new();
    for (name, selected) in [("coding", default_tier), ("candidate", "cheap")] {
        policies.insert(
            name.into(),
            Policy {
                tiers: BTreeMap::from([
                    (
                        "strong".into(),
                        Target {
                            model: "fixture:strong".into(),
                            effort: Some(ReasoningEffort::High),
                        },
                    ),
                    (
                        "cheap".into(),
                        Target {
                            model: "fixture:cheap".into(),
                            effort: Some(ReasoningEffort::Low),
                        },
                    ),
                ]),
                default_tier: selected.into(),
                classifier_digest: None,
                routes: BTreeMap::new(),
                tool_use_tier: Some("strong".into()),
                tool_safe_tiers: vec!["strong".into()],
            },
        );
    }
    Ok(Document {
        spec_version: SpecVersion::Beta1,
        policies,
    })
}

pub(super) fn assert_no_blocks(fixture: &Fixture) -> Result<()> {
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        ensure!(
            EvolutionStore::new(db.clone(), "local")?
                .list::<Value>("control")
                .await?
                .is_empty(),
            "independent fixture contains Block state"
        );
        db.close().await?;
        Ok(())
    })
}
