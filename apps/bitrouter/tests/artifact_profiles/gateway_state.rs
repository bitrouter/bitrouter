//! Explicit model-only publication and immutable request facts across artifacts.

use super::*;
use bitrouter::gateway::observability::inventory::INVENTORY_VERSION;
use bitrouter::metering::entities::requests;
use bitrouter::storage::evolution::{EvolutionStore, digest};
use bitrouter::storage::session_identity::SessionIdentity;
use bitrouter_ai::policy::Document;
use sea_orm::{DatabaseConnection, EntityTrait};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

#[path = "gateway_seed.rs"]
mod seed;

#[path = "gateway_judge.rs"]
mod judge;

#[path = "gateway_trajectory.rs"]
mod trajectory;

fn selector(named: bool) -> &'static str {
    if named { "bitrouter/coding" } else { "coding" }
}

fn database_url(fixture: &Fixture) -> String {
    format!(
        "sqlite://{}?mode=rwc",
        fixture.directory.path().join("gateway.db").display()
    )
}

fn session(id: &str) -> SessionIdentity {
    SessionIdentity {
        owner: "local".into(),
        source: "fixture".into(),
        native_session_id: id.into(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RequestFacts {
    metering: requests::Model,
    inventory: (i64, Value),
    execution: Option<(i64, Value)>,
    intent: Option<(i64, Value)>,
}

async fn request_facts(db: &DatabaseConnection, request_id: &str) -> Result<RequestFacts> {
    let owner = EvolutionStore::new(db.clone(), "local")?;
    let scope = digest(&(INVENTORY_VERSION, "local", "controller"))?;
    let inventory = EvolutionStore::new(db.clone(), &format!("gateway:{scope}"))?;
    Ok(RequestFacts {
        metering: requests::Entity::find_by_id(request_id)
            .one(db)
            .await?
            .context("metering missing")?,
        inventory: inventory
            .get("gateway_request", request_id)
            .await?
            .context("inventory missing")?,
        execution: owner.get("execution", request_id).await?,
        intent: owner.get("route_intent", request_id).await?,
    })
}

fn snapshot(fixture: &Fixture, request_id: &str) -> Result<RequestFacts> {
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        let result = request_facts(&db, request_id).await;
        db.close().await?;
        result
    })
}

// Only this isolation negative creates Block state. Independent judge and
// trajectory fixtures use the neutral shared seed and never call this helper.
fn seed_unrelated_adopted_block(fixture: &Fixture) -> Result<Value> {
    use bitrouter::gateway::routing::blocks::{
        allocation::BanditConfig,
        control::{BlockDefinition, BlockRule, BlockStatus, ControlState},
        store::BlockPolicyStore,
    };
    fixture.runtime.block_on(async {
        let raw = fs::read_to_string(&fixture.config)?;
        let config = bitrouter::config::parse_with(&raw, |_| None)?;
        let assembled = Box::pin(bitrouter::assemble::build_app_with_path(
            &config,
            Some(&fixture.config),
        ))
        .await?;
        assembled
            .evolution
            .register(
                "local",
                BlockDefinition {
                    block_id: "unrelated-current-block".into(),
                    source: "fixture".into(),
                    rationale: "Stock gateway isolation negative.".into(),
                    rules: vec![BlockRule {
                        selector: "bitrouter/coding".into(),
                        fingerprint: None,
                        baseline_route: "bitrouter/coding".into(),
                        challenger_route: "bitrouter/candidate".into(),
                    }],
                    independence_rationale: "One independent block.".into(),
                    dependencies: std::collections::BTreeMap::new(),
                    measurement_contract: "fixture-measurement".into(),
                    batch_sessions: 32,
                    bandit: BanditConfig::default(),
                },
            )
            .await?;
        let store = EvolutionStore::new(assembled.db.clone(), "local")?;
        let (revision, _): (_, ControlState) = store
            .get("control", "owner")
            .await?
            .context("registered control missing")?;
        store
            .update::<ControlState, _>("control", "owner", revision, |state| {
                let block = state
                    .blocks
                    .get_mut("unrelated-current-block")
                    .context("registered Block missing")?;
                block.parent_revision = Some(block.revision.clone());
                block.revision = "controlled-isolation-adoption".into();
                block.status = BlockStatus::Adopted;
                block.plan = None;
                state.generation += 1;
                Ok(())
            })
            .await?;
        let state = BlockPolicyStore::new(assembled.db.clone(), "local")?
            .state()
            .await?;
        let block = state
            .blocks
            .get("unrelated-current-block")
            .context("current Block")?;
        block.definition.validate()?;
        ensure!(block.status == BlockStatus::Adopted && !block.routing_config_digest.is_empty());
        let result = serde_json::to_value(state)?;
        let db = assembled.db.clone();
        drop(assembled);
        db.close().await?;
        Ok(result)
    })
}

fn check_isolated_block(fixture: &Fixture, original: &Value) -> Result<()> {
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        let store = EvolutionStore::new(db.clone(), "local")?;
        let (_, state): (_, Value) = store
            .get("control", "owner")
            .await?
            .context("isolation Block missing")?;
        ensure!(state == *original, "gateway changed unrelated Block state");
        for kind in ["enrollment", "route_intent", "judge_job"] {
            ensure!(
                store.list::<Value>(kind).await?.is_empty(),
                "gateway created experimental {kind}"
            );
        }
        db.close().await?;
        Ok(())
    })
}

