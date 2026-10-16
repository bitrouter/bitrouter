//! A real judge dispatch keeps its reserved identity and charge across profiles.

use super::*;
use crate::capture_fixture::{FixtureScope, PersistedHistory};
use bitrouter::acp_trajectory::CanonicalStore;
use bitrouter::core::evolution::jobs::{JobStatus, JudgeJob, JudgeJobs};
use bitrouter::storage::capture::{CaptureDirection, CaptureEvent, CaptureKind};

#[derive(Debug, PartialEq, Eq)]
struct JudgeFacts {
    metering: requests::Model,
    attempt: (i64, Value),
    binding: (i64, Value),
    job: (i64, Value),
}

fn facts(fixture: &Fixture, request_id: &str, job_id: &str) -> Result<JudgeFacts> {
    fixture.runtime.block_on(async {
        let db = bitrouter::db::connect(&database_url(fixture)).await?;
        let owner = EvolutionStore::new(db.clone(), "local")?;
        let lookup = EvolutionStore::new(db.clone(), "internal:checkpoint-judge-costs")?;
        let result = JudgeFacts {
            metering: requests::Entity::find_by_id(request_id)
                .one(&db)
                .await?
                .context("judge metering missing")?,
            attempt: owner
                .get("judge_attempt", request_id)
                .await?
                .context("attempt missing")?,
            binding: lookup
                .get("judge_request_owner", request_id)
                .await?
                .context("binding missing")?,
            job: owner
                .get("judge_job", job_id)
                .await?
                .context("job missing")?,
        };
        db.close().await?;
        Ok(result)
    })
}

