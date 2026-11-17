//! Standalone Eval history is checked before shared incarnation registration.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::eval::{
    EvalDecisionRef, EvalScope, EvalSubject, EvalSubjectIdentity, EvalVerdict, EvaluationResult,
    EvaluatorIdentity, EvaluatorKind, RouteActionCandidate,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::{Value, json};

use crate::gateway::shared::{
    authority::Authority, engine::Engine, schema, store::Store, test_database::EmptyDatabase,
};
use crate::runtime_crypto::DeploymentKey;
use crate::storage::evaluation::{
    canonical_digest, evidence_digest, route_decision_measurement, shared::SharedEvaluation,
};

const OWNER: &str = "standalone-evaluator";
const CREATED_AT: &str = "2026-11-18T00:00:00Z";

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_shared_eval_subject_prejoin() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(60), exercise(&mut database))
        .await
        .context("shared Eval preflight deadline exceeded")
        .and_then(|result| result);
    database.finish(result).await
}

async fn exercise(database: &mut EmptyDatabase) -> Result<()> {
    let db = database.connect(3, Duration::from_secs(3)).await?;
    let namespace = format!("eval-preflight-{}", uuid::Uuid::new_v4());
    let store = Store::new(db.clone(), &namespace, Duration::from_secs(3))?;
    schema::initialize_all(&store).await?;
    let key = rand::random();
    authority(&store, key)?.initialize().await?;
    let shared = SharedEvaluation::new(&namespace)?;
    let isolated = subject("z-isolated")?;

    // No execution, projection, result, or publication can lead a late reader to
    // this old subject. The namespace and schema markers are otherwise current.
    write_raw(&store, &isolated, &legacy_body(&isolated)?).await?;
    let row = db.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT (SELECT count(*) FROM gateway_shared_eval_subjects WHERE namespace=$1) AS subjects, (SELECT count(*) FROM gateway_shared_records WHERE namespace=$1 AND kind IN ('execution','projection_receipt','instance')) AS executions",
        [namespace.clone().into()]
    )).await?.context("missing isolated history counts")?;
    ensure!(row.try_get::<i64>("", "subjects")? == 1 && row.try_get::<i64>("", "executions")? == 0);
    rejected_without_writes(&store, key).await?;
    delete_subject(&store, &isolated.eval_id).await?;

    // A real current writer persists a standalone gateway-generic subject with
    // the unchanged route-measurement schema 2. No retained execution is required.
    insert_current(&store, &shared, &isolated).await?;
    Engine::join(authority(&store, key)?, 30_000).await?;
    let tx = store.begin().await?;
    ensure!(shared.subject_in(&tx, OWNER, &isolated.eval_id).await? == Some(isolated.clone()));
    tx.commit().await?;
    crate::storage::evaluation::validate_result_for_subject(
        &EvaluationResult {
            schema_version: 1,
            eval_id: isolated.eval_id.clone(),
            evidence_digest: isolated.evidence_digest.clone(),
            evaluator: EvaluatorIdentity {
                authority_id: OWNER.into(),
                evaluator_id: "external-check".into(),
                kind: EvaluatorKind::Generic,
                version: "1".into(),
                config_digest: canonical_digest(&"external-criterion")?,
            },
            verdict: EvalVerdict::Pass,
            metrics: Default::default(),
            hard_violations: Vec::new(),
            confidence_ppm: None,
            evidence_refs: Vec::new(),
            decision_credit: Default::default(),
            idempotency_key: "standalone-result".into(),
            submitted_at: CREATED_AT.into(),
        },
        &isolated,
    )?;

    // A different namespace cannot poison this installation's subject scan.
    let foreign = Store::new(
        db,
        &format!("eval-foreign-{}", uuid::Uuid::new_v4()),
        Duration::from_secs(3),
    )?;
    write_raw(&foreign, &isolated, &legacy_body(&isolated)?).await?;
    Engine::join(authority(&store, key)?, 30_000).await?;

    // Put the attacked row beyond the first bounded page of healthy subjects.
    for index in 0..9 {
        insert_current(&store, &shared, &subject(&format!("a-page-{index:02}"))?).await?;
    }
    let last = subject("zz-retained")?;
    write_raw(&store, &last, &legacy_body(&last)?).await?;
    rejected_without_writes(&store, key).await?;

    let current = serde_json::to_string(&last)?;
    let mut tenant = serde_json::to_value(&last)?;
    tenant["identity"] =
        json!({"profile":"tenant_generic", "installation_id":"installation", "tenant_id":"tenant"});
    let tenant: EvalSubject = serde_json::from_value(tenant)?;
    let mut invalid_evidence = serde_json::to_value(&last)?;
    invalid_evidence["evidence_digest"] = format!("sha256:{}", "0".repeat(64)).into();
    for body in [
        serde_json::to_string(&tenant)?,
        serde_json::to_string(&invalid_evidence)?,
        format!(" {current}"),
    ] {
        // Digests match the injected body: a version-only or digest-only check
        // would miss these profile, evidence and canonical-encoding failures.
        write_raw(&store, &last, &body).await?;
        rejected_without_writes(&store, key).await?;
    }
    write_raw(&store, &last, &current).await?;
    Engine::join(authority(&store, key)?, 30_000).await?;
    for assignment in [
        "subject_id='different-indexed-subject'",
        "policy_digest='wrong-indexed-identity'",
        "holdout=false",
        "owner_user_id=' '",
        "created_at='invalid-time'",
    ] {
        write_raw(&store, &last, &current).await?;
        // The assignment comes only from this static fault list.
        let sql = format!(
            "UPDATE gateway_shared_eval_subjects SET {assignment} WHERE namespace=$1 AND eval_id=$2"
        );
        store
            .database()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                sql,
                [store.namespace().into(), last.eval_id.clone().into()],
            ))
            .await?;
        rejected_without_writes(&store, key).await?;
    }
    delete_subject(&store, &last.eval_id).await?;
    insert_current(&store, &shared, &last).await?;
    Engine::join(authority(&store, key)?, 30_000).await?;
    Ok(())
}