fn policy_cli(fixture: &mut Fixture, binary: &Path, args: &[&str]) -> Result<Value> {
    let mut command = fixture.command(binary);
    command
        .args(["--json", "policy"])
        .args(args)
        .arg("--config")
        .arg(&fixture.config);
    fixture.run(command)?.json()
}

fn check_source(fixture: &mut Fixture, binary: &Path, source: &Document) -> Result<String> {
    let digest = bitrouter::policy_lock::semantic_digest(source)?;
    for (args, view) in [
        (vec!["show", "coding"], "active"),
        (vec!["show", "coding", "--disk"], "disk"),
    ] {
        let report = policy_cli(fixture, binary, &args)?;
        ensure!(report["view"] == view && report["source_digest"] == digest);
        ensure!(serde_json::from_value::<Document>(report["source"].clone())? == *source);
    }
    Ok(digest)
}

fn conversation() -> Vec<Value> {
    vec![
        json!({"role":"user", "content":"Make a small change."}),
        json!({"role":"assistant", "content":"The existing contract must remain intact."}),
        json!({"role":"user", "content":"Keep every earlier message and verify the result."}),
    ]
}

struct ExpectedPolicy<'a> {
    model: &'a str,
    effort: &'a str,
}

fn upstream_count(fixture: &Fixture, upstream: &MockServer) -> Result<usize> {
    fixture.runtime.block_on(async {
        Ok(upstream
            .received_requests()
            .await
            .context("upstream request log missing")?
            .len())
    })
}

fn request(
    fixture: &Fixture,
    listen: &str,
    id: &str,
    session_id: &str,
    named: bool,
) -> Result<(u16, Value)> {
    fixture.runtime.block_on(async {
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(COMMAND_TIMEOUT)
            .build()?
            .post(format!("http://{listen}/v1/chat/completions"))
            .header("x-bitrouter-request-id", id)
            .header("x-bitrouter-controller-id", "controller")
            .header("x-bitrouter-acp-session-id", session_id)
            .json(&json!({"model":selector(named),"messages":conversation()}))
            .send()
            .await?;
        let status = response.status().as_u16();
        Ok((status, response.json().await?))
    })
}

