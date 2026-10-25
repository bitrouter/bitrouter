use super::*;
use crate::continuation::tests::{static_authority, target};

use sea_orm::{ConnectOptions, Database, TransactionTrait};

pub(super) async fn isolated_database() -> Result<DatabaseConnection> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
        .context("BITROUTER_TEST_DATABASE_URL is required")?;
    let mut options = ConnectOptions::new(url.clone());
    options.max_connections(1).sqlx_logging(false);
    let setup = Database::connect(options).await?;
    let schema = format!("native_clock_{}", uuid::Uuid::new_v4().simple());
    // The identifier is generated locally from a UUID, never from input.
    // https://www.postgresql.org/docs/current/sql-createschema.html
    setup
        .execute(Statement::from_string(
            DatabaseBackend::Postgres,
            format!("CREATE SCHEMA {schema}"),
        ))
        .await?;
    setup.close().await?;
    let mut options = ConnectOptions::new(url);
    options
        .max_connections(4)
        .sqlx_logging(false)
        .set_schema_search_path(schema);
    Ok(Database::connect(options).await?)
}

pub(super) async fn bind(native: &SharedNative, request: &str) -> Result<Bound> {
    native
        .bind(
            "owner",
            request,
            &ContinuationBinding {
                provider_response_id: "fixture-native-response",
                effective_model: "openai:gpt-5",
                effective_effort: None,
                causal_prefix_commitment: None,
                target: &target("fixture-static-key"),
                credential_authority: &static_authority("fixture-static-key"),
            },
            &publication_generation()?,
            PUBLICATION_PROVISIONAL,
            TimeDelta::days(30),
        )
        .await
}

pub(super) fn publication(row: &continuation_entity::Model) -> PendingPublication {
    PendingPublication {
        tenant: None,
        continuation_identity: row.continuation_identity.clone(),
        generation: row.publication_generation.clone(),
        instance_id: row.publication_instance_id.clone(),
        lease_until: row.publication_lease_until.clone(),
        receipt: None,
        phase: PendingPublicationPhase::Prepared,
    }
}

pub(super) async fn read(
    native: &SharedNative,
    identity: &str,
) -> Result<continuation_entity::Model> {
    let tx = native.authority.begin_cleanup(identity).await?;
    let row = native
        .lock_row(&tx, identity)
        .await?
        .context("native row missing")?
        .row;
    tx.commit().await?;
    Ok(row)
}

/// Install authenticated fixture metadata without waiting a thirty-second lease.
/// This deliberately bypasses the production liveness guard, never its AEAD.
pub(super) async fn fixture_lease(
    native: &SharedNative,
    old: &continuation_entity::Model,
    remaining_ms: i64,
) -> Result<continuation_entity::Model> {
    let tx = native
        .authority
        .begin_cleanup(&old.continuation_identity)
        .await?;
    let previous = native
        .lock_row(&tx, &old.continuation_identity)
        .await?
        .context("fixture row missing")?;
    let now = SharedContinuationDb::now(&tx).await?;
    let mut row = previous.row.clone();
    let plaintext = native.decrypt(&row)?;
    row.publication_lease_until = timestamp(now + TimeDelta::milliseconds(remaining_ms));
    native.seal(&mut row, &plaintext)?;
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE gateway_shared_native_continuations
         SET body = $1, lease_until = $2, revision = revision + 1
         WHERE namespace = $3 AND identity = $4 AND body = $5",
        [
            serde_json::to_string(&row)?.into(),
            row.publication_lease_until.clone().into(),
            native.authority.namespace().into(),
            row.continuation_identity.clone().into(),
            previous.body.into(),
        ],
    ))
    .await?;
    tx.commit().await?;
    Ok(row)
}