fn seed_judge(fixture: &Fixture, raw: &str) -> Result<JudgeJob> {
    fixture.runtime.block_on(async {
        let config = bitrouter::config::parse_with(raw, |_| None)?;
        let assembled = bitrouter::assemble::build_app_with_path(&config, Some(&fixture.config)).await?;
        let identity = session("judge");
        let history = PersistedHistory::new(assembled.db.clone(), FixtureScope {
            owner: identity.owner.clone(), source: identity.source.clone(),
            controller_instance_id: None, route_scope_id: None,
        }).await?;
        for (direction, kind, method, call_id, payload) in [
            (CaptureDirection::Client, CaptureKind::Request, "session/new", Some(1), json!({"cwd":"/fixture"})),
            (CaptureDirection::Client, CaptureKind::Response, "session/new", Some(1), json!({"result":{"sessionId":"judge"}})),
            (CaptureDirection::Client, CaptureKind::Request, "session/prompt", Some(2),
                json!({"sessionId":"judge","prompt":[{"type":"text","text":"Fix the parser and verify the result."}]})),
            (CaptureDirection::Agent, CaptureKind::Notification, "session/update", None,
                json!({"sessionId":"judge","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"The change is ready for review."}}})),
        ] {
            history.append("judge", CaptureEvent { direction, kind, call_id, method: method.into(), payload }).await?;
        }
        let canonical = CanonicalStore::new(assembled.db.clone());
        let head = canonical.transcript(&identity).await?.session.head;
        let checkpoint = canonical.freeze_checkpoint(&identity, head).await?;
        let jobs = JudgeJobs::new(EvolutionStore::new(assembled.db.clone(), "local")?);
        let job = jobs.enqueue(&identity, &checkpoint.checkpoint_id, "fixture:strong", None).await?;
        let pipeline = assembled.app.language_model().context("model pipeline missing")?;
        // A malformed rubric still consumed one provider request. Exercise real
        // reservation/admission/settlement without manufacturing private rows.
        let error = jobs.run(&job.job_id, pipeline).await.err().context("invalid rubric accepted")?;
        ensure!(format!("{error:#}").contains("judge did not return the rubric JSON contract"));
        pipeline.drain_required_pending_settlements().await?;
        let job = jobs.get(&job.job_id).await?;
        ensure!(job.status == JobStatus::Failed && job.request_ids.len() == 1);
        ensure!(job.lease_owner.is_none() && job.lease_until.is_none());
        let db = assembled.db.clone();
        drop(assembled);
        db.close().await?;
        Ok(job)
    })
}

fn exercise_judge(fixture: &mut Fixture, full: &Path, minimum: &Path) -> Result<()> {
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
        Mock::given(method("POST")).and(path("/chat/completions"))
            .and(body_partial_json(json!({"model":"strong"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"judge-fixture-response","object":"chat.completion","model":"strong",
                "choices":[{"index":0,"message":{"role":"assistant","content":"invalid rubric"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
            }))).mount(&upstream).await;
        upstream
    });
    let raw = seed::configuration(fixture, &upstream.uri(), false)?;
    let job = seed_judge(fixture, &raw)?;
    let id = job
        .request_ids
        .first()
        .context("reserved request missing")?;
    ensure!(id.starts_with("brjudge_"));
    ensure!(upstream_count(fixture, &upstream)? == 1);
    let original = facts(fixture, id, &job.job_id)?;
    ensure!(original.metering.model_id == "strong" && original.metering.error.is_none());
    ensure!(original.metering.prompt_tokens == 10 && original.metering.completion_tokens == 2);
    ensure!(original.metering.estimated_charge_micro_usd == 28);
    ensure!(original.attempt.1["identity"] == serde_json::to_value(session("judge"))?);
    ensure!(original.attempt.1["request_id"] == *id && original.attempt.1["job_id"] == job.job_id);
    ensure!(
        original.attempt.1["outcome"] == "completed"
            && original.attempt.1["started_at"].is_string()
            && original.attempt.1["settled_at"].is_string()
    );
    ensure!(
        original.attempt.1["hops"]
            .as_array()
            .context("hops missing")?
            .len()
            == 1
    );
    ensure!(
        original.attempt.1["hops"][0]["provider"] == "fixture"
            && original.attempt.1["hops"][0]["model"] == "strong"
            && original.attempt.1["hops"][0]["status"] == "completed"
    );
    ensure!(original.binding.1 == json!({"request_id":id,"owner":"local"}));

    for (binary, identity, require_auth) in [
        (full, &full_identity, false),
        (minimum, &minimum_identity, false),
        (minimum, &minimum_identity, true),
        (full, &full_identity, false),
    ] {
        let mut config: Value = serde_saphyr::from_str(&raw)?;
        config["server"]["skip_auth"] = json!(!require_auth);
        fs::write(&fixture.config, serde_saphyr::to_string(&config)?)?;
        let (pid, listen) = start(fixture, binary, identity)?;
        if require_auth {
            let (status, _) = request(
                fixture,
                &listen,
                "unauthenticated-control",
                "unbound",
                false,
            )?;
            ensure!(status == 401, "authentication control returned {status}");
        }
        let (status, body) = request(fixture, &listen, id, "unbound", false)?;
        ensure!(
            status >= 400 && body.get("error").is_some(),
            "reserved request accepted: {status}: {body}"
        );
        ensure!(
            upstream_count(fixture, &upstream)? == 1,
            "reserved request dispatched again"
        );
        ensure!(
            facts(fixture, id, &job.job_id)? == original,
            "rejection changed judge facts"
        );
        fixture.stop_resident(pid)?;
        ensure!(
            facts(fixture, id, &job.job_id)? == original,
            "shutdown changed judge facts"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires separately built immutable full and minimum gateway artifacts"]
fn reserved_judge_charge_survives_full_minimum_full() -> Result<()> {
    let full = artifact_path("BITROUTER_TEST_FULL_BINARY")?;
    let minimum = artifact_path("BITROUTER_TEST_GATEWAY_BINARY")?;
    ensure!(full != minimum);
    let mut fixture = Fixture::new()?;
    let result = exercise_judge(&mut fixture, &full, &minimum);
    let cleanup = fixture.cleanup();
    result
        .with_context(|| format!("judge profile transition failed\n{}", fixture.diagnostics()))?;
    cleanup.with_context(|| format!("judge cleanup failed\n{}", fixture.diagnostics()))?;
    Ok(())
}
