//! SQLite durability, restart, format-isolation and bounded-load qualification.
use super::*;
use crate::tenancy::{origin::StartupOwner, ownership, transaction::Proof};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    execution_v2::AttemptV2,
    ordered::GenerationProtocol,
    usage_evidence::AttemptEvidence,
};
use sea_orm::{Statement, TransactionTrait};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[path = "ordered_tests/shared_migration.rs"]
mod shared_migration;

fn acquisition(
    path: &std::path::Path,
) -> Result<(ownership::Lease, Arc<Proof>, Arc<StartupOwner>)> {
    let lease = ownership::acquire_path(path, true)?;
    let proof = Proof::new(Arc::new(AtomicBool::new(true)));
    let startup = StartupOwner::new(
        lease.claim()?,
        uuid::Uuid::new_v4().to_string().try_into()?,
        proof.clone(),
    );
    Ok((lease, proof, startup))
}
fn identity(installation: &InstallationId, id: &str, count: usize) -> Result<Identity> {
    Ok(Identity {
        installation: installation.clone(),
        tenant: "tenant".try_into()?,
        principal: "principal".try_into()?,
        execution: id.try_into()?,
        correlation: "correlation".try_into()?,
        revision: "revision".try_into()?,
        ordered: Some((
            AcceptedSelectionV2 {
                generation_defaults_digest: None,
                admission_mode: bitrouter_ai::tenancy::admission::AdmissionMode::Legacy,
                route: ResourceRef {
                    owner_kind: OwnerKind::Tenant,
                    owner_id: "tenant".try_into()?,
                    resource_kind: ResourceKind::Route,
                    resource_id: "route".try_into()?,
                    revision_id: "route-version".try_into()?,
                },
                inbound_protocol: GenerationProtocol::Messages,
                candidate_indices: (0..count)
                    .rev()
                    .map(|i| u16::try_from(i * 3))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            },
            64,
        )),
    })
}
async fn open(path: &std::path::Path, lease: &ownership::Lease) -> Result<DatabaseConnection> {
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}?mode=rwc", path.display()), lease).await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}
fn terminal(intent: &Intent) -> Result<Terminal> {
    let Terminal::V2(base) = intent.unknown()? else {
        anyhow::bail!("wrong fixture format");
    };
    let mut r = base.receipt.clone();
    r.outcome = Outcome::Failed;
    r.state = ReceiptState::Incomplete;
    r.attempts = intent
        .selection
        .candidate_indices
        .iter()
        .enumerate()
        .map(|(i, &ordinal)| {
            Ok(AttemptV2 {
                attempt_id: format!("attempt-{i}").try_into()?,
                candidate_index: ordinal,
                target: None,
                outcome: Outcome::Failed,
                observed_http_status: Some(503),
                usage: r.usage.clone(),
                native_usage: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut e = base.evidence.document.clone();
    e.capture = Capture::Captured;
    e.attempts = r
        .attempts
        .iter()
        .map(|a| AttemptEvidence {
            attempt_id: a.attempt_id.clone(),
            target: a.target.clone(),
            outcome: a.outcome,
            terminal_observed: true,
        })
        .collect();
    Ok(Terminal::V2(Arc::new(CandidateV2::new(
        r,
        Disposition {
            dispatch: DispatchState::Started,
            statuses: vec![Some(503); intent.selection.candidate_indices.len()],
            auxiliary_digest: None,
        },
        e,
    )?)))
}
async fn persist_intent(db: &DatabaseConnection, intent: &Intent) -> Result<()> {
    let tx = db.begin().await?;
    insert(&tx, intent).await?;
    tx.commit().await?;
    Ok(())
}

#[test]
fn profiled_final_intent_serializer_is_bounded_and_mode_is_identity() -> Result<()> {
    use bitrouter_ai::tenancy::admission::AdmissionMode;
    use serde_json::json;
    for (count, expected) in [(1, 1826), (8, 1854), (9, 1860), (64, 2082)] {
        let selected = AcceptedSelectionV2 {
            generation_defaults_digest: None,
            admission_mode: AdmissionMode::ProfiledV1,
            route: ResourceRef {
                owner_kind: OwnerKind::OperatorPool,
                owner_id: "o".repeat(128).try_into()?,
                resource_kind: ResourceKind::Route,
                resource_id: "r".repeat(128).try_into()?,
                revision_id: "v".repeat(128).try_into()?,
            },
            inbound_protocol: GenerationProtocol::ChatCompletions,
            candidate_indices: (0..count)
                .map(|index| u16::try_from(255 - index))
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        let budget = EvidenceBudgetV2::for_attempts(count)?;
        let intent: Intent = serde_json::from_value(
            json!({"schema_version":2,"installation":"i".repeat(128),
            "tenant":"t".repeat(128),"principal":"p".repeat(128),"execution":"e".repeat(128),
            "correlation":"c".repeat(128),"revision":"r".repeat(128),"selection":selected,"attempt_limit":64,
            "evidence_bytes":budget.max_bytes,"frame_bytes":budget.max_frame_bytes,"reserved_bytes":budget.reserved_bytes,
            "frames":budget.max_frames,"origin":{"acquisition":"00000000-0000-0000-0000-000000000000","instance":"n".repeat(128)},
            "admitted_at":"2026-11-02T00:00:00Z"}),
        )?;
        intent.validate()?;
        ensure!(serde_json::to_vec(&intent)?.len() == expected && expected < MAX_BYTES);
        let mut extended = intent.clone();
        extended.selection.generation_defaults_digest = Some("a".repeat(64).try_into()?);
        extended.validate()?;
        ensure!(serde_json::to_vec(&extended)?.len() == expected + 96 && expected + 96 < MAX_BYTES);
        let Terminal::V2(unknown) = extended.unknown()? else {
            anyhow::bail!("format")
        };
        ensure!(extended.matches(&unknown.receipt) && !intent.matches(&unknown.receipt));
        unknown
            .evidence
            .document
            .validate_against(&unknown.receipt)?;
        let Terminal::V2(terminal) = intent.unknown()? else {
            anyhow::bail!("format");
        };
        ensure!(intent.matches(&terminal.receipt));
        let mut changed = terminal.receipt.clone();
        changed.accepted_selection.admission_mode = AdmissionMode::Legacy;
        ensure!(!intent.matches(&changed));
        let mut oversized = serde_json::to_value(intent)?;
        oversized["admitted_at"] = json!(format!("2026-11-02T00:00:00.{}Z", "0".repeat(MAX_BYTES)));
        ensure!(
            serde_json::from_value::<Intent>(oversized)?
                .validate()
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn ordered_1_8_9_64_attempts_preserve_ordinals_and_explicit_format_reads() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receipts.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    for count in [1, 8, 9, 64] {
        let identity = identity(&installation, &format!("execution-{count}"), count)?;
        let intent = Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?;
        persist_intent(&db, &intent).await?;
        let candidate = terminal(&intent)?;
        let tx = db.begin().await?;
        ensure!(store::insert_terminal(&tx, &candidate).await?);
        tx.commit().await?;
        ensure!(
            load(&db, &installation, &identity.tenant, &identity.execution)
                .await?
                .is_none()
        );
        let saved =
            store::lookup_terminal(&db, &installation, &identity.tenant, &identity.execution)
                .await?
                .context("terminal")?;
        ensure!(saved.json() == candidate.json() && saved.digest() == candidate.digest());
        let Terminal::V2(saved) = saved else {
            anyhow::bail!("v2 format lost")
        };
        ensure!(
            saved.receipt.attempts.len() == count
                && saved
                    .receipt
                    .attempts
                    .iter()
                    .map(|a| a.candidate_index)
                    .collect::<Vec<_>>()
                    == intent.selection.candidate_indices
        );
        let tx = db.begin().await?;
        ensure!(!store::insert_terminal(&tx, &candidate).await?);
        tx.commit().await?;
        use crate::tenancy::receipts::read::{self, Request};
        let failure = || {
            crate::tenancy::error::Failure::from(
                bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable,
            )
        };
        let v2 = read::body(
            &db,
            &installation,
            &identity.tenant,
            &Request::OneV2(identity.execution.clone()),
            failure,
        )
        .await
        .map_err(|error| anyhow::anyhow!("v2 read failed: {:?}", error.0.code))?;
        ensure!(v2 == candidate.json().as_bytes());
        for request in [
            Request::One(identity.execution.clone()),
            Request::UsageEvidence(identity.execution.clone()),
        ] {
            let error = read::body(&db, &installation, &identity.tenant, &request, failure)
                .await
                .err()
                .context("v1 mismatch")?;
            ensure!(
                error.0.code == bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch
            );
        }
    }
    ensure!(
        Intent::new(
            &identity(&installation, "too-many", 65)?,
            startup.bootstrap_stamp(&proof)?
        )
        .is_err()
    );
    let inventory = store::inventory(&db, &installation).await?;
    ensure!(inventory.rows == 4 && inventory.unknown_count == 0);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn ordered_terminal_sidecar_and_intent_retirement_rollback_atomically() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receipts.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    let identity = identity(&installation, "execution", 64)?;
    let intent = Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?;
    let tx = db.begin().await?;
    insert(&tx, &intent).await?;
    tx.rollback().await?;
    ensure!(
        load(&db, &installation, &identity.tenant, &identity.execution)
            .await?
            .is_none()
    );
    persist_intent(&db, &intent).await?;
    let before = inventory(&db, &installation).await?;
    ensure!(
        before.rows == 1 && before.unknown.len() == 1 && before.bytes + 16 * 1024 == MAX_OBLIGATION
    );
    let candidate = terminal(&intent)?;
    for (name, sql) in [
        (
            "reject_evidence",
            "CREATE TRIGGER reject_evidence BEFORE INSERT ON tenancy_usage_evidence BEGIN SELECT RAISE(ABORT,'evidence failure'); END",
        ),
        (
            "reject_retirement",
            "CREATE TRIGGER reject_retirement BEFORE DELETE ON tenancy_dispatch_intents BEGIN SELECT RAISE(ABORT,'intent retirement failure'); END",
        ),
    ] {
        db.execute_unprepared(sql).await?;
        let tx = db.begin().await?;
        ensure!(store::insert_terminal(&tx, &candidate).await.is_err());
        tx.rollback().await?;
        db.execute_unprepared(&format!("DROP TRIGGER {name}"))
            .await?;
        ensure!(
            load(&db, &installation, &identity.tenant, &identity.execution)
                .await?
                .is_some()
        );
        ensure!(
            store::lookup_terminal(&db, &installation, &identity.tenant, &identity.execution)
                .await?
                .is_none()
        );
    }
    let tx = db.begin().await?;
    store::insert_terminal(&tx, &candidate).await?;
    tx.rollback().await?;
    ensure!(
        load(&db, &installation, &identity.tenant, &identity.execution)
            .await?
            .is_some()
    );
    ensure!(
        store::lookup_terminal(&db, &installation, &identity.tenant, &identity.execution)
            .await?
            .is_none()
    );
    let tx = db.begin().await?;
    store::insert_terminal(&tx, &candidate).await?;
    tx.commit().await?;
    ensure!(
        load(&db, &installation, &identity.tenant, &identity.execution)
            .await?
            .is_none()
    );
    let after = store::inventory(&db, &installation).await?;
    ensure!(
        after.rows == 1
            && after.unknown_count == 0
            && after.bytes == candidate.bytes() + candidate.auxiliary_bytes()
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn ordered_restart_converts_outstanding_intent_to_unknown_without_replay() -> Result<()> {
    restart_case(None).await
}

#[tokio::test]
async fn envelope_digest_survives_outstanding_intent_recovery_without_payload_or_replay()
-> Result<()> {
    let values: bitrouter_ai::tenancy::generation_defaults::GenerationDefaultsEnvelopeV1 =
        serde_json::from_value(
            serde_json::json!({"schema_version":1,"supplemental":{"vendor_knob":"request-only-secret"}}),
        )?;
    restart_case(Some(values.digest()?)).await
}

#[tokio::test]
async fn envelope_v2_choice_digest_recovers_unknown_without_executable_payload_or_replay()
-> Result<()> {
    for mode in ["apply", "preserve_absence"] {
        let values: bitrouter_ai::tenancy::generation_defaults::GenerationDefaultsEnvelopeV2 =
            serde_json::from_value(
                serde_json::json!({"schema_version":2,"supplemental":{"vendor_knob":"request-only-secret"},"configuration_defaults":mode}),
            )?;
        restart_case(Some(values.digest()?)).await?;
    }
    Ok(())
}

async fn restart_case(digest: Option<bitrouter_ai::tenancy::identity::Digest>) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receipts.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    let mut identity = identity(&installation, "execution", 64)?;
    identity
        .ordered
        .as_mut()
        .context("ordered identity")?
        .0
        .generation_defaults_digest = digest.clone();
    let intent = Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?;
    persist_intent(&db, &intent).await?;
    let control = Control::new(proof.clone());
    ensure!(
        recover(&db, &installation, Some(&startup), &proof, &control)
            .await
            .is_err()
    );
    ensure!(
        load(&db, &installation, &identity.tenant, &identity.execution)
            .await?
            .is_some()
    );
    db.close().await?;
    drop((control, startup, proof, lease));
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    recover(
        &db,
        &installation,
        Some(&startup),
        &proof,
        &Control::new(proof.clone()),
    )
    .await?;
    let saved = store::lookup_terminal(&db, &installation, &identity.tenant, &identity.execution)
        .await?
        .context("recovered receipt")?;
    ensure!(saved.unknown());
    ensure!(!saved.json().contains("request-only-secret"));
    ensure!(!saved.json().contains("configuration_defaults"));
    let bytes = saved.json().to_owned();
    let Terminal::V2(candidate) = saved else {
        anyhow::bail!("recovery format lost")
    };
    ensure!(
        candidate.receipt.attempts.is_empty()
            && candidate.receipt.accepted_selection == intent.selection
            && candidate.receipt.usage.input_tokens.is_none()
    );
    ensure!(
        candidate.evidence.document.capture == Capture::Unavailable
            && candidate.evidence.document.frames.is_empty()
    );
    if let Some(digest) = digest {
        ensure!(
            candidate
                .receipt
                .accepted_selection
                .generation_defaults_digest
                == Some(digest.clone())
        );
        ensure!(
            candidate
                .evidence
                .document
                .accepted_selection
                .generation_defaults_digest
                == Some(digest.clone())
        );
    }
    let inventory = store::inventory(&db, &installation).await?;
    ensure!(inventory.rows == 1 && inventory.unknown_count == 1);
    db.close().await?;
    drop((startup, proof, lease));
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    recover(
        &db,
        &installation,
        Some(&startup),
        &proof,
        &Control::new(proof.clone()),
    )
    .await?;
    ensure!(
        store::lookup_terminal(&db, &installation, &identity.tenant, &identity.execution)
            .await?
            .context("second restart")?
            .json()
            == bytes
    );
    db.close().await?;
    Ok(())
}

struct Probe<'a> {
    db: &'a DatabaseConnection,
    bodies: AtomicUsize,
}
#[async_trait::async_trait]
impl ConnectionTrait for Probe<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        self.db.get_database_backend()
    }
    async fn execute(
        &self,
        s: Statement,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.db.execute(s).await
    }
    async fn execute_unprepared(
        &self,
        s: &str,
    ) -> std::result::Result<sea_orm::ExecResult, sea_orm::DbErr> {
        self.db.execute_unprepared(s).await
    }
    async fn query_one(
        &self,
        s: Statement,
    ) -> std::result::Result<Option<sea_orm::QueryResult>, sea_orm::DbErr> {
        if s.sql.contains("THEN intent_json") || s.sql.contains("THEN evidence_json") {
            self.bodies.fetch_add(1, Ordering::SeqCst);
        }
        self.db.query_one(s).await
    }
    async fn query_all(
        &self,
        s: Statement,
    ) -> std::result::Result<Vec<sea_orm::QueryResult>, sea_orm::DbErr> {
        if s.sql.contains("THEN intent_json") || s.sql.contains("THEN evidence_json") {
            self.bodies.fetch_add(1, Ordering::SeqCst);
        }
        self.db.query_all(s).await
    }
}
#[tokio::test]
async fn ordered_oversized_or_understated_intents_fail_before_body_transfer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receipts.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    let identity = identity(&installation, "execution", 1)?;
    let intent = Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?;
    persist_intent(&db, &intent).await?;
    db.execute_unprepared(
        "UPDATE tenancy_dispatch_intents SET intent_json=printf('%.*c',16385,'x'),byte_count=1",
    )
    .await?;
    let probe = Probe {
        db: &db,
        bodies: AtomicUsize::new(0),
    };
    ensure!(
        load(&probe, &installation, &identity.tenant, &identity.execution)
            .await
            .is_err()
    );
    ensure!(probe.bodies.load(Ordering::SeqCst) == 0);
    ensure!(inventory(&db, &installation).await.is_err());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn ordered_evidence_proportional_limit_preflights_before_json_transfer() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receipts.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    let identity = identity(&installation, "execution", 1)?;
    let intent = Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?;
    persist_intent(&db, &intent).await?;
    let candidate = terminal(&intent)?;
    let tx = db.begin().await?;
    store::insert_terminal(&tx, &candidate).await?;
    tx.commit().await?;
    db.execute_unprepared("UPDATE tenancy_usage_evidence SET evidence_json=printf('%.*c',262145,'x'),byte_count=262145").await?;
    let probe = Probe {
        db: &db,
        bodies: AtomicUsize::new(0),
    };
    ensure!(
        store::lookup_terminal(&probe, &installation, &identity.tenant, &identity.execution)
            .await
            .is_err()
    );
    ensure!(probe.bodies.load(Ordering::SeqCst) == 0);
    ensure!(store::inventory(&db, &installation).await.is_err());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn ordered_writer_queue_overload_before_admission_does_not_fence_receipts() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = crate::tenancy::Service::open(db.clone(), None).await?;
    service.dispatch_test_ready.store(true, Ordering::Release);
    let state = service
        .current()
        .map_err(|e| anyhow::anyhow!("state: {:?}", e.0.code))?;
    ensure!(service.dispatch_ready(&state));
    ensure!(!service.ordered_dispatch_ready(&state));
    let mut slots = Vec::new();
    for _ in 0..16 {
        slots.push(
            service
                .reserve_ordered_intent()
                .map_err(|e| anyhow::anyhow!("slot: {:?}", e.0.code))?,
        );
    }
    let error = service
        .reserve_ordered_intent()
        .err()
        .context("full queue")?;
    ensure!(error.0.code == bitrouter_ai::tenancy::operation::ErrorCode::CapacityExceeded);
    ensure!(service.admission.receipts_ready());
    drop(slots);
    let slot = service
        .reserve_ordered_intent()
        .map_err(|e| anyhow::anyhow!("slot: {:?}", e.0.code))?;
    drop(slot);
    ensure!(service.admission.receipts_ready());
    service.shutdown().await?;
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn ordered_terminal_cannot_change_intent_ingress_protocol() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receipts.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    let identity = identity(&installation, "execution", 9)?;
    let intent = Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?;
    persist_intent(&db, &intent).await?;
    let Terminal::V2(candidate) = terminal(&intent)? else {
        anyhow::bail!("fixture format")
    };
    let mut receipt = candidate.receipt.clone();
    receipt.accepted_selection.inbound_protocol = GenerationProtocol::Responses;
    let mut evidence = candidate.evidence.document.clone();
    evidence.accepted_selection = receipt.accepted_selection.clone();
    let replacement = Terminal::V2(Arc::new(CandidateV2::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Started,
            statuses: vec![Some(503); 9],
            auxiliary_digest: None,
        },
        evidence,
    )?));
    let tx = db.begin().await?;
    ensure!(store::insert_terminal(&tx, &replacement).await.is_err());
    tx.rollback().await?;
    ensure!(
        load(&db, &installation, &identity.tenant, &identity.execution)
            .await?
            .is_some()
    );
    ensure!(
        store::lookup_terminal(&db, &installation, &identity.tenant, &identity.execution)
            .await?
            .is_none()
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn ordered_authority_bootstrap_converts_intents_before_service_publication() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("receipts.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
        .await?
        .installation;
    let identity = identity(&installation, "execution", 64)?;
    persist_intent(
        &db,
        &Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?,
    )
    .await?;
    db.close().await?;
    drop((startup, proof, lease));
    let owned =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}?mode=rw", path.display())).await?;
    crate::db::run_migrations(owned.database()).await?;
    let service = crate::tenancy::Service::open_owned_profile(
        owned,
        &Default::default(),
        &Default::default(),
    )
    .await?;
    ensure!(service.current().is_ok());
    ensure!(inventory(&service.db, &installation).await?.rows == 0);
    let receipt = store::lookup_terminal(
        &service.db,
        &installation,
        &identity.tenant,
        &identity.execution,
    )
    .await?
    .context("bootstrap terminal receipt")?;
    ensure!(receipt.unknown());
    let inventory = store::inventory(&service.db, &installation).await?;
    ensure!(
        inventory.rows == 1 && inventory.unknown_count == 1 && service.admission.receipts_ready()
    );
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn provider_facts_v1_object_reads_exact_v2_attempts_1_8_9_64_without_receipt_changes()
-> Result<()> {
    use crate::tenancy::receipts::{
        provider_technical::{Capture, storage as technical},
        read::{self, Request},
    };
    let failure = || {
        crate::tenancy::error::Failure::from(
            bitrouter_ai::tenancy::operation::ErrorCode::StorageUnavailable,
        )
    };
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("technical-v2.sqlite");
    let (lease, proof, startup) = acquisition(&path)?;
    let db = open(&path, &lease).await?;
    let checked: Result<()> = async {
        let installation = crate::tenancy::store::load(&db, chrono::Utc::now())
            .await?
            .installation;
        for count in [1, 8, 9, 64] {
            let identity = identity(&installation, &format!("technical-{count}"), count)?;
            let intent = Intent::new(&identity, startup.bootstrap_stamp(&proof)?)?;
            persist_intent(&db, &intent).await?;
            let original = terminal(&intent)?;
            let tx = db.begin().await?;
            ensure!(store::insert_terminal(&tx, &original).await?);
            tx.commit().await?;
            let before = (
                original.json().to_owned(),
                original.private_json().to_owned(),
                original.digest().to_owned(),
            );
            let capture = Arc::new(Capture::default());
            let operation = capture.begin(std::time::Instant::now(), false);
            let Terminal::V2(value) = &original else {
                anyhow::bail!("V2 fixture")
            };
            for attempt in &value.receipt.attempts {
                operation.armed(&attempt.attempt_id);
            }
            operation.finish(false);
            operation.selected();
            capture.freeze();
            let facts = capture.take(&original).context("V2 technical snapshot")?;
            ensure!(facts.receipt_format == 2 && facts.operations[0].attempt_ids.len() == count);
            facts.validate_against_v2(&value.receipt)?;
            ensure!(technical::insert(&db, &original, &technical::Prepared::new(&facts)?).await?);
            let bytes = read::body(
                &db,
                &installation,
                &identity.tenant,
                &Request::ProviderTechnical(identity.execution.clone()),
                failure,
            )
            .await
            .map_err(|error| anyhow::anyhow!("technical read failed: {:?}", error.0.code))?;
            let saved: Option<
                bitrouter_ai::tenancy::provider_technical_facts::ProviderTechnicalFactsV1,
            > = serde_json::from_slice(&bytes)?;
            ensure!(saved == Some(facts));
            let loaded =
                store::lookup_terminal(&db, &installation, &identity.tenant, &identity.execution)
                    .await?
                    .context("unchanged V2 receipt")?;
            ensure!(
                loaded.json() == before.0
                    && loaded.private_json() == before.1
                    && loaded.digest() == before.2
            );
            ensure!(
                read::body(
                    &db,
                    &installation,
                    &identity.tenant,
                    &Request::One(identity.execution.clone()),
                    failure
                )
                .await
                .is_err()
            );
        }
        Ok(())
    }
    .await;
    let closed = db.close().await;
    checked?;
    closed?;
    drop((startup, lease));
    Ok(())
}
