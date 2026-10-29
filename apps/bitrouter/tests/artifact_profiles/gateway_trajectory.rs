//! Durable gateway facts and deferred evaluation across real feature profiles.

use super::*;
use crate::persistence::table_rows;
use bitrouter::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml};
use bitrouter::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};
use bitrouter::trajectory::health::reduce;
use bitrouter::trajectory::types::{TrajectoryEvent, TrajectorySnapshot};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
struct OutboxRow {
    immutable: String,
    attempts: i64,
    delivered_at: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct TrajectoryFacts {
    records: BTreeMap<&'static str, Vec<String>>,
    outbox: Vec<OutboxRow>,
    evaluation: BTreeMap<&'static str, Vec<String>>,
    health: TrajectorySnapshot,
}

fn trajectory_facts(fixture: &Fixture) -> Result<TrajectoryFacts> {
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        let mut records = BTreeMap::new();
        for table in ["requests", "trajectory_episodes", "trajectory_requests", "trajectory_events", "trajectory_prefix_index"] {
            records.insert(table, table_rows(&db, table).await?);
        }
        let mut evaluation = BTreeMap::new();
        for table in ["eval_subjects", "eval_results", "eval_admission_events"] {
            evaluation.insert(table, table_rows(&db, table).await?);
        }
        let outbox = db.query_all(Statement::from_string(DbBackend::Sqlite,
            "SELECT json_array(outbox_id,owner_user_id,topic,payload_json,payload_digest,created_at) AS immutable,attempts,delivered_at FROM trajectory_outbox ORDER BY outbox_id".to_owned()))
            .await?.into_iter().map(|row| Ok(OutboxRow {
                immutable: row.try_get("", "immutable")?, attempts: row.try_get("", "attempts")?,
                delivered_at: row.try_get("", "delivered_at")?,
            })).collect::<Result<Vec<_>>>()?;
        let events = db.query_all(Statement::from_string(DbBackend::Sqlite,
            "SELECT event_json FROM trajectory_events WHERE owner_user_id = 'local' ORDER BY sequence".to_owned()))
            .await?.into_iter().map(|row| {
                let raw: String = row.try_get("", "event_json")?;
                Ok(serde_json::from_str::<TrajectoryEvent>(&raw)?)
            }).collect::<Result<Vec<_>>>()?;
        let health = reduce(&events, &BTreeSet::from(["strong".into()]))?;
        db.close().await?;
        Ok(TrajectoryFacts { records, outbox, evaluation, health })
    })
}

fn execute_sql(fixture: &Fixture, sql: &str) -> Result<()> {
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        db.execute(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await?;
        db.close().await?;
        Ok(())
    })
}

fn configuration(fixture: &Fixture, upstream: &str) -> Result<()> {
    let raw = seed::configuration(fixture, upstream, false)?;
    let mut config: Value = serde_saphyr::from_str(&raw)?;
    config["trajectory"] = json!({"enabled":true});
    config["presets"]["coding"]["policy"] = json!("coding");
    fs::write(&fixture.config, serde_saphyr::to_string(&config)?)?;
    let mut lock = PolicyLock::default();
    lock.policies.insert(
        "coding".into(),
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("cheap".into(), "fixture:cheap".into()),
                ("strong".into(), "fixture:strong".into()),
            ]),
            default_tier: Some("cheap".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            progress_guard: Some(ProgressGuardPolicy {
                escalation_tier: "strong".into(),
                protected_tiers: BTreeSet::from(["strong".into()]),
                max_consecutive_unprotected: None,
                max_same_projection_unprotected: None,
                max_recovery_count: None,
                max_episode_requests: None,
                max_episode_elapsed_ms: None,
                max_episode_cost_micro_usd: Some(10),
                hold_for_requests: 2,
                incomplete_history: IncompleteHistoryAction::Observe,
            }),
            ..PolicyDefinition::default()
        },
    );
    fs::write(
        fixture.directory.path().join("policy-lock.yaml"),
        deterministic_yaml(&lock)?,
    )?;
    // Let the full gateway commit a real terminal fact/outbox while its separate
    // consumer fails. No synthetic settlement or delivery receipt is inserted.
    execute_sql(
        fixture,
        "CREATE TRIGGER fixture_reject_eval BEFORE INSERT ON eval_subjects BEGIN SELECT RAISE(FAIL, 'fixture consumer unavailable'); END",
    )
}

fn infer(
    fixture: &Fixture,
    upstream: &MockServer,
    listen: &str,
    id: &str,
    messages: &[Value],
    model: &str,
) -> Result<()> {
    let before = upstream_count(fixture, upstream)?;
    fixture.runtime.block_on(async {
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(COMMAND_TIMEOUT)
            .build()?
            .post(format!("http://{listen}/v1/chat/completions"))
            .header("x-bitrouter-request-id", id)
            .json(&json!({"model":"@coding","messages":messages}))
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        ensure!(
            status.is_success(),
            "guarded request failed: {status}: {body}"
        );
        let calls = upstream
            .received_requests()
            .await
            .context("upstream log missing")?;
        ensure!(calls.len() == before + 1);
        let sent: Value = serde_json::from_slice(&calls.last().context("dispatch missing")?.body)?;
        ensure!(sent["model"] == model, "wrong guarded model: {sent}");
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        let metered = requests::Entity::find_by_id(id)
            .one(&db)
            .await?
            .context("metering missing")?;
        ensure!(metered.model_id == model && metered.error.is_none());
        ensure!(metered.prompt_tokens == 10 && metered.completion_tokens == 2);
        ensure!(metered.estimated_charge_micro_usd == if model == "cheap" { 14 } else { 28 });
        db.close().await?;
        Ok(())
    })
}

