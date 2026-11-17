//! Mature gateway exchange. Client-driven tenant Policy updates are exercised in
//! tenancy::tests::execution::model_policy, including real revision-bound inference.
use bitrouter::eval::{EvalService, admission::SubmissionPrincipal, store::EvalStore};
use bitrouter_ai::eval::{
    EVAL_RESULT_SCHEMA_VERSION, EVAL_SUBJECT_SCHEMA_VERSION, EvalScope, EvalSubject,
    EvalSubjectIdentity, EvalVerdict, EvaluationResult, EvaluatorIdentity, EvaluatorKind,
    FreezeRequest,
};
use bitrouter_sdk::gateway::GatewayClient;
use std::collections::{BTreeMap, BTreeSet};
use tokio_util::task::AbortOnDropHandle;

fn subject() -> anyhow::Result<EvalSubject> {
    Ok(EvalSubject {
        schema_version: EVAL_SUBJECT_SCHEMA_VERSION,
        identity: EvalSubjectIdentity::GatewayGeneric {},
        eval_id: "client-eval".into(),
        scope: EvalScope::Task,
        subject_id: "client-task".into(),
        cohort: None,
        holdout: false,
        decisions: Vec::new(),
        requested_dimensions: BTreeSet::new(),
        evidence: Vec::new(),
        evidence_digest: bitrouter::storage::evaluation::evidence_digest(&[])?,
        observed_at: "2026-11-18T00:00:00Z".into(),
    })
}

#[tokio::test]
async fn gateway_sdk_exchange_preserves_current_versions_and_refuses_tenant_profiles()
-> anyhow::Result<()> {
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = EvalStore::new(db.clone());
    let service = EvalService::new(store.clone(), bitrouter::config::EvalConfig::default());
    let router = bitrouter::eval::api::router(service, db.clone(), true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let checked: anyhow::Result<()> = async {
        let client = GatewayClient::new(&endpoint, None)?;
        let input = subject()?;
        client.put_eval_subject(&input).await?;
        assert_eq!(client.eval_subject(&input.eval_id).await?, input);
        let result = EvaluationResult {
            schema_version: EVAL_RESULT_SCHEMA_VERSION,
            eval_id: input.eval_id.clone(),
            evidence_digest: input.evidence_digest.clone(),
            evaluator: EvaluatorIdentity {
                authority_id: "client".into(),
                evaluator_id: "external-check".into(),
                kind: EvaluatorKind::Generic,
                version: "1".into(),
                config_digest: format!("sha256:{}", "a".repeat(64)),
            },
            verdict: EvalVerdict::Pass,
            metrics: BTreeMap::new(),
            hard_violations: Vec::new(),
            confidence_ppm: None,
            evidence_refs: Vec::new(),
            decision_credit: BTreeMap::new(),
            idempotency_key: "client-result".into(),
            submitted_at: "2026-11-18T00:01:00Z".into(),
        };
        assert_eq!(
            client.submit_eval_result(&result).await?.status,
            bitrouter_ai::eval::AdmissionStatus::Admitted
        );
        let frozen = client
            .freeze_eval_snapshot(&FreezeRequest::default())
            .await?;
        assert_eq!(frozen.entries.len(), 1);
        assert_eq!(client.eval_snapshot(&frozen.evidence_root).await?, frozen);
        let exported =
            bitrouter::eval::compiler::EvalEvidenceSnapshot::load(&store, &frozen.evidence_root)
                .await?;
        assert_eq!(
            exported.records[0].subject.identity,
            EvalSubjectIdentity::GatewayGeneric {}
        );
        let http = reqwest::Client::builder().no_proxy().build()?;
        let mut old = serde_json::to_value(&input)?;
        old["schema_version"] = 1.into();
        assert!(
            http.post(format!("{endpoint}/v1/evals/subjects"))
                .json(&old)
                .send()
                .await?
                .status()
                .is_client_error()
        );
        let mut tenant = input.clone();
        tenant.eval_id = "wrong-endpoint".into();
        tenant.identity = EvalSubjectIdentity::TenantGeneric {
            installation_id: "installation".try_into()?,
            tenant_id: "tenant".try_into()?,
            execution: None,
        };
        assert_eq!(
            http.post(format!("{endpoint}/v1/evals/subjects"))
                .json(&tenant)
                .send()
                .await?
                .status(),
            reqwest::StatusCode::BAD_REQUEST
        );
        assert_eq!(client.eval_status().await?.subjects, 1);
        Ok(())
    }
    .await;
    server.abort();
    let stopped = server.await;
    checked?;
    anyhow::ensure!(stopped.is_err_and(|error| error.is_cancelled()));
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn generic_results_only_freeze_their_own_immutable_subject() -> anyhow::Result<()> {
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = EvalStore::new(db.clone());
    let service = EvalService::new(store.clone(), bitrouter::config::EvalConfig::default());
    let input = subject()?;
    store.insert_subject(&input).await?;
    let result = EvaluationResult {
        schema_version: EVAL_RESULT_SCHEMA_VERSION,
        eval_id: input.eval_id.clone(),
        evidence_digest: input.evidence_digest.clone(),
        evaluator: EvaluatorIdentity {
            authority_id: "client".into(),
            evaluator_id: "external-check".into(),
            kind: EvaluatorKind::Generic,
            version: "1".into(),
            config_digest: format!("sha256:{}", "a".repeat(64)),
        },
        verdict: EvalVerdict::Pass,
        metrics: BTreeMap::new(),
        hard_violations: Vec::new(),
        confidence_ppm: None,
        evidence_refs: Vec::new(),
        decision_credit: BTreeMap::new(),
        idempotency_key: "generic-result".into(),
        submitted_at: "2026-11-18T00:01:00Z".into(),
    };
    service
        .submit(result, SubmissionPrincipal::LocalOperator)
        .await?;
    let frozen = store.freeze_snapshot("2026-11-18T00:02:00Z").await?;
    assert_eq!(frozen.entries.len(), 1);
    let export =
        bitrouter::eval::compiler::EvalEvidenceSnapshot::from_manifest(&store, frozen).await?;
    assert_eq!(export.records[0].subject, input);
    db.close().await?;
    Ok(())
}
