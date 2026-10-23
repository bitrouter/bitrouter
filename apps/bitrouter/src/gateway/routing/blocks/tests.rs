//! Serving published blocks uses the same records with and without core.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::Value;

use super::control::{BlockState, BlockStatus, ControlState, EnrollmentBatch};
use super::fixtures::{Fixture, definition, fixture_with_persistence, request, session};
use super::store::{BlockPolicyStore, CONTROL_KEY, CONTROL_KIND, SESSION_KIND, SessionEnrollment};
use crate::storage::evolution::EvolutionStore;
use crate::storage::execution::DecisionContext;

async fn published(fixture: &Fixture, selector: &str, candidate: &str) -> Result<ControlState> {
    // Controlled persisted adoption, as in the original dispatch regression.
    // This fixture does not claim promotion from empirical feedback.
    let definition = definition(selector, candidate);
    let config = fixture.assembled.routing_table.snapshot_config();
    let policies = fixture.assembled.policy_runtime.routing_snapshot();
    let routing_config_digest =
        super::catalog::block_digest(&config, &policies, &definition).await?;
    let state = ControlState {
        generation: 1,
        blocks: BTreeMap::from([(
            definition.block_id.clone(),
            BlockState {
                definition,
                revision: "adopted-revision".into(),
                parent_revision: Some("baseline-revision".into()),
                experiment_id: "published-experiment".into(),
                predecessor_experiment_id: None,
                baseline_ancestry: Vec::new(),
                status: BlockStatus::Adopted,
                plan: None,
                routing_config_digest,
                batch: EnrollmentBatch::default(),
                assigned_challenger_sessions: 0,
                last_exposure_ppm: 500_000,
            },
        )]),
        ..ControlState::default()
    };
    EvolutionStore::new(fixture.assembled.db.clone(), "local")?
        .initialize(CONTROL_KIND, CONTROL_KEY, None, &state)
        .await?;
    Ok(state)
}

async fn dispatch(
    fixture: &Fixture,
    session_id: &str,
    request_id: &str,
    model: &str,
) -> Result<()> {
    let identity = session(fixture, session_id, "fixture").await?;
    fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .execute(request(&identity, request_id, model)?)
        .await?;
    Ok(())
}

#[tokio::test]
async fn persisted_adoption_survives_gateway_reassembly_and_preserves_replay_protection()
-> Result<()> {
    for named in [false, true] {
        let fixture = fixture_with_persistence(named, true).await?;
        let (selector, candidate) = if named {
            ("@coding", "@candidate")
        } else {
            ("coding", "candidate")
        };
        let state = published(&fixture, selector, candidate).await?;
        dispatch(&fixture, "before-restart", "first-request", selector).await?;
        let config = fixture.assembled.routing_table.snapshot_config();
        let Fixture {
            assembled,
            upstream,
            _home,
        } = fixture;
        drop(assembled);
        let assembled = crate::assemble::build_app_with_path(
            &config,
            Some(&_home.path().join("bitrouter.yaml")),
        )
        .await?;
        let fixture = Fixture {
            assembled,
            upstream,
            _home,
        };
        dispatch(&fixture, "after-restart", "second-request", selector).await?;
        let policy_store = BlockPolicyStore::new(fixture.assembled.db.clone(), "local")?;
        assert_eq!(
            serde_json::to_value(policy_store.state().await?)?,
            serde_json::to_value(state)?
        );
        let identity = crate::storage::session_identity::SessionIdentity {
            owner: "local".into(),
            source: "fixture".into(),
            native_session_id: "after-restart".into(),
        };
        let intents = policy_store.intents(&identity).await?;
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].selected_route, candidate);
        assert_eq!(intents[0].reason, "adopted_baseline");
        assert!(
            fixture
                .assembled
                .app
                .language_model()
                .context("pipeline missing")?
                .execute(request(&identity, "second-request", selector)?)
                .await
                .is_err()
        );
        let requests = fixture
            .upstream
            .received_requests()
            .await
            .context("requests missing")?;
        assert_eq!(requests.len(), 2);
        for request in requests {
            let body: Value = serde_json::from_slice(&request.body)?;
            assert_eq!(body["model"], "cheap");
        }
    }
    Ok(())
}

#[tokio::test]
async fn published_adoption_retains_existing_baseline_membership() -> Result<()> {
    let fixture = fixture_with_persistence(false, false).await?;
    published(&fixture, "coding", "candidate").await?;
    let identity = session(&fixture, "existing-baseline", "fixture").await?;
    let enrollment = SessionEnrollment {
        identity: identity.clone(),
        first_watermark: 1,
        first_request_id: "original".into(),
        context: DecisionContext {
            selector: "coding".into(),
            fingerprint: "original".into(),
        },
        assignments: BTreeMap::new(),
        block_experiments: Some(BTreeMap::from([(
            "coding-block".into(),
            "published-experiment".into(),
        )])),
        adopted_blocks: BTreeMap::new(),
        monitoring: BTreeMap::new(),
        admission_reason: "session_already_started".into(),
    };
    let key = identity.key()?;
    EvolutionStore::new(fixture.assembled.db.clone(), "local")?
        .initialize(SESSION_KIND, &key, Some(key.clone()), &enrollment)
        .await?;
    fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?
        .execute(request(&identity, "retained-baseline", "coding")?)
        .await?;
    let policy_store = BlockPolicyStore::new(fixture.assembled.db.clone(), "local")?;
    assert_eq!(
        serde_json::to_value(policy_store.enrollment(&identity).await?)?,
        serde_json::to_value(enrollment)?
    );
    let requests = fixture
        .upstream
        .received_requests()
        .await
        .context("requests missing")?;
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body)?;
    assert_eq!(body["model"], "strong");
    assert_eq!(
        policy_store
            .intents(&identity)
            .await?
            .first()
            .context("intent missing")?
            .reason,
        "retained_preexisting_baseline"
    );
    Ok(())
}

#[tokio::test]
async fn published_adoption_does_not_bypass_changed_route_dependencies() -> Result<()> {
    let fixture = fixture_with_persistence(false, false).await?;
    published(&fixture, "coding", "candidate").await?;
    let mut config = fixture.assembled.routing_table.snapshot_config();
    config
        .models
        .get_mut("candidate")
        .context("candidate missing")?
        .endpoints[0]
        .service_id = "strong".into();
    fixture
        .assembled
        .routing_table
        .replace_prepared_config(config)
        .await?;
    dispatch(&fixture, "changed-route", "changed-request", "coding").await?;
    let requests = fixture
        .upstream
        .received_requests()
        .await
        .context("requests missing")?;
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body)?;
    assert_eq!(body["model"], "strong");
    let identity = crate::storage::session_identity::SessionIdentity {
        owner: "local".into(),
        source: "fixture".into(),
        native_session_id: "changed-route".into(),
    };
    let intents = BlockPolicyStore::new(fixture.assembled.db.clone(), "local")?
        .intents(&identity)
        .await?;
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].reason, "routing_dependency_changed");
    Ok(())
}