fn exercise_trajectory(fixture: &mut Fixture, full: &Path, minimum: &Path) -> Result<()> {
    let full_identity = crate::persistence::identity(fixture, full)?;
    let minimum_identity = crate::persistence::identity(fixture, minimum)?;
    ensure!(full_identity.profile == DistributionProfile::Full);
    ensure!(
        minimum_identity.profile == DistributionProfile::Gateway
            && minimum_identity.features.is_empty()
    );
    ensure!(
        full_identity.source_revision.is_some()
            && full_identity.source_revision == minimum_identity.source_revision
            && full_identity.target == minimum_identity.target
            && full_identity.version == minimum_identity.version
            && full_identity.handoff_build_id != minimum_identity.handoff_build_id
    );
    let upstream = fixture.runtime.block_on(async {
        let upstream = MockServer::start().await;
        for model in ["strong", "cheap"] {
            Mock::given(method("POST")).and(path("/chat/completions"))
                .and(body_partial_json(json!({"model":model})))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id":"trajectory-fixture","object":"chat.completion","model":model,
                    "choices":[{"index":0,"message":{"role":"assistant","content":"recorded response"},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
                }))).mount(&upstream).await;
        }
        upstream
    });
    configuration(fixture, &upstream.uri())?;
    let (pid, listen) = start(fixture, full, &full_identity)?;
    let mut messages = vec![json!({"role":"user","content":"Make a small change."})];
    infer(
        fixture,
        &upstream,
        &listen,
        "trajectory-first",
        &messages,
        "cheap",
    )?;
    fixture.stop_resident(pid)?;
    let original = trajectory_facts(fixture)?;
    ensure!(original.records["trajectory_episodes"].len() == 1);
    ensure!(
        original.health.health.settled_request_count == 1
            && original.health.health.settled_cost_micro_usd == Some(14)
    );
    ensure!(
        original.outbox.len() == 1
            && original.outbox[0].attempts > 0
            && original.outbox[0].delivered_at.is_none()
    );
    ensure!(original.evaluation.values().all(Vec::is_empty));
    let key = fs::read(fixture.directory.path().join("correlation.key"))?;
    execute_sql(fixture, "DROP TRIGGER fixture_reject_eval")?;

    let (pid, listen) = start(fixture, minimum, &minimum_identity)?;
    ensure!(
        trajectory_facts(fixture)? == original,
        "minimum startup changed retained facts"
    );
    ensure!(fs::read(fixture.directory.path().join("correlation.key"))? == key);
    messages.extend([
        json!({"role":"assistant","content":"recorded response"}),
        json!({"role":"user","content":"Check the result."}),
    ]);
    infer(
        fixture,
        &upstream,
        &listen,
        "trajectory-second",
        &messages,
        "strong",
    )?;
    let pending = trajectory_facts(fixture)?;
    ensure!(pending.records["trajectory_episodes"].len() == 1);
    ensure!(pending.health.episode_id == original.health.episode_id);
    ensure!(
        pending.health.health.request_count == 2
            && pending.health.health.settled_request_count == 2
    );
    ensure!(pending.health.health.settled_cost_micro_usd == Some(42));
    ensure!(
        pending.health.active_hold_remaining > 0,
        "cost escalation did not create a hold"
    );
    ensure!(pending.outbox.len() == 2 && pending.outbox.contains(&original.outbox[0]));
    ensure!(
        pending
            .outbox
            .iter()
            .filter(|row| row.attempts == 0)
            .count()
            == 1
    );
    ensure!(pending.outbox.iter().all(|row| row.delivered_at.is_none()));
    ensure!(pending.evaluation == original.evaluation);
    fixture.stop_resident(pid)?;
    ensure!(
        trajectory_facts(fixture)? == pending,
        "minimum shutdown attempted publication"
    );

    let (pid, _) = start(fixture, full, &full_identity)?;
    let delivered = trajectory_facts(fixture)?;
    ensure!(
        delivered.records == pending.records && delivered.health == pending.health,
        "publication repeated settlement"
    );
    ensure!(delivered.outbox.len() == pending.outbox.len());
    for (before, after) in pending.outbox.iter().zip(&delivered.outbox) {
        ensure!(
            before.immutable == after.immutable
                && after.attempts == before.attempts + 1
                && after.delivered_at.is_some()
        );
    }
    ensure!(delivered.evaluation.values().all(|rows| rows.len() == 2));
    ensure!(upstream_count(fixture, &upstream)? == 2);
    fixture.stop_resident(pid)?;
    ensure!(trajectory_facts(fixture)? == delivered);
    let (pid, _) = start(fixture, full, &full_identity)?;
    ensure!(
        trajectory_facts(fixture)? == delivered,
        "delivered evidence was published twice"
    );
    ensure!(fs::read(fixture.directory.path().join("correlation.key"))? == key);
    fixture.stop_resident(pid)?;
    ensure!(trajectory_facts(fixture)? == delivered);
    Ok(())
}

#[test]
#[ignore = "requires separately built immutable full and minimum gateway artifacts"]
fn trajectory_cost_guard_and_outbox_survive_full_minimum_full() -> Result<()> {
    let full = artifact_path("BITROUTER_TEST_FULL_BINARY")?;
    let minimum = artifact_path("BITROUTER_TEST_GATEWAY_BINARY")?;
    ensure!(full != minimum);
    let mut fixture = Fixture::new()?;
    let result = exercise_trajectory(&mut fixture, &full, &minimum);
    let cleanup = fixture.cleanup();
    result.with_context(|| {
        format!(
            "trajectory profile transition failed\n{}",
            fixture.diagnostics()
        )
    })?;
    cleanup.with_context(|| format!("trajectory cleanup failed\n{}", fixture.diagnostics()))?;
    Ok(())
}