pub(super) async fn wait_past(native: &SharedNative, deadline: &str) -> Result<()> {
    let deadline = parse_timestamp(deadline)?;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if SharedContinuationDb::now(native.authority.database()).await? >= deadline {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("fixture database clock did not reach expiry")?
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn shared_database_clock_fences_expired_native_ownership() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let before = SharedContinuationDb::now(&database).await?;
        let bound = bind(&native, "expired-native-owner").await?;
        let after = SharedContinuationDb::now(&database).await?;
        ensure!(bound.outcome == BindOutcome::Inserted);
        ensure!(parse_timestamp(&bound.row.created_at)? >= before);
        ensure!(parse_timestamp(&bound.row.created_at)? <= after);
        let expired = fixture_lease(&native, &bound.row, -1).await?;
        let error = native
            .renew(&publication(&expired))
            .await
            .err()
            .context("expired publication lease was renewed")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::Expired)
        ));
        ensure!(read(&native, &expired.continuation_identity).await? == expired);
        // Shared native operation never initializes the local global key epoch.
        let tables = database
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT to_regclass('provider_continuation_key_epoch')::text AS local_epoch",
            ))
            .await?
            .context("table lookup missing")?;
        ensure!(
            tables
                .try_get::<Option<String>>("", "local_epoch")?
                .is_none()
        );
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn unchanged_row_lock_wait_cannot_renew_after_expiry() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let bound = bind(&native, "locked-native-owner").await?;
        let short = fixture_lease(&native, &bound.row, 300).await?;
        let blocker = database.begin().await?;
        native
            .lock_row(&blocker, &short.continuation_identity)
            .await?;
        let token = publication(&short);
        let mut renewal = Box::pin(native.renew(&token));
        ensure!(
            tokio::time::timeout(Duration::from_millis(80), renewal.as_mut())
                .await
                .is_err()
        );
        wait_past(&native, &short.publication_lease_until).await?;
        blocker.commit().await?;
        let error = renewal
            .await
            .err()
            .context("lock waiter renewed an expired lease")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::Expired)
        ));
        ensure!(read(&native, &short.continuation_identity).await? == short);
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn final_sql_guard_covers_pause_after_database_clock_sample() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let bound = bind(&native, "paused-native-owner").await?;
        let short = fixture_lease(&native, &bound.row, 200).await?;
        let tx = native
            .authority
            .begin_bind(&short.continuation_identity)
            .await?;
        let snapshot = native
            .lock_row(&tx, &short.continuation_identity)
            .await?
            .context("paused owner row missing")?;
        let now = SharedContinuationDb::now(&tx).await?;
        ensure!(parse_timestamp(&short.publication_lease_until)? > now);
        let mut candidate = snapshot.row.clone();
        candidate.publication_lease_until = publication_lease_until(now)?;
        native.seal(&mut candidate, &native.decrypt(&snapshot.row)?)?;
        wait_past(&native, &short.publication_lease_until).await?;
        let error = native
            .save_live(&tx, &snapshot, &candidate)
            .await
            .err()
            .context("paused expired owner changed durable lease")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::Expired)
        ));
        tx.rollback().await?;
        ensure!(read(&native, &short.continuation_identity).await? == short);
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn renewal_reseals_metadata_and_preserves_retention() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let bound = bind(&native, "live-native-owner").await?;
        let short = fixture_lease(&native, &bound.row, 5_000).await?;
        let token = publication(&short);
        let renewed = native.renew(&token).await?;
        ensure!(renewed.publication_lease_until > short.publication_lease_until);
        ensure!(renewed.expires_at == short.expires_at && renewed.purge_after == short.purge_after);
        ensure!(native.decrypt(&renewed)? == native.decrypt(&short)?);
        ensure!(
            native.renew(&token).await.is_err(),
            "superseded token retained authority"
        );

        let mut recovery = publication(&renewed);
        recovery.phase = PendingPublicationPhase::Reconcile;
        ensure!(
            native.renew(&recovery).await.is_err(),
            "cleanup token became forward authority"
        );
        let other = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        ensure!(native.identity("owner", "same")? != other.identity("owner", "same")?);
        // Even identical encryption material must bind the namespace identity.
        let same_key_other_namespace = SharedNative {
            key: native.key.clone(),
            authority: other.authority,
            maintenance_cursor: Mutex::new(None),
            recovery_cursor: Mutex::new(None),
        };
        ensure!(same_key_other_namespace.decrypt(&renewed).is_err());
        ensure!(
            decrypt_row(&native.key, &renewed).is_err(),
            "shared ciphertext entered the local domain"
        );
        ensure!(
            native.identity("owner", "same")?
                != same_key_other_namespace.identity("owner", "same")?
        );
        ensure!(native.identity("a\0b", "c")? != native.identity("a", "b\0c")?);
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cached_future_lease_and_tampered_metadata_do_not_grant_authority() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let bound = bind(&native, "cached-owner").await?;
        let changed = fixture_lease(&native, &bound.row, 5_000).await?;
        let error = native
            .renew(&publication(&bound.row))
            .await
            .err()
            .context("cached future lease bypassed durable fencing")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::Changed)
        ));

        for field in ["lease", "generation", "instance", "state", "retention"] {
            let mut forged = changed.clone();
            match field {
                "lease" => {
                    forged.publication_lease_until = timestamp(
                        parse_timestamp(&changed.publication_lease_until)? + TimeDelta::seconds(1),
                    )
                }
                "generation" => forged.publication_generation = publication_generation()?,
                "instance" => forged.publication_instance_id = publication_instance_id()?,
                "state" => forged.publication_state = PUBLICATION_ACTIVE.into(),
                "retention" => {
                    forged.expires_at =
                        timestamp(parse_timestamp(&changed.expires_at)? + TimeDelta::days(1))
                }
                _ => anyhow::bail!("unsupported tamper fixture"),
            }
            // Tamper both JSON and scan metadata, leaving ciphertext unchanged.
            // https://www.postgresql.org/docs/current/sql-update.html
            database
                .execute(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "UPDATE gateway_shared_native_continuations
                 SET body = $1, publication_state = $2, lease_until = $3, expires_at = $4,
                     revision = revision + 1
                 WHERE namespace = $5 AND identity = $6",
                    [
                        serde_json::to_string(&forged)?.into(),
                        forged.publication_state.clone().into(),
                        forged.publication_lease_until.clone().into(),
                        forged.expires_at.clone().into(),
                        native.authority.namespace().into(),
                        forged.continuation_identity.clone().into(),
                    ],
                ))
                .await?;
            ensure!(
                native.decrypt(&forged).is_err(),
                "{field} tampering was authenticated"
            );
            ensure!(native.renew(&publication(&forged)).await.is_err());
            ensure!(read(&native, &forged.continuation_identity).await? == forged);
        }
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn target_field_boundaries_reject_a_different_duplicate_binding() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        let credential = static_authority("fixture-static-key");
        let mut first = target("fixture-static-key");
        first.provider_name = "a\0b".into();
        first.account_label = Some("c".into());
        let mut second = first.clone();
        second.provider_name = "a".into();
        second.account_label = Some("b\0c".into());
        ensure!(
            native.target_fingerprint(&first, &credential)?
                != native.target_fingerprint(&second, &credential)?
        );
        let first_binding = ContinuationBinding {
            provider_response_id: "same-native-response",
            effective_model: "same-effective-model",
            effective_effort: None,
            causal_prefix_commitment: None,
            target: &first,
            credential_authority: &credential,
        };
        let bound = native
            .bind(
                "owner",
                "framed-target",
                &first_binding,
                &publication_generation()?,
                PUBLICATION_ACTIVE,
                TimeDelta::days(30),
            )
            .await?;
        ensure!(bound.outcome == BindOutcome::Inserted);
        let second_binding = ContinuationBinding {
            target: &second,
            ..first_binding
        };
        ensure!(
            native
                .bind(
                    "owner",
                    "framed-target",
                    &second_binding,
                    &publication_generation()?,
                    PUBLICATION_ACTIVE,
                    TimeDelta::days(30),
                )
                .await
                .is_err(),
            "different provider/account fields reused a binding"
        );
        ensure!(read(&native, &bound.row.continuation_identity).await? == bound.row);
        let exact_binding = ContinuationBinding {
            target: &first,
            ..second_binding
        };
        let duplicate = native
            .bind(
                "owner",
                "framed-target",
                &exact_binding,
                &publication_generation()?,
                PUBLICATION_ACTIVE,
                TimeDelta::days(30),
            )
            .await?;
        ensure!(duplicate.outcome == BindOutcome::ExistingActive);
        ensure!(duplicate.row == bound.row);
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn oversized_native_payload_is_rejected_before_the_database_gate() -> Result<()> {
    let database = isolated_database().await?;
    let result = async {
        let native = SharedNative::new(SharedContinuationDb::fixture(database.clone()).await?);
        native.authority.close_binds().await?;
        let target = target("fixture-static-key");
        let credential = static_authority("fixture-static-key");
        let generation = publication_generation()?;
        for id in [
            "x".repeat(MAX_NATIVE_PLAINTEXT_BYTES + 1),
            "\0".repeat(MAX_NATIVE_PLAINTEXT_BYTES / 2),
            "valid-native-id".into(),
        ] {
            let error = native
                .bind(
                    "owner",
                    "bounded-payload",
                    &ContinuationBinding {
                        provider_response_id: &id,
                        effective_model: "openai:gpt-5",
                        effective_effort: None,
                        causal_prefix_commitment: None,
                        target: &target,
                        credential_authority: &credential,
                    },
                    &generation,
                    PUBLICATION_PROVISIONAL,
                    TimeDelta::days(30),
                )
                .await
                .err()
                .context("closed producer accepted a bind")?;
            if id == "valid-native-id" {
                ensure!(error.to_string() == "continuation producer is closed");
            } else {
                ensure!(
                    matches!(
                        error.downcast_ref::<Rejection>(),
                        Some(Rejection::PayloadTooLarge)
                    ),
                    "oversized payload reached database gate: {error}"
                );
            }
        }
        Ok(())
    }
    .await;
    let closed = database.close().await;
    result?;
    closed?;
    Ok(())
}
