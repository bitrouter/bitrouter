//! Serve the same adopted policy and request facts through full/minimum artifacts.

use super::*;
use bitrouter::gateway::observability::inventory::INVENTORY_VERSION;
use bitrouter::gateway::routing::blocks::control::ControlState;
use bitrouter::gateway::routing::blocks::store::BlockPolicyStore;
use bitrouter::metering::entities::requests;
use bitrouter::storage::evolution::{EvolutionStore, digest};
use bitrouter::storage::session_identity::SessionIdentity;
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
    if named { "@coding" } else { "coding" }
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

fn check_policy(fixture: &Fixture, adopted: &Value, baseline: &Value) -> Result<()> {
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        let store = BlockPolicyStore::new(db.clone(), "local")?;
        ensure!(
            serde_json::to_value(store.state().await?)? == *adopted,
            "serving changed control state"
        );
        ensure!(
            serde_json::to_value(store.enrollment(&session("baseline")).await?)? == *baseline,
            "serving changed the preexisting baseline assignment"
        );
        ensure!(
            EvolutionStore::new(db.clone(), "local")?
                .list::<Value>("judge_job")
                .await?
                .is_empty(),
            "serving created a learning job"
        );
        db.close().await?;
        Ok(())
    })
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
        let response = reqwest::Client::builder().no_proxy().timeout(COMMAND_TIMEOUT).build()?
            .post(format!("http://{listen}/v1/chat/completions"))
            .header("x-bitrouter-request-id", id)
            .header("x-bitrouter-controller-id", "controller")
            .header("x-bitrouter-acp-session-id", session_id)
            .json(&json!({"model":selector(named),"messages":[{"role":"user","content":"Make a small change."}]}))
            .send().await?;
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
    expected_model: &str,
) -> Result<RequestFacts> {
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

fn exercise(
    fixture: &mut Fixture,
    full_binary: &Path,
    minimum_binary: &Path,
    named: bool,
) -> Result<()> {
    let full = super::persistence::identity(fixture, full_binary)?;
    let minimum = super::persistence::identity(fixture, minimum_binary)?;
    ensure!(full.profile == DistributionProfile::Full);
    ensure!(minimum.profile == DistributionProfile::Gateway && minimum.features.is_empty());
    ensure!(
        full.source_revision.is_some()
            && full.source_revision == minimum.source_revision
            && full.target == minimum.target
            && full.version == minimum.version
            && full.handoff_build_id != minimum.handoff_build_id
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
    let raw = seed::configuration(fixture, &upstream.uri(), named)?;
    let (pid, listen) = start(fixture, full_binary, &full)?;
    let baseline_request = dispatch(
        fixture,
        &upstream,
        &listen,
        "baseline-first",
        "baseline",
        named,
        "strong",
    )?;
    let baseline = fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        let enrollment = BlockPolicyStore::new(db.clone(), "local")?
            .enrollment(&session("baseline"))
            .await?
            .context("baseline enrollment missing")?;
        ensure!(enrollment.adopted_blocks.is_empty() && enrollment.assignments.is_empty());
        db.close().await?;
        Ok::<_, anyhow::Error>(serde_json::to_value(enrollment)?)
    })?;
    fixture.stop_resident(pid)?;
    let adopted = seed::adopt(fixture)?;
    let mut completed = vec![("baseline-first", "baseline", baseline_request)];
    for (binary, artifact, session_id) in [
        (full_binary, &full, "full"),
        (minimum_binary, &minimum, "minimum"),
        (full_binary, &full, "restored"),
    ] {
        let (pid, listen) = start(fixture, binary, artifact)?;
        for (id, original_session, facts) in &completed {
            reject_replay(
                fixture,
                &upstream,
                &listen,
                (id, original_session),
                named,
                facts,
            )?;
        }
        let facts = dispatch(
            fixture, &upstream, &listen, session_id, session_id, named, "cheap",
        )?;
        ensure!(
            facts.intent.as_ref().context("adopted intent missing")?.1["reason"]
                == "adopted_baseline"
        );
        completed.push((session_id, session_id, facts));
        if artifact.features.is_empty() {
            let retained = dispatch(
                fixture,
                &upstream,
                &listen,
                "baseline-next",
                "baseline",
                named,
                "strong",
            )?;
            ensure!(
                retained
                    .intent
                    .as_ref()
                    .context("baseline intent missing")?
                    .1["reason"]
                    == "retained_preexisting_baseline"
            );
            completed.push(("baseline-next", "baseline", retained));
            let unresolved = dispatch(
                fixture,
                &upstream,
                &listen,
                "unresolved",
                "missing-session",
                named,
                "strong",
            )?;
            ensure!(
                unresolved.inventory.1["binding"].is_null()
                    && unresolved.execution.is_none()
                    && unresolved.intent.is_none()
            );
            completed.push(("unresolved", "missing-session", unresolved));
        }
        check_policy(fixture, &adopted, &baseline)?;
        fixture.stop_resident(pid)?;
        check_policy(fixture, &adopted, &baseline)?;
        ensure!(fs::read_to_string(&fixture.config)? == raw);
    }
    // Both direct and named routes depend on the complete provider contract.
    // Change that contract without re-publishing the adopted block.
    let mut changed: Value = serde_saphyr::from_str(&raw)?;
    changed["providers"]["fixture"]["models"][1]["pricing"]["input_micro_usd_per_token"] = json!(3);
    fs::write(&fixture.config, serde_saphyr::to_string(&changed)?)?;
    let (pid, listen) = start(fixture, minimum_binary, &minimum)?;
    let changed = dispatch(
        fixture, &upstream, &listen, "changed", "changed", named, "strong",
    )?;
    ensure!(
        changed
            .intent
            .as_ref()
            .context("changed-dependency intent missing")?
            .1["reason"]
            == "routing_dependency_changed"
    );
    check_policy(fixture, &adopted, &baseline)?;
    fixture.stop_resident(pid)?;
    fs::write(&fixture.config, &raw)?;
    let (pid, listen) = start(fixture, full_binary, &full)?;
    for (id, original_session, facts) in &completed {
        reject_replay(
            fixture,
            &upstream,
            &listen,
            (id, original_session),
            named,
            facts,
        )?;
    }
    reject_replay(
        fixture,
        &upstream,
        &listen,
        ("changed", "changed"),
        named,
        &changed,
    )?;
    check_policy(fixture, &adopted, &baseline)?;
    fixture.stop_resident(pid)?;
    Ok(())
}

#[test]
#[ignore = "requires separately built immutable full and minimum gateway artifacts"]
fn published_policy_and_accounting_survive_full_minimum_full() -> Result<()> {
    let full = artifact_path("BITROUTER_TEST_FULL_BINARY")?;
    let minimum = artifact_path("BITROUTER_TEST_GATEWAY_BINARY")?;
    ensure!(full != minimum);
    for named in [false, true] {
        let mut fixture = Fixture::new()?;
        let result = exercise(&mut fixture, &full, &minimum, named);
        let cleanup = fixture.cleanup();
        result.with_context(|| {
            format!(
                "gateway policy transition failed (named={named})\n{}",
                fixture.diagnostics()
            )
        })?;
        cleanup
            .with_context(|| format!("gateway policy cleanup failed\n{}", fixture.diagnostics()))?;
    }
    Ok(())
}
