//! Real database evidence for mutation receipts, without an operator HTTP server.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::operator::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::json;

use super::{Actor, Failure, Operator};
use crate::gateway::providers::auth::store::CredentialKey;
use crate::gateway::shared::{
    authority::Authority,
    credentials::Credentials,
    crypto::DeploymentKey,
    engine::{Admission, AdmissionResult, Engine, TrajectoryBinding},
    schema,
    store::{self, Kind, Store, Stored},
};

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL test database"]
async fn postgres_operator_transactions() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), fixture())
        .await
        .context("operator fixture deadline exceeded")?
}

async fn fixture() -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
        .context("dedicated PostgreSQL test URL is required")?;
    let first_db = store::connect(&url, 3, Duration::from_secs(3)).await?;
    let second_db = store::connect(&url, 3, Duration::from_secs(3)).await?;
    let namespace = format!("operator-{}", uuid::Uuid::new_v4());
    let first = Store::new(first_db.clone(), &namespace, Duration::from_secs(3))?;
    let second = Store::new(second_db.clone(), &namespace, Duration::from_secs(3))?;
    let result = async {
        crate::db::run_migrations(first.database()).await?;
        schema::initialize(&first).await?;
        let authority = Authority::new(first.clone(), DeploymentKey::from_bytes([37; 32])?);
        authority.initialize().await?;
        let peer_authority = Authority::new(second.clone(), DeploymentKey::from_bytes([37; 32])?);
        let a = Operator::new(authority.clone());
        let b = Operator::new(peer_authority);
        let actor = Actor::new(
            "operator-a".into(),
            [
                Grant::Read,
                Grant::PolicyWrite,
                Grant::CredentialWrite,
                Grant::Publish,
                Grant::RotateCorrelation,
                Grant::ResolveExecution,
                Grant::GlobalPrincipalWrite,
            ]
            .into(),
        )?;
        policy_receipts(&a, &b, &actor).await?;
        invalid_reads(&a, &actor).await?;
        credentials_and_publication(&a, &b, &actor).await?;
        principals(&a, &b, &actor).await?;
        cancellation_and_isolation(&a, &b, &actor).await?;
        atomic_rollback_and_reply_loss(&a, &b, &actor).await?;
        // Administration must not join or publish a serving incarnation.
        ensure!(row_count(&first, "instance").await? == 0);
        resolution(&a, &actor).await?;
        audit_rows(&first).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let closed_first = first_db.close().await;
    let closed_second = second_db.close().await;
    result?;
    closed_first?;
    closed_second?;
    Ok(())
}

fn mutation(id: &str, command: Command) -> Mutation {
    Mutation {
        operation_id: id.into(),
        command,
    }
}

fn tenant(id: &str, expected: Option<u64>) -> Command {
    Command::PutTenant(TenantPut {
        tenant_id: id.into(),
        expected_revision: expected,
        limits: TenantLimits {
            concurrent_executions: 4,
            requests_per_minute: 100,
            resource_pools: ["pool".into()].into(),
        },
    })
}

fn rejected<T>(result: Result<T>, expected: ErrorCode) -> Result<()> {
    let error = result.err().context("unexpected operator success")?;
    ensure!(
        matches!(error.downcast_ref::<Failure>(), Some(Failure(code)) if *code == expected),
        "unexpected operator error: {error:#}"
    );
    Ok(())
}

