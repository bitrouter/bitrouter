//! Real selector -> retained custody -> shared terminal/evaluation handoff.
//! No provider, local recorder, synthetic EvalDecisionRef, or dispatch is used.

use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    eval::{AdmissionStatus, EvalDecisionRef, EvalScope, EvalVerdict},
    gateway::execution::{ExecutionState, TerminalOutcome},
    types::{ModelOperation, ReasoningEffort},
};
use futures::FutureExt as _;

use super::Fixture;
use crate::auth::db::{NewApiKey, insert_api_key};
use crate::gateway::{
    caller::CallerContext,
    observability::decision::{EvalInvocation, PendingEvalDecision, PendingEvalDecisionStore},
    pipeline::{context::PipelineContext, routing::ModelSelector, types::PipelineRequest},
    shared::{
        admitted::SharedAdmission,
        capture::Requirements,
        engine::{Admission, Outbox, TrajectoryBinding},
        ownership::{Registry, Submission},
        store::{Kind, Store, Stored},
        test_database::{EmptyDatabase, combine_results},
    },
};
use crate::metering::shared::SharedMetering;
use crate::policy_lock::{PolicyRuntime, semantic_digest};
use crate::storage::evaluation::shared::SharedEvaluation;
use crate::trajectory::{
    canonical::Canonicalizer,
    correlation::{CorrelatedRequest, TrajectoryRuntime},
    evaluation::decode_eval_decision,
    shared::SharedTrajectory,
    store::TrajectoryStore,
    types::{RequestStatus, TrajectoryEventKind},
};

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_policy_runtime_guard_handoff_survives_pending_cache_removal() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let mut retained_custody: Option<Arc<Registry>> = None;
    // Retain custody and every registered pool outside the cancelled campaign.
    // There are no native workers, pipeline tasks, or process children to join.
    let result = AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(60), async {
        let fixture = Handoff::new(&mut database).await?;
        let custody = Registry::new(fixture.base.engine.clone(), 4)?;
        retained_custody = Some(custody.clone());
        exercise(&fixture, &custody).await
    }))
    .catch_unwind()
    .await;
    let result = match result {
        Ok(result) => result
            .context("policy runtime guard handoff timed out")
            .and_then(|result| result),
        Err(payload) => {
            drop(payload);
            Err(anyhow::anyhow!("policy runtime guard handoff panicked"))
        }
    };
    // The timed future (and every admission/context reference) is gone first.
    let cleanup = match retained_custody {
        Some(custody) => tokio::time::timeout(Duration::from_secs(10), async {
            while custody.pending() > 0 {
                ensure!(
                    custody.reconcile_one().await?,
                    "handoff fixture retained a live custody reference"
                );
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("handoff custody cleanup timed out")
        .and_then(|result| result),
        None => Ok(()),
    };
    database
        .finish(combine_results(result, vec![cleanup]))
        .await
}

struct Handoff {
    base: Fixture,
    raw_key: String,
    runtime: Arc<PolicyRuntime>,
    pending: PendingEvalDecisionStore,
    policy_digest: String,
}

impl Handoff {
    async fn new(database: &mut EmptyDatabase) -> Result<Self> {
        // This constructor includes the parent's real initial publication fix.
        let mut base = Fixture::new(database).await?;
        let head = base.authority.head().await?;
        ensure!(
            head.config_generation > 0,
            "fixture configuration was not published"
        );
        let mut publication = base.authority.configuration(&head).await?;
        let definition = publication
            .policy_lock
            .as_mut()
            .context("published policy document missing")?
            .policies
            .get_mut("fixture-guard")
            .context("published guard policy missing")?;
        for (tier, effort) in [
            ("economy", ReasoningEffort::Low),
            ("strong", ReasoningEffort::High),
        ] {
            definition
                .tiers
                .get_mut(tier)
                .context("fixture tier missing")?
                .effort = Some(effort);
        }
        // Publish the actual effort-bearing document before admission. Do not
        // substitute a different digest in a generic disabled fixture capture.
        base.authority
            .publish(head.config_generation, &publication)
            .await?;
        let config = publication.prepare()?.config;
        let document = publication.policy_lock.context("policy document missing")?;
        ensure!(document.policies.contains_key("unselected-policy"));
        let policy_digest = semantic_digest(&document)?;
        base.requirements = Requirements::from_config(&config, Some(&document))?;
        let requirements = serde_json::to_value(&base.requirements)?;
        ensure!(requirements["trajectory"] == "guarded_policies");
        ensure!(requirements["policy_lock_digest"].as_str() == Some(policy_digest.as_str()));

        // Keep a real bearer secret so SharedAdmission::establish authenticates
        // the same public principal that admitted the execution.
        let raw_key = uuid::Uuid::new_v4().to_string();
        base.key_hash = crate::auth::keys::hash_key(&raw_key);
        base.principal = uuid::Uuid::new_v4().to_string();
        insert_api_key(
            base.authority.store().database(),
            &NewApiKey {
                id: base.principal.clone(),
                key_hash: base.key_hash.clone(),
                user_id: base.owner.clone(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;

        let head = base.authority.head().await?;
        let key = base.authority.keys(&head).await?.active_correlation()?;
        // This is the same marker used by shared assembly. A disconnected local
        // store makes any accidental fallback to local guard persistence fail.
        let trajectory = Arc::new(TrajectoryRuntime::new(
            TrajectoryStore::new(sea_orm::DatabaseConnection::Disconnected),
            Canonicalizer::new(key),
        ));
        let pending = PendingEvalDecisionStore::default();
        let runtime = PolicyRuntime::from_document(
            &config,
            Some(document),
            pending.clone(),
            Some(trajectory),
        )?;
        Ok(Self {
            base,
            raw_key,
            runtime,
            pending,
            policy_digest,
        })
    }

    async fn context(
        &self,
        custody: &Arc<Registry>,
    ) -> Result<(Arc<SharedAdmission>, PipelineContext)> {
        let f = &self.base;
        let admission = match custody
            .reserve()?
            .admit(&Admission {
                requirements: f.requirements.clone(),
                key_hash: &f.key_hash,
                correlation_id: "guard-handoff-caller-correlation",
                idempotency_key: None,
                fingerprint: &"e".repeat(64),
                operation: ModelOperation::Generation,
                revision: f.authority.head().await?.revision(),
            })
            .await?
        {
            Submission::Accepted(admission) => admission,
            Submission::Existing(_) => {
                anyhow::bail!("fresh handoff request unexpectedly duplicated")
            }
        };
        let prompt = super::prompt("real policy runtime guarded handoff");
        // The fixture starts at the named selector seam, after preset resolution.
        let mut request =
            PipelineRequest::new(prompt.model.clone(), CallerContext::anonymous(), prompt);
        request.inbound_protocol = Some(f.protocol.clone());
        request.headers.insert(
            "authorization",
            http::HeaderValue::from_str(&format!("Bearer {}", self.raw_key))?,
        );
        admission.attach(&mut request);
        let mut ctx = PipelineContext::new(request);
        admission.establish(&mut ctx).await?;
        ensure!(ctx.caller().user_id() == f.owner && ctx.caller().api_key_id() == f.principal);
        Ok((admission, ctx))
    }
}

async fn exercise(fixture: &Handoff, custody: &Arc<Registry>) -> Result<()> {
    let f = &fixture.base;
    let (admission, mut ctx) = fixture.context(custody).await?;
    let id = ctx.request_id().to_owned();
    fixture
        .runtime
        .prepare_selection("fixture-guard", None, &mut ctx)
        .await?;
    fixture
        .runtime
        .select_variant("fixture-guard", None, &mut ctx)
        .await?;
    ensure!(
        ctx.model() == "fixture:strong",
        "the durable guard did not escalate the real selection"
    );
    ensure!(
        ctx.require_generation_prompt()?.params.reasoning_effort == Some(ReasoningEffort::High)
    );
    let invocation = ctx
        .extension::<EvalInvocation>()
        .context("selector did not emit its invocation")?;
    let pending = fixture
        .pending
        .peek(&invocation, &f.owner)
        .context("real router pending decision missing")?;
    let correlated = ctx
        .extension::<CorrelatedRequest>()
        .context("shared selector did not retain correlation")?;
    let binding = f.binding(&id).await?;
    ensure!(
        binding.request_id == correlated.request_id && binding.episode_id == correlated.episode_id
    );
    ensure!(binding.request_id != id && pending.request_id == binding.request_id);
    ensure!(pending.policy_digest == fixture.policy_digest);
    ensure!(
        pending.selected_tier == "strong" && pending.selected_effort == Some(ReasoningEffort::High)
    );
    ensure!(
        pending.baseline_tier.as_deref() == Some("economy")
            && pending.baseline_effort == Some(ReasoningEffort::Low)
    );

    let tx = f.observer.begin().await?;
    let trajectory = SharedTrajectory::new(f.authority.store().namespace())?;
    let history = trajectory
        .history_in(&tx, &f.owner, &binding.episode_id)
        .await?;
    let routes = history
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
        .collect::<Vec<_>>();
    ensure!(
        routes.len() == 1,
        "selector did not commit exactly one route intent"
    );
    let route = routes.first().context("guard route missing")?;
    let durable = decode_eval_decision(route)?.context("real route lacks evaluation identity")?;
    ensure!(durable.decision_id == route.event_id);
    pending_matches_route(&pending, &durable)?;
    tx.commit().await?;
    ensure!(f.execution(&id).await?.value.view.attempts.is_empty());

    // Removing the optional local cache must not lose the direct capture already
    // retained by custody. No synthetic PendingEvalDecision or EvalDecisionRef
    // is inserted into this path, and no ready/provider attempt is invented.
    ensure!(fixture.pending.remove(&invocation, &f.owner));
    ensure!(fixture.pending.peek(&invocation, &f.owner).is_none());
    admission.finish_delivery(false).await?;
    let execution = f.execution(&id).await?;
    let capture = serde_json::to_value(
        execution
            .value
            .projection
            .context("terminal capture missing")?,
    )?;
    ensure!(capture["phase"]["state"] == "terminal" && capture["phase"]["route_ready"] == false);
    ensure!(capture["policy"]["state"] == "evaluated" && capture["policy"]["guarded"] == true);
    ensure!(capture["policy"]["digest"].as_str() == Some(fixture.policy_digest.as_str()));
    let captured: EvalDecisionRef =
        serde_json::from_value(capture["policy"]["treatment"]["decision"].clone())?;
    ensure!(
        captured == durable,
        "retained capture changed the actual committed route decision"
    );
    ensure!(capture["policy"]["treatment"]["preset"].as_str() == pending.preset.as_deref());
    ensure!(
        capture["policy"]["treatment"]["observed_at"].as_str()
            == Some(pending.observed_at.as_str())
    );
    ensure!(capture["policy"]["treatment"]["holdout"] == pending.holdout);

    let event_id = format!("gateway-terminal-{id}");
    let event: Stored<Outbox> = f
        .authority
        .store()
        .get(Kind::Outbox, &event_id)
        .await?
        .context("terminal outbox missing")?;
    let failure = TerminalOutcome::Failed {
        code: "pipeline_failed".into(),
    };
    ensure!(event.value.event.execution.attempts.is_empty());
    ensure!(
        matches!(&event.value.event.execution.state, ExecutionState::Terminal { result } if result.outcome == failure)
    );
    f.engine
        .deliver(&event.value.event)
        .await
        .context("deliver the real evaluated guard capture")?;
    verify_projection(f, &binding, &id, &event_id, &durable, &failure).await?;

    // Exact duplicate delivery must preserve all rows, revisions, evaluation
    // admissions and delivery marks, including after the cache was removed.
    let delivered = f.snapshot().await?;
    f.engine.deliver(&event.value.event).await?;
    ensure!(f.snapshot().await? == delivered);
    drop(ctx);
    drop(admission);
    ensure!(custody.reconcile_one().await?);
    ensure!(
        custody.pending() == 0,
        "completed custody was not reclaimed"
    );
    Ok(())
}

fn pending_matches_route(pending: &PendingEvalDecision, durable: &EvalDecisionRef) -> Result<()> {
    // Compare independently produced records. Only the event identity is
    // deliberately canonicalized by Capture::policy; all treatment fields match.
    ensure!(pending.decision_id == format!("{}:{}", pending.request_id, pending.policy));
    ensure!(
        pending.decision_id != durable.decision_id,
        "fixture failed to exercise the two real ID domains"
    );
    ensure!(pending.policy == durable.policy);
    ensure!(pending.policy_digest == durable.policy_digest);
    ensure!(pending.route_projection == durable.route_projection);
    ensure!(pending.request_key == durable.request_key);
    ensure!(
        pending.selected_tier == durable.selected_tier
            && pending.selected_effort == durable.selected_effort
    );
    ensure!(
        pending.baseline_tier == durable.baseline_tier
            && pending.baseline_effort == durable.baseline_effort
    );
    ensure!(
        pending.experiment == durable.experiment
            && pending.route_measurement == durable.route_measurement
    );
    Ok(())
}

async fn verify_projection(
    f: &Fixture,
    binding: &TrajectoryBinding,
    id: &str,
    event_id: &str,
    decision: &EvalDecisionRef,
    failure: &TerminalOutcome,
) -> Result<()> {
    let receipt: Stored<serde_json::Value> = f
        .authority
        .store()
        .get(Kind::ProjectionReceipt, event_id)
        .await?
        .context("projection receipt missing")?;
    ensure!(receipt.status() == "delivered");
    ensure!(receipt.value["projection"]["evaluation"]["sink"] == "trajectory");
    ensure!(receipt.value["completion"]["sink"] == "trajectory");
    let reference = &receipt.value["projection"]["evaluation"]["reference"];
    let subject_id = reference["subject_id"]
        .as_str()
        .context("receipt eval id missing")?;
    let result_id = reference["result_digest"]
        .as_str()
        .context("receipt result id missing")?;
    let tx = f.observer.begin().await?;
    f.authority.fence(&tx).await?;
    let record = SharedMetering::new(f.authority.store().namespace())?
        .get_in(&tx, &f.owner, &f.principal, id)
        .await?
        .context("metering record missing")?;
    ensure!(record.attempts.is_empty() && record.event.execution.attempts.is_empty());
    ensure!(record.request_id.as_deref() == Some(binding.request_id.as_str()));
    ensure!(
        matches!(&record.event.execution.state, ExecutionState::Terminal { result } if &result.outcome == failure)
    );
    let trajectory = SharedTrajectory::new(f.authority.store().namespace())?;
    let request = trajectory
        .request_in(&tx, &f.owner, &binding.request_id)
        .await?
        .context("settled request missing")?;
    ensure!(request.status == RequestStatus::Failed);
    let history = trajectory
        .history_in(&tx, &f.owner, &binding.episode_id)
        .await?;
    let settlements = history
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RequestSettled)
        .collect::<Vec<_>>();
    ensure!(settlements.len() == 1);
    let settlement = settlements.first().context("settlement missing")?;
    ensure!(settlement.request_id.as_deref() == Some(binding.request_id.as_str()));
    ensure!(
        settlement
            .evidence
            .categorical
            .get("settlement.outcome")
            .map(String::as_str)
            == Some("failed")
    );
    let evaluation = SharedEvaluation::new(f.authority.store().namespace())?;
    let subject = evaluation
        .subject_in(&tx, &f.owner, subject_id)
        .await?
        .context("required evaluation subject missing")?;
    ensure!(subject.scope == EvalScope::Episode && subject.subject_id == binding.episode_id);
    ensure!(subject.policy_digest == decision.policy_digest);
    ensure!(subject.decisions.as_slice() == std::slice::from_ref(decision));
    ensure!(
        evaluation
            .subject_in(&tx, &f.owner, &format!("request:{id}"))
            .await?
            .is_none(),
        "guard also projected a duplicate request subject"
    );
    let result = evaluation
        .result_in(&tx, &f.owner, result_id)
        .await?
        .context("required evaluation result missing")?;
    ensure!(result.eval_id == subject.eval_id && result.verdict == EvalVerdict::Inconclusive);
    let admission = evaluation
        .latest_admission_in(&tx, &f.owner, result_id)
        .await?
        .context("required evaluation admission missing")?;
    ensure!(
        admission.status == AdmissionStatus::Admitted
            && admission.authority_id == result.evaluator.authority_id
    );
    let fence = f
        .engine
        .episode_fence(&tx, binding, Store::now(&tx).await?)
        .await?;
    ensure!(fence.value.pending.is_empty());
    tx.commit().await?;
    Ok(())
}
