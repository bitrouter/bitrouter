//! The same durable accounting contracts run on SQLite and native PostgreSQL.
use super::tests::candidate;
use super::*;
use crate::db::{migration::capacity, store_format};
use sea_orm::{DatabaseConnection, DbErr, ExecResult, Statement, TransactionTrait};

mod fixture;
#[cfg(unix)]
mod lost_ack;
mod migration;
mod policy;

use fixture::Fixture;

#[derive(Clone, Copy, Debug)]
enum Case {
    Projection,
    Upgrade,
    FailedUpgrade,
    MissingProjection,
    ForeignSources,
    ProjectionOnly,
    Epoch,
    DriftAudit,
    OAuthTechnical,
    #[cfg(unix)]
    LostAck,
}

impl Case {
    fn legacy(self) -> bool {
        matches!(
            self,
            Self::Upgrade | Self::FailedUpgrade | Self::ForeignSources | Self::Epoch
        )
    }

    async fn run(self, fixture: &mut Fixture) -> Result<()> {
        match self {
            Self::Projection => policy::projection(fixture).await,
            Self::Upgrade => migration::upgrade(fixture).await,
            Self::FailedUpgrade => migration::failed_upgrade(fixture).await,
            Self::MissingProjection => policy::missing_projection(fixture).await,
            Self::ForeignSources => migration::foreign_sources(fixture).await,
            Self::ProjectionOnly => policy::projection_only(fixture).await,
            Self::Epoch => migration::epoch(fixture).await,
            Self::DriftAudit => policy::drift_audit(fixture).await,
            Self::OAuthTechnical => policy::oauth_and_technical(fixture).await,
            #[cfg(unix)]
            Self::LostAck => lost_ack::exercise(fixture).await,
        }
    }
}

async fn run(backend: DatabaseBackend, case: Case) -> Result<()> {
    let mut fixture = Fixture::new(backend, case.legacy()).await?;
    let outcome = case
        .run(&mut fixture)
        .await
        .with_context(|| format!("{backend:?} capacity contract {case:?}"));
    fixture.finish(outcome).await
}

macro_rules! contract {
    ($sqlite:ident, $postgres:ident, $case:ident) => {
        #[tokio::test]
        async fn $sqlite() -> Result<()> { run(DatabaseBackend::Sqlite, Case::$case).await }

        #[cfg(feature = "db-postgres")]
        #[tokio::test]
        #[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL for isolated native PostgreSQL with CREATEDB; no fallback"]
        async fn $postgres() -> Result<()> { run(DatabaseBackend::Postgres, Case::$case).await }
    };
}

contract!(
    policy_projection_tracks_first_write_replay_rollback_and_auxiliary_archive,
    postgres_policy_projection_tracks_first_write_replay_rollback_and_auxiliary_archive,
    Projection
);
contract!(
    legacy_upgrade_backfills_once_and_reopen_does_not_decode_history,
    postgres_native8_upgrade_backfills_once_and_reopen_does_not_decode_history,
    Upgrade
);
contract!(
    failed_upgrade_rolls_back_schema_and_marker_and_can_retry,
    postgres_failed_upgrade_rolls_back_schema_and_marker_and_can_retry,
    FailedUpgrade
);
contract!(
    missing_current_projection_never_reinitializes_and_overflow_rolls_back_source,
    postgres_missing_current_projection_never_reinitializes_and_overflow_rolls_back_source,
    MissingProjection
);
contract!(
    legacy_sources_without_identity_and_foreign_installations_cannot_be_zeroed,
    postgres_legacy_sources_without_identity_and_foreign_installations_cannot_be_zeroed,
    ForeignSources
);
contract!(
    small_and_large_histories_load_only_the_projection,
    postgres_small_and_large_histories_load_only_the_projection,
    ProjectionOnly
);
contract!(
    epoch_requirement_is_transactional_and_survives_legacy_upgrade,
    postgres_epoch_requirement_is_transactional_and_survives_native8_upgrade,
    Epoch
);
contract!(
    explicit_audit_detects_positive_underestimate_and_each_policy_projection_mismatch,
    postgres_explicit_audit_detects_positive_underestimate_and_each_policy_projection_mismatch,
    DriftAudit
);
contract!(
    oauth_pending_transitions_and_optional_technical_facts_have_exact_policy_deltas,
    postgres_oauth_pending_transitions_and_optional_technical_facts_have_exact_policy_deltas,
    OAuthTechnical
);
#[cfg(unix)]
contract!(
    committed_terminal_with_lost_ack_survives_reopen_without_replay_charge,
    postgres_committed_terminal_with_lost_ack_survives_fresh_owner_without_replay_charge,
    LostAck
);