async fn policy_receipts(a: &Operator, b: &Operator, actor: &Actor) -> Result<()> {
    let create = mutation("tenant-create", tenant("tenant", None));
    let (left, right) = tokio::join!(a.apply(actor, &create), b.apply(actor, &create));
    let original = left?;
    ensure!(original == right?);
    let collision = mutation("tenant-create", tenant("different-tenant", None));
    rejected(a.apply(actor, &collision).await, ErrorCode::Conflict)?;
    rejected(
        a.tenant(actor, "different-tenant").await,
        ErrorCode::NotFound,
    )?;

    // Occupancy revisions are intentionally different from operator policy CAS.
    let tx = a.authority.store().begin().await?;
    a.authority.fence(&tx).await?;
    let mut row: Stored<serde_json::Value> = a
        .authority
        .store()
        .lock(&tx, Kind::Tenant, "tenant")
        .await?;
    row.value["active"] = json!(2);
    row.value["accepted"] = json!(7);
    a.authority
        .store()
        .save(&tx, &mut row, "active", None, Store::now(&tx).await?)
        .await?;
    tx.commit().await?;

    let first = mutation("tenant-update-a", tenant("tenant", Some(1)));
    let second = mutation("tenant-update-b", tenant("tenant", Some(1)));
    let (left, right) = tokio::join!(a.apply(actor, &first), b.apply(actor, &second));
    match (left, right) {
        (Ok(_), error @ Err(_)) | (error @ Err(_), Ok(_)) => rejected(error, ErrorCode::Conflict)?,
        _ => anyhow::bail!("CAS race did not have exactly one winner"),
    }
    ensure!(a.tenant(actor, "tenant").await?.revision == 2);
    let row: Stored<serde_json::Value> = a
        .authority
        .store()
        .get(Kind::Tenant, "tenant")
        .await?
        .context("tenant missing")?;
    ensure!(row.value["active"] == 2 && row.value["accepted"] == 7);
    // Replay returns its original committed projection, not the newer policy.
    ensure!(b.apply(actor, &create).await? == original);
    ensure!(
        b.lookup(actor, "tenant-create").await?
            == OperationLookup::Committed {
                receipt: Box::new(original)
            }
    );
    let revoked = Actor::new(actor.id.clone(), [Grant::Read].into())?;
    rejected(a.apply(&revoked, &create).await, ErrorCode::Forbidden)?;
    rejected(
        a.lookup(&revoked, "tenant-create").await,
        ErrorCode::Forbidden,
    )?;
    let other = Actor::new("operator-b".into(), [Grant::Read].into())?;
    ensure!(matches!(
        a.lookup(&other, "tenant-create").await?,
        OperationLookup::Unresolved { .. }
    ));
    let long_id = "t".repeat(200);
    a.apply(actor, &mutation("long-tenant", tenant(&long_id, None)))
        .await?;
    ensure!(b.tenant(actor, &long_id).await?.tenant_id == long_id);
    a.apply(
        actor,
        &mutation(
            "resource-create",
            Command::PutResource(ResourcePut {
                resource_pool_id: "pool".into(),
                expected_revision: None,
                concurrent_attempts: 4,
            }),
        ),
    )
    .await?;
    let tx = a.authority.store().begin().await?;
    let mut row: Stored<serde_json::Value> = a
        .authority
        .store()
        .lock(&tx, Kind::Resource, "pool")
        .await?;
    row.value["active"] = json!(1);
    a.authority
        .store()
        .save(&tx, &mut row, "active", None, Store::now(&tx).await?)
        .await?;
    tx.commit().await?;
    a.apply(
        actor,
        &mutation(
            "resource-update",
            Command::PutResource(ResourcePut {
                resource_pool_id: "pool".into(),
                expected_revision: Some(1),
                concurrent_attempts: 5,
            }),
        ),
    )
    .await?;
    ensure!(b.resource(actor, "pool").await?.revision == 2);
    let row: Stored<serde_json::Value> = a
        .authority
        .store()
        .get(Kind::Resource, "pool")
        .await?
        .context("resource missing")?;
    ensure!(row.value["active"] == 1);
    Ok(())
}

async fn invalid_reads(a: &Operator, actor: &Actor) -> Result<()> {
    let short_store = Store::new(
        a.authority.store().database().clone(),
        a.authority.store().namespace(),
        Duration::from_millis(100),
    )?;
    let short = Operator::new(Authority::new(
        short_store,
        DeploymentKey::from_bytes([37; 32])?,
    ));
    let tx = a.authority.store().begin().await?;
    a.authority.fence_exclusive(&tx).await?;
    for id in [String::new(), "x".repeat(257), "invalid\nidentity".into()] {
        rejected(
            tokio::time::timeout(Duration::from_millis(20), short.tenant(actor, &id)).await?,
            ErrorCode::InvalidInput,
        )?;
        rejected(
            tokio::time::timeout(Duration::from_millis(20), short.resource(actor, &id)).await?,
            ErrorCode::InvalidInput,
        )?;
    }
    let forbidden = Actor::new("writer".into(), [Grant::PolicyWrite].into())?;
    rejected(short.tenant(&forbidden, "").await, ErrorCode::Forbidden)?;
    rejected(short.resource(actor, "pool").await, ErrorCode::Unavailable)?;
    tx.rollback().await?;
    rejected(a.resource(actor, "missing").await, ErrorCode::NotFound)?;
    Ok(())
}