fn authority(store: &Store, key: [u8; 32]) -> Result<Authority> {
    Ok(Authority::new(
        store.clone(),
        DeploymentKey::from_bytes(key)?,
    ))
}

fn subject(id: &str) -> Result<EvalSubject> {
    let measurement = route_decision_measurement(
        "standard",
        "mock:model",
        None,
        vec![RouteActionCandidate {
            context: bitrouter_ai::routing::ContextStrategy::Preserve,
            tier: "standard".into(),
            model: "mock:model".into(),
            effort: None,
            logging_probability_ppm: 1_000_000,
        }],
    )?;
    ensure!(measurement.schema_version == 2);
    let subject = EvalSubject {
        schema_version: bitrouter_ai::eval::EVAL_SUBJECT_SCHEMA_VERSION,
        identity: EvalSubjectIdentity::GatewayGeneric {},
        eval_id: id.into(),
        scope: EvalScope::Request,
        subject_id: format!("subject-{id}"),
        cohort: None,
        holdout: true,
        decisions: vec![EvalDecisionRef {
            decision_id: format!("decision-{id}"),
            policy: "auto".into(),
            route_projection: "opening".into(),
            request_key: id.into(),
            selected_tier: "standard".into(),
            selected_effort: None,
            baseline_tier: None,
            baseline_effort: None,
            policy_digest: canonical_digest(&"current-model-only-source")?,
            experiment: None,
            route_measurement: Some(measurement),
        }],
        requested_dimensions: Default::default(),
        evidence: Vec::new(),
        evidence_digest: evidence_digest(&[])?,
        observed_at: CREATED_AT.into(),
    };
    crate::storage::evaluation::validate_subject(&subject)?;
    Ok(subject)
}

