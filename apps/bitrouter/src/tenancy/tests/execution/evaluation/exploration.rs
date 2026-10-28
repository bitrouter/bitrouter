//! Active experiment authority is distinct from retaining a valid policy artifact.

use super::*;
use crate::{
    gateway::{
        observability::decision::PendingEvalDecisionStore,
        routing::{
            experiment_identity::{
                HISTORY_OPTIMIZER_ID, HISTORY_OPTIMIZER_VERSION, experiment_id,
                explore_compiler_config_digest, treatment_context_digest,
            },
            exploration::{OptimizationGate, PolicyOptimizationState, RouteExploration},
            workflow::predictive::compiled_predictor_contract,
        },
    },
    policy_lock::{
        CertificateSource, CompilerIdentity, PolicyArtifact, PolicyCertificate, PolicyDefinition,
        PolicyLock, PolicyRuntime, PromotionVerdict, RouteOwner, semantic_digest,
        validate_document,
    },
};
use bitrouter_ai::routing::PolicyModelTarget;

const MODES: [LearningMode; 3] = [
    LearningMode::Off,
    LearningMode::Observe,
    LearningMode::Explore,
];

#[derive(Clone, Copy, Debug)]
enum Case {
    UnselectedExperiment,
    InertReference,
}

impl Case {
    fn binds_policy(self) -> bool {
        !matches!(self, Self::InertReference)
    }
}

/// The route-less optimizer certificate verifies the experiment's derived ID,
/// treatment, parent, compiler configuration and evidence root together.
fn signed_experiment(name: &str) -> Result<PolicyLock> {
    const PARENT: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const EVIDENCE: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const KEY: &str = "semantic_route/v1|code:debugging|implement|normal";
    let gate = OptimizationGate {
        objective: Default::default(),
        minimum_tasks_per_arm: 3,
        maximum_challenger_tasks: 20,
        minimum_pass_rate_ppm: 900_000,
        evaluator_config_digest: None,
    };
    let context = treatment_context_digest(name, KEY, "economy", "challenger", 1_000_000, &gate)?;
    let compiler_digest = explore_compiler_config_digest(name, "challenger", 1_000_000, &gate)?;
    let active = RouteExploration {
        experiment_id: experiment_id(PARENT, &context)?,
        target_request_key: KEY.into(),
        champion_tier: "economy".into(),
        challenger_tier: "challenger".into(),
        challenger_exposure_ppm: 1_000_000,
        gate,
    };
    let policy = PolicyDefinition {
        tiers: [
            (
                "economy".into(),
                PolicyModelTarget::model_only("mock:model-a".into()),
            ),
            (
                "challenger".into(),
                PolicyModelTarget::model_only("mock:model-b".into()),
            ),
        ]
        .into(),
        default_tier: Some("economy".into()),
        predictor: Some(compiled_predictor_contract()),
        optimization: Some(PolicyOptimizationState {
            active: Some(active),
            rejections: Vec::new(),
        }),
        ..Default::default()
    };
    let certificate = PolicyCertificate {
        classifier_digest: None,
        owner: RouteOwner::Compiler,
        selected_tier: "economy".into(),
        baseline_tier: Some("economy".into()),
        source: CertificateSource::Mixed,
        eligible_episodes: 1,
        independent_tasks: 1,
        quality: None,
        economics: None,
        latency: None,
        critical_violations: 0,
        verdict: PromotionVerdict::Experiment,
        evaluator_config_digest: None,
        compiler_config_digest: compiler_digest.clone(),
        evidence_digest: EVIDENCE.into(),
    };
    let document = PolicyLock {
        artifact: Some(PolicyArtifact {
            parent_digest: Some(PARENT.into()),
            evidence_root: EVIDENCE.into(),
            eval_snapshot_root: Some(EVIDENCE.into()),
            source_snapshot_time_unix_ms: 1,
            compiler: CompilerIdentity {
                id: HISTORY_OPTIMIZER_ID.into(),
                version: HISTORY_OPTIMIZER_VERSION,
                config_digest: compiler_digest,
            },
        }),
        policies: [(name.into(), policy)].into(),
        certificates: [(name.into(), [(KEY.into(), certificate)].into())].into(),
        ..Default::default()
    };
    validate_document(&document).context("active experiment fixture must be valid")?;
    // A valid-looking digest with the wrong parent/treatment identity must fail;
    // otherwise a fixture could accidentally bypass signed experiment validation.
    let mut corrupted = document.clone();
    corrupted
        .policies
        .get_mut(name)
        .and_then(|policy| policy.optimization.as_mut())
        .and_then(|optimization| optimization.active.as_mut())
        .context("fixture active experiment")?
        .experiment_id = PARENT.into();
    ensure!(
        validate_document(&corrupted).is_err(),
        "fixture must bind the experiment identity"
    );
    Ok(document)
}