async fn credentials_and_publication(a: &Operator, b: &Operator, actor: &Actor) -> Result<()> {
    let create = mutation(
        "credential-create",
        Command::ImportCredential(CredentialImport {
            provider: "fixture".into(),
            account: "default".into(),
            resource_pool_id: "pool".into(),
            expected_version: None,
            credential: CredentialInput::ApiKey {
                value: "operator-private-credential-sentinel".into(),
            },
        }),
    );
    let original = a.apply(actor, &create).await?;
    ensure!(b.apply(actor, &create).await? == original);
    let mut update = create.clone();
    update.operation_id = "credential-update".into();
    if let Command::ImportCredential(input) = &mut update.command {
        input.expected_version = Some(1);
        input.credential = CredentialInput::Oauth {
            access_token: "operator-private-oauth-sentinel".into(),
            refresh_token: Some("operator-private-refresh-sentinel".into()),
            expires_at: 0,
        };
    }
    a.apply(actor, &update).await?;
    ensure!(b.credential(actor, "fixture", "default").await?.version == 2);
    ensure!(b.apply(actor, &create).await? == original);
    let publication = mutation(
        "publish",
        Command::Publish(PublicationPut {
            expected_generation: 0,
            configuration: Publication {
                format: 1,
                routing: json!({"providers":{"fixture":{"api_base":"https://example.com/v1", "models":[{"id":"fixture-model"}], "headers":{"x-private":"operator-private-header-sentinel"}}}}),
                policies: vec![],
                policy_lock: None,
                credentials: vec![CredentialBinding {
                    provider: "fixture".into(),
                    account: "default".into(),
                    credential_provider: "fixture".into(),
                    credential_account: "default".into(),
                }],
                require_known_pricing: false,
            },
        }),
    );
    let published = a.apply(actor, &publication).await?;
    let rotate = mutation(
        "rotate",
        Command::RotateCorrelation {
            expected_generation: 1,
        },
    );
    let (left, right) = tokio::join!(a.apply(actor, &rotate), b.apply(actor, &rotate));
    ensure!(left? == right?);
    ensure!(
        b.revision(actor).await?
            == Revision {
                config_generation: 1,
                key_generation: 2
            }
    );
    ensure!(b.apply(actor, &publication).await? == published);
    let wrong = mutation("publish-stale", publication.command.clone());
    rejected(a.apply(actor, &wrong).await, ErrorCode::Conflict)?;
    ensure!(matches!(
        b.lookup(actor, &wrong.operation_id).await?,
        OperationLookup::Unresolved { .. }
    ));
    let head = a.authority.head().await?;
    ensure!(a.authority.keys(&head).await?.correlations()?.len() == 2);
    ensure!(
        a.authority.configuration(&head).await?.routing["providers"]["fixture"]["headers"]["x-private"]
            == "operator-private-header-sentinel"
    );
    let mut next = publication.clone();
    next.operation_id = "publish-next".into();
    if let Command::Publish(input) = &mut next.command {
        input.expected_generation = 1;
    }
    let rotate = mutation(
        "rotate-next",
        Command::RotateCorrelation {
            expected_generation: 2,
        },
    );
    let (published, rotated) = tokio::join!(a.apply(actor, &next), b.apply(actor, &rotate));
    published?;
    rotated?;
    ensure!(
        b.revision(actor).await?
            == Revision {
                config_generation: 2,
                key_generation: 3
            }
    );
    // Configuration CAS and key CAS are independent even under contention.
    let mut invalid = publication;
    invalid.operation_id = "publish-invalid".into();
    if let Command::Publish(input) = &mut invalid.command {
        input.expected_generation = 2;
        input.configuration.policy_lock = Some(json!("operator-private-invalid-sentinel"));
    }
    let error = a
        .apply(actor, &invalid)
        .await
        .err()
        .context("invalid publication accepted")?;
    ensure!(!format!("{error:#}").contains("operator-private-"));
    rejected::<Receipt>(Err(error), ErrorCode::InvalidInput)?;
    ensure!(b.revision(actor).await?.config_generation == 2);
    reject_corrupt_publication(a, b, actor, &next).await?;
    Ok(())
}

