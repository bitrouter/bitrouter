//!034 publication is a separate obligation from032 terminal settlement.
//! Guarded parent correlation remains fenced; an HTTP mapping is not StartProof.
use super::*;
use crate::{
    policy_lock::{PolicyDefinition, PolicyLock},
    tenancy::{Service, command::Command, resources::Resources, store::StoredResponse},
    trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy},
};
use bitrouter_ai::tenancy::{
    authority::ResourceKind,
    configuration::{LearningMode, ResourceRef},
};

pub(super) async fn guarded(provider: &Server, mode: LearningMode) -> Result<Ready> {
    let (mut fixture, mut route, slot) = provision_protocol(&provider.url, "responses").await?;
    fixture.service.shutdown().await?;
    let policy = ResourceRef {
        resource_kind: ResourceKind::Policy,
        resource_id: "http-guard".try_into()?,
        ..route.clone()
    };
    route.revision_id = "r2".try_into()?;
    let mut document = PolicyLock::default();
    document.policies.insert(
        "selected".into(),
        PolicyDefinition {
            tiers: [(
                "economy".into(),
                bitrouter_ai::routing::PolicyModelTarget::model_only("mock:model-a".into()),
            )]
            .into(),
            default_tier: Some("economy".into()),
            progress_guard: Some(ProgressGuardPolicy {
                escalation_tier: "economy".into(),
                protected_tiers: ["economy".into()].into(),
                max_consecutive_unprotected: None,
                max_same_projection_unprotected: None,
                max_recovery_count: None,
                max_episode_requests: Some(1),
                max_episode_elapsed_ms: None,
                max_episode_cost_micro_usd: None,
                hold_for_requests: 1,
                incomplete_history: IncompleteHistoryAction::Observe,
            }),
            ..Default::default()
        },
    );
    let resources = Resources::with_key(fixture.installation.clone(), [29; 32])?;
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format":1,"installation_id":fixture.installation,"resources":[
            {"reference":policy,"definition":{"kind":"policy","name":"selected","document":document}},
            {"reference":route,"definition":{"kind":"route","selector":"bitrouter/guarded","policy":policy,
                "routing":{"providers":{"mock":{"api_base":provider.url,"api_protocol":[{"*":"responses"}],"models":[{"id":"model-a"}]}},
                    "routers":{"guarded":{"selection":{"kind":"policy","policy":"selected","base_model":"mock:model-a"}}},
                    "trajectory":{"enabled":true}},
                "credentials":[{"provider":"mock","account":"default","credential":slot}]}}
        ]
    }))?).await?;
    let snapshot = crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        Default::default(),
        snapshot,
        Arc::new(resources),
    )
    .await?;
    let mut input = fixture.configuration_input("http-guarded-configuration")?;
    input.document.routes = vec![route.clone()];
    input.document.policy = Some(policy);
    input.document.learning.mode = mode;
    let revision = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(revision) => revision,
        _ => anyhow::bail!("expected guarded configuration"),
    };
    let issued = fixture.issue("guarded-http-caller").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        issued.secret.as_deref().context("caller secret")?,
        &revision.revision_id,
    )?;
    Ok(Ready {
        fixture,
        headers,
        route,
        revision,
    })
}

async fn count(ready: &Ready, table: &str, predicate: &str) -> Result<i64> {
    let db = &ready.fixture.db;
    Ok(db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            format!("SELECT COUNT(*) AS n FROM {table} WHERE {predicate}"),
        ))
        .await?
        .context("count")?
        .try_get("", "n")?)
}

#[tokio::test]
async fn guarded_http_issue_settles_each_obligation_and_parent_resume_stays_fenced() -> Result<()> {
    for (mode, stream) in [(LearningMode::Off, false), (LearningMode::Observe, true)] {
        let mut provider = Server::start().await?;
        let ready = guarded(&provider, mode).await?;
        let outcome: Result<()> = async {
            let (status, bytes) = send(&ready, ready.headers.clone(), body(stream, None)).await?;
            ensure!(
                status == StatusCode::OK,
                "guarded issuance {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
            let public = public_id(&bytes, stream)?;
            provider.wire().await?;
            ensure!(row_count(&ready).await? == 1);
            ensure!(count(&ready, "trajectory_requests", "status <> 'started'").await? == 1);
            ensure!(count(&ready, "trajectory_requests", "status = 'started'").await? == 0);
            ensure!(count(&ready, "trajectory_events", "1=1").await? >= 3);
            let observed = i64::from(mode != LearningMode::Off);
            for table in [
                "trajectory_outbox",
                "eval_subjects",
                "eval_results",
                "eval_admission_events",
            ] {
                ensure!(
                    count(&ready, table, "1=1").await? == observed,
                    "unexpected {table}"
                );
            }
            ensure!(count(&ready, "trajectory_outbox", "delivered_at IS NULL").await? == 0);
            //032 has no trusted HTTP parent adapter yet. Do not reinterpret the
            // gateway ID as an ambient trajectory request or relax that fence.
            let (status, _) =
                send(&ready, ready.headers.clone(), body(false, Some(&public))).await?;
            ensure!(!status.is_success());
            ensure!(provider.state.calls.load(Ordering::SeqCst) == 1);
            ensure!(row_count(&ready).await? == 1);
            ensure!(count(&ready, "trajectory_requests", "1=1").await? == 1);
            ready.fixture.service.pipelines.close_and_drain().await?;
            ensure!(ready.fixture.service.current().is_ok());
            Ok(())
        }
        .await;
        ready.fixture.service.shutdown().await?;
        provider.close().await?;
        outcome?;
    }
    Ok(())
}