fn document(case: Case) -> Result<PolicyLock> {
    let name = if matches!(case, Case::UnselectedExperiment) {
        "unused"
    } else {
        "selected"
    };
    let mut document = signed_experiment(name)?;
    if matches!(case, Case::UnselectedExperiment) {
        document.policies.insert(
            "selected".into(),
            PolicyDefinition {
                tiers: [(
                    "economy".into(),
                    PolicyModelTarget::model_only("mock:model-a".into()),
                )]
                .into(),
                default_tier: Some("economy".into()),
                ..Default::default()
            },
        );
    }
    validate_document(&document)?;
    Ok(document)
}

#[tokio::test]
async fn selected_active_experiment_is_fenced_in_every_learning_mode() -> Result<()> {
    for mode in MODES {
        let provider = wiremock::MockServer::start().await;
        let url = format!("{}/v1", provider.uri());
        let (fixture, mut route, slot) = provision_protocol(&url, "chat_completions").await?;
        // The learning mode is a valid retained configuration choice. It cannot
        // authorize an unguarded experiment in a subsequently imported route.
        let mut input = fixture.configuration_input("experiment-mode")?;
        input.document.routes = vec![route.clone()];
        input.document.learning.mode = mode;
        let configured = fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Configuration(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?;
        let StoredResponse::Configuration(configured) = configured else {
            anyhow::bail!("expected retained learning mode");
        };
        ensure!(configured.document.learning.mode == mode);
        fixture.service.shutdown().await?;
        let before = retained_bytes(&fixture).await?;
        let document = signed_experiment("selected")?;
        let routing = json!({
            "providers":{"mock":{"api_base":url,"api_protocol":[{"*":"chat_completions"}],
                "models":[{"id":"model-a"},{"id":"model-b"}]}},
            "routers":{"observed":{"selection":{"kind":"policy","policy":"selected","base_model":"mock:model-a"}}}
        });
        let mut config: crate::config::Config = serde_json::from_value(routing.clone())?;
        config.inherit_defaults = false;
        // The signed artifact is valid for the ordinary runtime. The owned
        // import boundary specifically requires a retained progress guard.
        let ordinary = PolicyRuntime::from_document(
            &config,
            Some(document.clone()),
            PendingEvalDecisionStore::default(),
            None,
        )?;
        drop(ordinary);
        ensure!(
            crate::tenancy::trajectory_profile::retained_guarded(&config, &document, "selected")
                .is_err()
        );
        let policy = ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: ResourceKind::Policy,
            resource_id: "observed-policy".try_into()?,
            revision_id: "r1".try_into()?,
        };
        route.revision_id = "r2".try_into()?;
        let resources =
            crate::tenancy::resources::Resources::with_key(fixture.installation.clone(), [29; 32])?;
        let imported = resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
            "format":1, "installation_id":fixture.installation, "resources":[
                {"reference":policy,"definition":{"kind":"policy","name":"selected","document":document}},
                {"reference":route,"definition":{"kind":"route","selector":"bitrouter/observed",
                    "policy":policy,"routing":routing,
                    "credentials":[{"provider":"mock","account":"default","credential":slot}]}}
            ]
        }))?).await;
        let error = imported
            .err()
            .context("unguarded experiment import was accepted")?;
        ensure!(error.to_string() == "invalid imported route closure");
        ensure!(
            retained_bytes(&fixture).await? == before,
            "rejected {mode:?} import changed retained policy/route/configuration bytes"
        );
        ensure!(
            provider
                .received_requests()
                .await
                .context("provider recording")?
                .is_empty(),
            "rejected {mode:?} import sent provider work"
        );
    }
    Ok(())
}