async fn reject_corrupt_publication(
    a: &Operator,
    b: &Operator,
    actor: &Actor,
    source: &Mutation,
) -> Result<()> {
    let store = a.authority.store();
    let (stamp, _) = Credentials::new(a.authority.clone())
        .load(&CredentialKey {
            provider: "fixture".into(),
            account: "default".into(),
        })
        .await?;
    let original: Stored<serde_json::Value> = store
        .get(Kind::Credential, &stamp.id)
        .await?
        .context("credential missing")?;
    for case in ["ciphertext", "encoding", "refresh"] {
        let mut value = original.value.clone();
        match case {
            "ciphertext" => value["secret"]["ciphertext"] = json!("broken"),
            "encoding" => {
                value["secret"] = serde_json::to_value(a.authority.seal(
                    &stamp.id,
                    stamp.version,
                    b"{\"unexpected\":true}",
                )?)?
            }
            _ => value["lease"] = json!({"token":"fixture", "expires_at_ms":0, "started":true}),
        }
        let tx = store.begin().await?;
        a.authority.fence(&tx).await?;
        let mut row: Stored<serde_json::Value> =
            store.lock(&tx, Kind::Credential, &stamp.id).await?;
        row.value = value;
        store
            .save(&tx, &mut row, "ready", None, Store::now(&tx).await?)
            .await?;
        tx.commit().await?;
        let mut publication = source.clone();
        publication.operation_id = format!("publish-reject-{case}");
        if let Command::Publish(input) = &mut publication.command {
            input.expected_generation = 2;
        }
        rejected(a.apply(actor, &publication).await, ErrorCode::Unknown)?;
        ensure!(b.revision(actor).await?.config_generation == 2);
        ensure!(
            store
                .get::<serde_json::Value>(Kind::Authority, "configuration/3")
                .await?
                .is_none()
        );
        ensure!(matches!(
            b.lookup(actor, &publication.operation_id).await?,
            OperationLookup::Unresolved { .. }
        ));
        let tx = store.begin().await?;
        let mut row: Stored<serde_json::Value> =
            store.lock(&tx, Kind::Credential, &stamp.id).await?;
        row.value = original.value.clone();
        store
            .save(&tx, &mut row, "ready", None, Store::now(&tx).await?)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

fn principal_input(id: &str, tenant_id: &str) -> PrincipalPut {
    PrincipalPut {
        principal_id: id.into(),
        tenant_id: tenant_id.into(),
        expected_authorization_revision: None,
        key_hash: Some(crate::auth::keys::hash_key(&format!(
            "fixture-{}",
            uuid::Uuid::new_v4()
        ))),
        active: true,
        expires_at_ms: None,
        requests_per_minute: Some(100),
        policy_id: None,
    }
}

async fn principals(a: &Operator, b: &Operator, actor: &Actor) -> Result<()> {
    let id = format!("principal-{}", uuid::Uuid::new_v4());
    let mut input = principal_input(&id, "tenant");
    let create = mutation("principal-create", Command::PutPrincipal(input.clone()));
    let namespace_actor = Actor::new(
        "namespace-admin".into(),
        [Grant::Read, Grant::PolicyWrite].into(),
    )?;
    rejected(
        a.apply(&namespace_actor, &create).await,
        ErrorCode::Forbidden,
    )?;
    rejected(
        a.principal(&namespace_actor, &id).await,
        ErrorCode::Forbidden,
    )?;
    let receipt = a.apply(actor, &create).await?;
    let before = b.principal(actor, &id).await?;
    input.expected_authorization_revision = Some(before.authorization_revision.clone());
    input.key_hash = None;
    input.active = false;
    a.apply(
        actor,
        &mutation("principal-revoke", Command::PutPrincipal(input.clone())),
    )
    .await?;
    let after = b.principal(actor, &id).await?;
    ensure!(!after.active && after.authorization_revision != before.authorization_revision);
    rejected(
        a.apply(
            actor,
            &mutation("principal-stale", Command::PutPrincipal(input)),
        )
        .await,
        ErrorCode::Conflict,
    )?;
    ensure!(b.apply(actor, &create).await? == receipt);
    Ok(())
}

async fn cancellation_and_isolation(a: &Operator, b: &Operator, actor: &Actor) -> Result<()> {
    let tx = a.authority.store().begin().await?;
    a.authority.fence_exclusive(&tx).await?;
    let cancelled = mutation("cancelled", tenant("cancelled-tenant", None));
    ensure!(
        tokio::time::timeout(Duration::from_millis(30), b.apply(actor, &cancelled))
            .await
            .is_err()
    );
    tx.rollback().await?;
    ensure!(matches!(
        a.lookup(actor, "cancelled").await?,
        OperationLookup::Unresolved { .. }
    ));
    rejected(
        a.tenant(actor, "cancelled-tenant").await,
        ErrorCode::NotFound,
    )?;
    // The same operation can be safely retried with identical input once the
    // caller knows its earlier sending future has ended.
    a.apply(actor, &cancelled).await?;
    let other_namespace = format!("operator-{}", uuid::Uuid::new_v4());
    let store = Store::new(
        a.authority.store().database().clone(),
        &other_namespace,
        Duration::from_secs(3),
    )?;
    let authority = Authority::new(store, DeploymentKey::from_bytes([37; 32])?);
    authority.initialize().await?;
    let other = Operator::new(authority);
    ensure!(matches!(
        other.lookup(actor, "cancelled").await?,
        OperationLookup::Unresolved { .. }
    ));
    rejected(
        other.tenant(actor, "cancelled-tenant").await,
        ErrorCode::NotFound,
    )?;
    Ok(())
}

async fn atomic_rollback_and_reply_loss(a: &Operator, b: &Operator, actor: &Actor) -> Result<()> {
    let command = mutation("rollback", tenant("rolled-back-tenant", None));
    let prepared = super::commands::Prepared::new(&a.authority, &actor.id, &command.command)?;
    let fingerprint = a
        .authority
        .operator_fingerprint("mutation", &serde_json::to_vec(&(&actor.id, &command))?)?;
    let tx = a.authority.store().begin().await?;
    a.write(&tx, actor, &command, &prepared, &fingerprint)
        .await?;
    // Both effect and receipt exist inside the write transaction, neither is
    // visible to an independent connection, and rollback removes both.
    ensure!(
        crate::gateway::shared::engine::management::tenant(&a.authority, &tx, "rolled-back-tenant")
            .await?
            .revision
            == 1
    );
    ensure!(matches!(
        b.lookup(actor, "rollback").await?,
        OperationLookup::Unresolved { .. }
    ));
    rejected(
        b.tenant(actor, "rolled-back-tenant").await,
        ErrorCode::NotFound,
    )?;
    tx.rollback().await?;
    ensure!(matches!(
        b.lookup(actor, "rollback").await?,
        OperationLookup::Unresolved { .. }
    ));
    rejected(
        b.tenant(actor, "rolled-back-tenant").await,
        ErrorCode::NotFound,
    )?;
    let tx = a.authority.store().begin().await?;
    let committed = a
        .write(&tx, actor, &command, &prepared, &fingerprint)
        .await?;
    tx.commit().await?;
    // Simulate losing the response after a successful commit. This is receipt
    // recovery evidence, not an injected PostgreSQL lost-COMMIT-ACK test.
    ensure!(
        b.lookup(actor, "rollback").await?
            == OperationLookup::Committed {
                receipt: Box::new(committed.clone())
            }
    );
    ensure!(b.apply(actor, &command).await? == committed);
    Ok(())
}

async fn resolution(a: &Operator, actor: &Actor) -> Result<()> {
    use bitrouter_ai::gateway::execution::{
        ExecutionState, ResolutionOutcome, ResolveAttemptRequest,
    };
    let tenant_id = format!("execution-tenant-{}", uuid::Uuid::new_v4());
    let principal_id = format!("execution-principal-{}", uuid::Uuid::new_v4());
    a.apply(
        actor,
        &mutation("execution-tenant", tenant(&tenant_id, None)),
    )
    .await?;
    let input = principal_input(&principal_id, &tenant_id);
    let hash = input.key_hash.clone().context("fixture key hash missing")?;
    a.apply(
        actor,
        &mutation("execution-principal", Command::PutPrincipal(input)),
    )
    .await?;
    let engine = Engine::join(a.authority.clone(), 30_000).await?;
    let view = match engine
        .admit(&Admission {
            key_hash: &hash,
            correlation_id: "operator-resolution",
            idempotency_key: None,
            fingerprint: &"f".repeat(64),
            operation: bitrouter_ai::types::ModelOperation::Generation,
            revision: a.authority.head().await?.revision(),
        })
        .await?
    {
        AdmissionResult::Accepted(view, _) => view,
        AdmissionResult::Existing(_) => anyhow::bail!("unexpected fixture deduplication"),
    };
    let tx = a.authority.store().begin().await?;
    engine
        .bind_trajectory(
            &tx,
            &view.execution_id,
            &TrajectoryBinding {
                owner_user_id: tenant_id.clone(),
                episode_id: "operator-episode".into(),
                request_id: "operator-request".into(),
                correlation_epoch: "fixture".into(),
            },
        )
        .await?;
    tx.commit().await?;
    let (stamp, _) = Credentials::new(a.authority.clone())
        .load(&CredentialKey {
            provider: "fixture".into(),
            account: "default".into(),
        })
        .await?;
    let attempt = engine
        .prepare_attempt(&hash, &view.execution_id, &stamp)
        .await?;
    engine
        .dispatch(&hash, &view.execution_id, &attempt.attempt_id)
        .await?;
    engine
        .mark_unknown(&view.execution_id, &attempt.attempt_id)
        .await?;
    let unknown = a.execution(actor, &view.execution_id).await?;
    let command = mutation(
        "resolve",
        Command::ResolveExecution {
            execution_id: view.execution_id.clone(),
            attempt_id: attempt.attempt_id,
            resolution: ResolveAttemptRequest {
                expected_revision: unknown.revision,
                outcome: ResolutionOutcome::Terminated,
                evidence_reference: "case-fixture-123".into(),
            },
        },
    );
    let receipt = a.apply(actor, &command).await?;
    ensure!(a.apply(actor, &command).await? == receipt);
    ensure!(matches!(
        a.execution(actor, &view.execution_id).await?.state,
        ExecutionState::Resolved { .. }
    ));
    let event: Stored<serde_json::Value> = a
        .authority
        .store()
        .get(
            Kind::Outbox,
            &format!("gateway-terminal-{}", view.execution_id),
        )
        .await?
        .context("resolution event missing")?;
    ensure!(event.value["event"]["execution"]["state"]["status"] == "resolved");
    let tenant: Stored<serde_json::Value> = a
        .authority
        .store()
        .get(Kind::Tenant, &tenant_id)
        .await?
        .context("tenant missing")?;
    ensure!(tenant.value["active"] == 0);
    let resource: Stored<serde_json::Value> = a
        .authority
        .store()
        .get(Kind::Resource, "pool")
        .await?
        .context("resource missing")?;
    ensure!(resource.value["active"] == 1);
    Ok(())
}

async fn row_count(store: &Store, kind: &str) -> Result<i64> {
    // https://www.postgresql.org/docs/current/functions-aggregate.html
    let row = store.database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT count(*) AS count FROM gateway_shared_records WHERE namespace = $1 AND kind = $2", [store.namespace().into(), kind.into()])).await?.context("count missing")?;
    Ok(row.try_get("", "count")?)
}

async fn audit_rows(store: &Store) -> Result<()> {
    // Bound namespace-selected test records; never inspect another fixture's rows.
    // https://www.postgresql.org/docs/current/sql-select.html
    let rows = store.database().query_all(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT body, status FROM gateway_shared_records WHERE namespace = $1 AND kind = 'operator_receipt' LIMIT 256", [store.namespace().into()])).await?;
    ensure!(!rows.is_empty());
    for row in rows {
        let body: String = row.try_get("", "body")?;
        let status: String = row.try_get("", "status")?;
        ensure!(
            status == "committed"
                && !body.contains("operator-private-")
                && !body.contains("key_hash")
        );
        let record: super::Record = serde_json::from_str(&body)?;
        ensure!(record.receipt.is_some());
    }
    Ok(())
}