fn dispatch(
    fixture: &Fixture,
    upstream: &MockServer,
    listen: &str,
    id: &str,
    session_id: &str,
    named: bool,
    expected: &ExpectedPolicy<'_>,
) -> Result<RequestFacts> {
    let expected_model = expected.model;
    let before = upstream_count(fixture, upstream)?;
    let (status, body) = request(fixture, listen, id, session_id, named)?;
    ensure!(status == 200, "request {id} failed: {status}: {body}");
    let sent = fixture.runtime.block_on(async {
        let calls = upstream
            .received_requests()
            .await
            .context("upstream request log missing")?;
        ensure!(
            calls.len() == before + 1,
            "request {id} dispatched an unexpected number of times"
        );
        let sent: Value = serde_json::from_slice(&calls.last().context("dispatch missing")?.body)?;
        Ok::<_, anyhow::Error>(sent)
    })?;
    // Delivery wakes a tracked terminal observer; receiving the body does not
    // imply that observer's transaction has already committed. Wait for the
    // actual persisted outcome before taking the immutable replay baseline.
    let deadline = Instant::now() + READY_TIMEOUT;
    let facts = loop {
        let facts = snapshot(fixture, id)?;
        if facts.inventory.1["settlement"]["outcome"] == "completed" {
            break facts;
        }
        ensure!(
            Instant::now() < deadline,
            "terminal outcome not persisted for {id}: {facts:?}"
        );
        std::thread::sleep(POLL);
    };
    ensure!(
        sent["model"] == expected_model,
        "wrong selected route for {id}: {sent}; route intent: {:?}",
        facts.intent
    );
    ensure!(
        sent["reasoning_effort"] == expected.effort,
        "wrong policy effort: {sent}"
    );
    ensure!(
        sent["temperature"] == 0.7 && sent["max_tokens"] == 96,
        "an unrelated route's defaults shadowed the public binding: {sent}"
    );
    let mut messages = vec![json!({"role":"system", "content":"Use recorded requirements."})];
    messages.extend(conversation());
    ensure!(
        sent["messages"] == json!(messages),
        "policy changed the full context: {sent}"
    );
    ensure!(
        facts.intent.is_none(),
        "mature gateway retained a Block route intent"
    );
    ensure!(facts.metering.router_id.as_deref() == Some("coding"));
    ensure!(
        facts
            .metering
            .binding_digest
            .as_ref()
            .is_some_and(|value| !value.is_empty())
    );
    ensure!(facts.metering.original_selector.as_deref() == Some("bitrouter/coding"));
    ensure!(facts.metering.model_id == expected_model && facts.metering.error.is_none());
    ensure!(facts.metering.prompt_tokens == 10 && facts.metering.completion_tokens == 2);
    ensure!(
        facts.metering.estimated_charge_micro_usd
            == if expected_model == "cheap" { 14 } else { 28 }
    );
    ensure!(facts.inventory.1["settlement"]["outcome"] == "completed");
    let settlement = &facts.inventory.1["settlement"];
    ensure!(
        settlement["final_provider"] == "fixture" && settlement["final_model"] == expected_model
    );
    let expected_cost = json!(if expected_model == "cheap" { 14 } else { 28 });
    ensure!(
        settlement["final_metered_cost_micro_usd"] == expected_cost
            && settlement["total_cost_micro_usd"] == expected_cost
    );
    ensure!(facts.inventory.1["request_id"] == id);
    if session_id != "missing-session" {
        let identity = serde_json::to_value(session(session_id))?;
        ensure!(facts.inventory.1["binding"]["identity"] == identity);
        let execution = &facts
            .execution
            .as_ref()
            .context("bound execution missing")?
            .1;
        ensure!(execution["request_id"] == id && execution["identity"] == identity);
        ensure!(execution["attempt_id"] == facts.inventory.1["attempt_id"]);
        ensure!(execution["settlement"] == *settlement);
        ensure!(
            execution["intent"].is_null(),
            "canonical fallback acquired Block authority"
        );
    }
    Ok(facts)
}

fn reject_replay(
    fixture: &Fixture,
    upstream: &MockServer,
    listen: &str,
    (id, session_id): (&str, &str),
    named: bool,
    original: &RequestFacts,
) -> Result<()> {
    let before = upstream_count(fixture, upstream)?;
    let (status, body) = request(fixture, listen, id, session_id, named)?;
    ensure!(
        status >= 400 && body.get("error").is_some(),
        "replay accepted: {status}: {body}"
    );
    ensure!(
        upstream_count(fixture, upstream)? == before,
        "replay dispatched upstream"
    );
    ensure!(
        snapshot(fixture, id)? == *original,
        "replay changed original attempt or accounting"
    );
    Ok(())
}

fn start(
    fixture: &mut Fixture,
    binary: &Path,
    artifact: &ArtifactIdentity,
) -> Result<(u32, String)> {
    let pid = fixture.start(binary)?;
    let report = fixture.status(binary, artifact, artifact, pid)?;
    fixture.health(&report)?;
    if artifact.features.is_empty() {
        ensure!(!bitrouter::client::agent_local::socket_path(&fixture.socket).exists());
    }
    Ok((pid, report.listen.context("listen address missing")?))
}

fn replay_completed(
    fixture: &Fixture,
    upstream: &MockServer,
    listen: &str,
    completed: &[(String, String, RequestFacts)],
) -> Result<()> {
    for (id, session_id, facts) in completed {
        ensure!(
            snapshot(fixture, id)? == *facts,
            "publication changed prior facts for {id}"
        );
        reject_replay(fixture, upstream, listen, (id, session_id), true, facts)?;
    }
    Ok(())
}