fn quantities(v: &Inventory) -> [u64; 6] {
    [
        v.live_bytes,
        v.history_bytes,
        v.history_rows,
        v.oauth_bytes,
        v.technical_bytes,
        v.unknown_count,
    ]
}

async fn matches_audit(db: &impl ConnectionTrait) -> Result<Inventory> {
    let audit = inventory(db, &"installation".try_into()?).await?;
    capacity::verify(db, &audit).await?;
    ensure!(
        quantities(&audit) == quantities(&capacity::load(db).await?),
        "durable policy quantities differ from canonical audit"
    );
    Ok(audit)
}

async fn persist(tx: &sea_orm::DatabaseTransaction, value: &Candidate) -> Result<bool> {
    let staged = stage_terminal(tx, value).await?;
    let inserted = staged.inserted();
    super::super::auxiliary_store::insert(tx, value).await?;
    drop(staged.reconcile(&|_, _| Ok(())).await?);
    Ok(inserted)
}

async fn commit(db: &DatabaseConnection, value: &Candidate) -> Result<bool> {
    let tx = db.begin().await?;
    match persist(&tx, value).await {
        Ok(inserted) => {
            tx.commit().await?;
            Ok(inserted)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

fn unknown(id: &str) -> Result<Candidate> {
    let mut receipt = candidate("installation", "tenant", id, "creator")?.receipt;
    receipt.state = ReceiptState::Unknown;
    receipt.outcome = bitrouter_ai::tenancy::execution::Outcome::Unknown;
    Candidate::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Unknown,
            statuses: vec![],
            auxiliary_digest: None,
        },
    )
}

fn auxiliary_candidate(id: &str) -> Result<Candidate> {
    use crate::tenancy::receipts::auxiliary::{
        Snapshot,
        count::{Envelope, Fact, Outcome},
    };
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    };
    candidate("installation", "tenant", id, "creator")?.with_auxiliary(Snapshot::Counts(Envelope {
        version: 2,
        counts: vec![Fact {
            ordinal: 0,
            work: "c53bbdf9-7771-41db-88af-a5017795a020".into(),
            credential: ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: "tenant".try_into()?,
                resource_kind: ResourceKind::Credential,
                resource_id: "credential".try_into()?,
                revision_id: "r1".try_into()?,
            },
            candidate: "01".repeat(32),
            prompt: "02".repeat(32),
            generation_request: Some("03".repeat(32)),
            count_request: Some("04".repeat(32)),
            http_status: Some(200),
            outcome: Outcome::Counted,
            input_tokens: Some(11),
        }],
    }))
}

async fn archive_value(db: &DatabaseConnection, value: &Candidate) -> Result<()> {
    let prepared = archive::Prepared::new(value, &|_, _| Ok(()))?;
    let tx = db.begin().await?;
    ensure!(archive::archive_one(&tx, &prepared, &|_, _| Ok(())).await?);
    tx.commit().await?;
    Ok(())
}

async fn update(
    db: &impl ConnectionTrait,
    table: &str,
    field: &str,
    value: impl Into<sea_orm::Value>,
) -> Result<()> {
    let query = Query::update()
        .table(Alias::new(table))
        .value(Alias::new(field), value.into())
        .to_owned();
    db.execute(db.get_database_backend().build(&query)).await?;
    Ok(())
}