fn legacy_body(subject: &EvalSubject) -> Result<String> {
    let mut old = serde_json::to_value(subject)?;
    old["schema_version"] = 1.into();
    old.as_object_mut()
        .context("subject object")?
        .remove("identity");
    old["policy_digest"] = canonical_digest(&"pre-M0-policy")?.into();
    old["preset"] = Value::Null;
    Ok(serde_json::to_string(&old)?)
}

async fn insert_current(
    store: &Store,
    shared: &SharedEvaluation,
    subject: &EvalSubject,
) -> Result<()> {
    let tx = store.begin().await?;
    shared
        .insert_request_subject_in(&tx, OWNER, subject)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn write_raw(store: &Store, source: &EvalSubject, body: &str) -> Result<()> {
    let value: Value = serde_json::from_str(body)?;
    // Keep current rows' indexed digests exact even when semantic validation
    // fails. The old raw fixture has no current typed decoder or writer.
    let (identity, content) = match serde_json::from_str::<EvalSubject>(body) {
        Ok(subject) => (
            crate::storage::evaluation::identity_digest(&subject.identity)?,
            canonical_digest(&subject)?,
        ),
        Err(_) => (
            value["policy_digest"]
                .as_str()
                .context("old policy digest")?
                .to_owned(),
            canonical_digest(&value)?,
        ),
    };
    store.database().execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "INSERT INTO gateway_shared_eval_subjects (namespace,eval_id,owner_user_id,subject_id,scope,policy_digest,holdout,content_digest,subject_json,created_at) VALUES ($1,$2,$3,$4,'request',$5,$6,$7,$8,$9) ON CONFLICT (namespace,eval_id) DO UPDATE SET owner_user_id=EXCLUDED.owner_user_id,subject_id=EXCLUDED.subject_id,scope=EXCLUDED.scope,policy_digest=EXCLUDED.policy_digest,holdout=EXCLUDED.holdout,content_digest=EXCLUDED.content_digest,subject_json=EXCLUDED.subject_json,created_at=EXCLUDED.created_at",
        [store.namespace().into(), source.eval_id.clone().into(), OWNER.into(), source.subject_id.clone().into(), identity.into(), source.holdout.into(), content.into(), body.into(), CREATED_AT.into()]
    )).await?;
    Ok(())
}

async fn delete_subject(store: &Store, id: &str) -> Result<()> {
    ensure!(
        store
            .database()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "DELETE FROM gateway_shared_eval_subjects WHERE namespace=$1 AND eval_id=$2",
                [store.namespace().into(), id.into()]
            ))
            .await?
            .rows_affected()
            == 1
    );
    Ok(())
}

async fn snapshot(store: &Store) -> Result<String> {
    store.database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT jsonb_build_object(
            'records',(SELECT COALESCE(jsonb_agg(to_jsonb(r) ORDER BY id),'[]'::jsonb) FROM gateway_shared_records r WHERE namespace=$1),
            'subjects',(SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY eval_id),'[]'::jsonb) FROM gateway_shared_eval_subjects s WHERE namespace=$1),
            'migrations',(SELECT jsonb_agg(to_jsonb(m) ORDER BY version) FROM gateway_shared_migrations m)
        )::text AS snapshot",
        [store.namespace().into()]
    )).await?.context("missing preflight snapshot")?.try_get("", "snapshot").map_err(Into::into)
}

async fn rejected_without_writes(store: &Store, key: [u8; 32]) -> Result<()> {
    let before = snapshot(store).await?;
    let error = Engine::join(authority(store, key)?, 30_000)
        .await
        .err()
        .context("incompatible standalone Eval subject registered an incarnation")?;
    ensure!(
        error
            .chain()
            .any(|cause| cause.to_string() == "validating retained shared Eval subjects"),
        "wrong preflight rejection: {error:#}"
    );
    ensure!(
        snapshot(store).await? == before,
        "pre-join rejection changed retained rows or authority"
    );
    Ok(())
}