fn exercise(fixture: &mut Fixture, full_binary: &Path, minimum_binary: &Path) -> Result<()> {
    let full = super::persistence::identity(fixture, full_binary)?;
    let minimum = super::persistence::identity(fixture, minimum_binary)?;
    ensure!(full.profile == DistributionProfile::Full);
    ensure!(minimum.profile == DistributionProfile::Gateway && minimum.features.is_empty());
    ensure!(
        full.source_revision.is_some()
            && full.source_revision == minimum.source_revision
            && full.target == minimum.target
            && full.version == minimum.version
    );
    let upstream = fixture.runtime.block_on(async {
        let upstream = MockServer::start().await;
        for model in ["strong", "cheap"] {
            Mock::given(method("POST")).and(path("/chat/completions"))
                .and(body_partial_json(json!({"model":model})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id":"fixture-response","object":"chat.completion","model":model,
                    "choices":[{"index":0,"message":{"role":"assistant","content":"recorded response"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
                }))).mount(&upstream).await;
        }
        upstream
    });
    let raw = seed::configuration(fixture, &upstream.uri(), true)?;
    seed::assert_no_blocks(fixture)?;
    let source_a = seed::source("strong")?;
    let source_b = seed::source("cheap")?;
    let digest_a = bitrouter::policy_lock::semantic_digest(&source_a)?;
    let digest_b = bitrouter::policy_lock::semantic_digest(&source_b)?;
    ensure!(digest_a != digest_b);
    let original_bytes = fs::read(fixture.directory.path().join("policy.yaml"))?;
    fs::write(
        fixture.directory.path().join("candidate.yaml"),
        bitrouter::policy_lock::deterministic_yaml(&source_b)?,
    )?;
    let unrelated = seed_unrelated_adopted_block(fixture)?;
    let strong = ExpectedPolicy {
        model: "strong",
        effort: "high",
    };
    let cheap = ExpectedPolicy {
        model: "cheap",
        effort: "low",
    };
    let mut completed = Vec::new();
    for (binary, artifact, session_id) in [
        (full_binary, &full, "full"),
        (minimum_binary, &minimum, "minimum"),
    ] {
        let (pid, listen) = start(fixture, binary, artifact)?;
        check_source(fixture, binary, &source_a)?;
        check_isolated_block(fixture, &unrelated)?;
        replay_completed(fixture, &upstream, &listen, &completed)?;
        // A candidate file and a valid adopted Block cannot publish a new source.
        let id = format!("{session_id}-a");
        let facts = dispatch(fixture, &upstream, &listen, &id, session_id, true, &strong)?;
        completed.push((id, session_id.to_owned(), facts));
        ensure!(fs::read(fixture.directory.path().join("policy.yaml"))? == original_bytes);
        let published = policy_cli(
            fixture,
            binary,
            &["publish", "candidate.yaml", "--expected-digest", &digest_a],
        )?;
        ensure!(published["applied"] == true);
        check_source(fixture, binary, &source_b)?;
        replay_completed(fixture, &upstream, &listen, &completed)?;
        let id = format!("{session_id}-b");
        let facts = dispatch(fixture, &upstream, &listen, &id, session_id, true, &cheap)?;
        completed.push((id, session_id.to_owned(), facts));
        policy_cli(
            fixture,
            binary,
            &["rollback", &digest_a, "--expected-digest", &digest_b],
        )?;
        check_source(fixture, binary, &source_a)?;
        ensure!(fs::read(fixture.directory.path().join("policy.yaml"))? == original_bytes);
        replay_completed(fixture, &upstream, &listen, &completed)?;
        let id = format!("{session_id}-rollback");
        let facts = dispatch(fixture, &upstream, &listen, &id, session_id, true, &strong)?;
        completed.push((id, session_id.to_owned(), facts));
        if artifact.features.is_empty() {
            let facts = dispatch(
                fixture,
                &upstream,
                &listen,
                "unresolved",
                "missing-session",
                true,
                &strong,
            )?;
            ensure!(
                facts.inventory.1["binding"].is_null()
                    && facts.execution.is_none()
                    && facts.intent.is_none()
            );
            completed.push(("unresolved".into(), "missing-session".into(), facts));
        }
        check_isolated_block(fixture, &unrelated)?;
        ensure!(
            fs::read_to_string(&fixture.config)? == raw,
            "publication changed router definitions"
        );
        fixture.stop_resident(pid)?;
        check_isolated_block(fixture, &unrelated)?;
    }
    let (pid, listen) = start(fixture, full_binary, &full)?;
    check_source(fixture, full_binary, &source_a)?;
    replay_completed(fixture, &upstream, &listen, &completed)?;
    check_isolated_block(fixture, &unrelated)?;
    fixture.stop_resident(pid)?;
    for (id, _, facts) in completed {
        ensure!(
            snapshot(fixture, &id)? == facts,
            "restart changed prior facts for {id}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires separately built immutable full and minimum gateway artifacts"]
fn published_policy_and_accounting_survive_full_minimum_full() -> Result<()> {
    let full = artifact_path("BITROUTER_TEST_FULL_BINARY")?;
    let minimum = artifact_path("BITROUTER_TEST_GATEWAY_BINARY")?;
    ensure!(full != minimum);
    let mut fixture = Fixture::new()?;
    let result = exercise(&mut fixture, &full, &minimum);
    let cleanup = fixture.cleanup();
    result.with_context(|| {
        format!(
            "gateway source publication transition failed\n{}",
            fixture.diagnostics()
        )
    })?;
    cleanup.with_context(|| format!("gateway policy cleanup failed\n{}", fixture.diagnostics()))?;
    Ok(())
}