#[tokio::test]
async fn unselected_active_experiment_does_not_block_static_policy() -> Result<()> {
    for mode in MODES {
        exercise(Case::UnselectedExperiment, mode).await?;
    }
    Ok(())
}

#[tokio::test]
async fn inert_active_policy_reference_does_not_block_fixed_route() -> Result<()> {
    for mode in MODES {
        exercise(Case::InertReference, mode).await?;
    }
    Ok(())
}

async fn exercise(case: Case, mode: LearningMode) -> Result<()> {
    let document = document(case)?;
    let document_bytes = serde_json::to_vec(&document)?;
    let digest = semantic_digest(&document)?;
    let state = Provider {
        calls: Arc::new(AtomicUsize::new(0)),
        fail: false,
        stream: false,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let router = Router::new()
        .route("/v1/chat/completions", post(provider))
        .with_state(state.clone());
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
    }));
    let (fixture, revision) =
        policy_fixture_with_document(&url, mode, document, case.binds_policy())
            .await
            .with_context(|| {
                format!("{case:?}/{mode:?}: provision retained policy configuration")
            })?;
    let checked: Result<()> = async {
        let credential = fixture
            .issue("experiment-execution")
            .await
            .context("issue execution credential")?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let headers = headers(
            &fixture,
            credential.secret.as_deref().context("secret")?,
            &revision,
        )?;
        let before = retained_bytes(&fixture).await?;
        // Audit the exact stored projection without warming the Service cache.
        // Admission failures then distinguish decode/weight errors from a slot
        // still owned by an earlier invocation. Keep the fixture's one active
        // execution and zero queued work; capacity must not be relaxed here.
        let heap_bytes = retained_heap_bytes(&fixture, &revision).await?;
        let execution = admitted(&fixture, &headers)
            .await
            .with_context(|| format!("cold execution admission; retained heap {heap_bytes} bytes"))?;
        let store = execution.eval_store();
        ensure!(execution.failure().is_none());
        if case.binds_policy() {
            let policy = execution.route().policy().context("bound policy")?;
            ensure!(policy.name() == "selected");
            ensure!(serde_json::to_vec(policy.document())? == document_bytes);
            ensure!(semantic_digest(policy.document())? == digest);
            // The retained artifact remains valid for the ordinary runtime;
            // its unselected experiment does not change the selected route.
            let config = policy
                .configuration()
                .map_err(|code| anyhow::anyhow!("policy configuration: {code:?}"))?;
            let ordinary = PolicyRuntime::from_document(
                &config,
                Some(policy.document().clone()),
                PendingEvalDecisionStore::default(),
                None,
            )?;
            drop(ordinary);
        } else {
            ensure!(
                execution.route().policy().is_none(),
                "inert reference became a route selection"
            );
            let stored = crate::tenancy::configuration_store::configuration(
                &fixture.db,
                &fixture.tenant,
                &revision,
            )
            .await?
            .context("retained configuration")?;
            let reference = stored
                .revision
                .document
                .policy
                .as_ref()
                .context("inert policy retained")?;
            ensure!(
                stored.dependencies.contains(reference),
                "inert policy lost its ownership dependency"
            );
        }
        drop(execution.pipeline().context("construct static tenant pipeline")?);
        ensure!(execution.failure().is_none());
        drop(execution);
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained())
            .await
            .context("unsubmitted pipeline probe retained its execution lease")?;

        let router = Router::new()
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ));
        let mut request = Request::post("/v1/chat/completions")
            .body(Body::from(serde_json::to_vec(&json!({"model":MODEL,
                "messages":[{"role":"user","content":"Summarize this paragraph."}]}))?))?;
        *request.headers_mut() = headers.clone();
        let response = tokio::time::timeout(TIMEOUT, router.oneshot(request))
            .await
            .context("HTTP invocation timed out")??;
        let status = response.status();
        let body = tokio::time::timeout(
            TIMEOUT,
            axum::body::to_bytes(response.into_body(), 1024 * 1024),
        )
        .await??;
        ensure!(
            status == StatusCode::OK,
            "static execution failed: {status}; retained heap {heap_bytes} bytes; provider calls {}; body {}",
            state.calls.load(Ordering::SeqCst),
            String::from_utf8_lossy(&body)
        );
        ensure!(String::from_utf8_lossy(&body).contains("observed"));
        // HTTP response completion does not join the registry's background
        // drain. With active=1/queue=0, a second admission can legitimately fail
        // until that owner drops. Require automatic release before the warm
        // probe; a leaked lease fails this bounded assertion without retries,
        // extra capacity, or closing/replacing the registry to force cleanup.
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained())
            .await
            .with_context(|| format!(
                "HTTP invocation retained its execution lease after response; provider calls {}",
                state.calls.load(Ordering::SeqCst)
            ))?;
        // A fresh load from the warm retained cache must still see the exact
        // signed document; refusal must not disable exploration by mutating it.
        if case.binds_policy() {
            let execution = admitted(&fixture, &headers)
                .await
                .with_context(|| format!(
                    "warm execution admission after automatic lease release; retained heap {heap_bytes} bytes"
                ))?;
            let policy = execution.route().policy().context("warm retained policy")?;
            ensure!(serde_json::to_vec(policy.document())? == document_bytes);
            ensure!(semantic_digest(policy.document())? == digest);
        }
        tokio::time::timeout(TIMEOUT, fixture.service.pipelines.close_and_drain())
            .await
            .context("registry drain timed out")??;
        ensure!(state.calls.load(Ordering::SeqCst) == 1);
        ensure!(
            retained_bytes(&fixture).await? == before,
            "execution rewrote retained bytes or digests"
        );
        let subjects = store.list_subjects().await?;
        let observed = matches!(case, Case::UnselectedExperiment) && mode != LearningMode::Off;
        ensure!(subjects.len() == usize::from(observed));
        if let Some(subject) = subjects.first() {
            ensure!(subject.policy_digest == digest);
            let decision = subject
                .decisions
                .first()
                .context("static policy decision")?;
            ensure!(subject.decisions.len() == 1 && decision.selected_tier == "economy");
            ensure!(
                decision.experiment.is_none(),
                "unselected experiment affected static routing"
            );
        }
        Ok(())
    }
    .await;
    let shutdown = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    stop.cancel();
    let joined = tokio::time::timeout(TIMEOUT, task).await;
    checked.with_context(|| format!("{case:?}/{mode:?}"))?;
    shutdown??;
    joined???;
    Ok(())
}

async fn retained_heap_bytes(fixture: &Fixture, revision: &RevisionId) -> Result<usize> {
    let stored =
        crate::tenancy::configuration_store::configuration(&fixture.db, &fixture.tenant, revision)
            .await
            .context("load retained configuration for allocation audit")?
            .context("retained configuration missing during allocation audit")?;
    ensure!(
        stored.revision.document.limits.active_executions == 1
            && stored.revision.document.limits.queued_work == 0,
        "exploration fixture must retain active=1/queue=0"
    );
    let retained = fixture
        .service
        .resources
        .open_execution(&stored)
        .map_err(|code| anyhow::anyhow!("retained projection decode failed: {code:?}"))?;
    retained
        .retained_heap_bytes()
        .map_err(|code| anyhow::anyhow!("retained allocation accounting failed: {code:?}"))
}

async fn retained_bytes(fixture: &Fixture) -> Result<Vec<Vec<String>>> {
    let mut bytes = Vec::new();
    // Fixtures own an isolated database. Read the stored text, not reserialized
    // JSON, so even an otherwise equivalent rewrite is detected.
    for (sql, columns) in [
        (
            "SELECT payload_json, dependency_json, content_digest FROM tenancy_resource_revisions ORDER BY owner_kind, owner_id, resource_kind, resource_id, revision_id",
            &["payload_json", "dependency_json", "content_digest"][..],
        ),
        (
            "SELECT document_json, compiled_json, dependencies_json, content_digest FROM tenancy_configurations ORDER BY tenant_id, revision_id",
            &[
                "document_json",
                "compiled_json",
                "dependencies_json",
                "content_digest",
            ][..],
        ),
    ] {
        for row in fixture
            .db
            .query_all(Statement::from_string(
                fixture.db.get_database_backend(),
                sql.to_owned(),
            ))
            .await?
        {
            bytes.push(
                columns
                    .iter()
                    .map(|column| row.try_get::<String>("", column))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
    }
    Ok(bytes)
}
