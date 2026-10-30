//! The same035+037 transaction, namespace and bounded decoder on every backend.
use super::*;
use crate::tenancy::receipts::{
    auxiliary::{
        Snapshot,
        count::{Envelope, Fact, Outcome as CountOutcome},
    },
    auxiliary_store,
};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
};
use sha2::{Digest, Sha256};

fn reference(tenant: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: tenant.try_into()?,
        resource_kind: ResourceKind::Credential,
        resource_id: "slot".try_into()?,
        revision_id: "r1".try_into()?,
    })
}
fn counts(tenant: &str, outcome: CountOutcome) -> Result<Snapshot> {
    Ok(Snapshot::Counts(Envelope {
        version: 2,
        counts: vec![Fact {
            ordinal: 0,
            work: "c53bbdf9-7771-41db-88af-a5017795a020".into(),
            credential: reference(tenant)?,
            candidate: "01".repeat(32),
            prompt: "02".repeat(32),
            generation_request: Some("03".repeat(32)),
            count_request: Some("04".repeat(32)),
            http_status: Some(200),
            outcome,
            input_tokens: (outcome == CountOutcome::Counted).then_some(11),
        }],
    }))
}
fn value(tenant: &str, id: &str, outcome: CountOutcome) -> Result<Candidate> {
    candidate("installation", tenant, id, "count-creator")?.with_auxiliary(counts(tenant, outcome)?)
}
async fn commit_pair(db: &DatabaseConnection, value: &Candidate) -> Result<bool> {
    let transaction = db.begin().await?;
    let result = async {
        let inserted = insert(&transaction, value).await?;
        auxiliary_store::insert(&transaction, value).await?;
        Ok::<_, anyhow::Error>(inserted)
    }
    .await;
    match result {
        Ok(inserted) => {
            transaction.commit().await?;
            Ok(inserted)
        }
        Err(error) => {
            transaction.rollback().await?;
            Err(error)
        }
    }
}
async fn evidence(db: &impl ConnectionTrait, value: &Candidate) -> Result<Option<String>> {
    let query = Query::select()
        .from(Alias::new("tenancy_receipt_auxiliary"))
        .column(Alias::new("evidence_json"))
        .and_where(
            Expr::col(Alias::new("installation_id")).eq(value.receipt.installation_id.as_str()),
        )
        .and_where(Expr::col(Alias::new("tenant_id")).eq(value.receipt.tenant_id.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(value.receipt.execution_id.as_str()))
        .to_owned();
    db.query_one(db.get_database_backend().build(&query))
        .await?
        .map(|row| row.try_get("", "evidence_json").map_err(Into::into))
        .transpose()
}
pub(super) async fn before_reconnect(db: &DatabaseConnection) -> Result<Candidate> {
    let counted = value("tenant", "count-known", CountOutcome::Counted)?;
    let rolled = value("tenant", "count-rolled", CountOutcome::Counted)?;
    let transaction = db.begin().await?;
    let written = async {
        ensure!(insert(&transaction, &rolled).await?);
        auxiliary_store::insert(&transaction, &rolled).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let rollback = transaction.rollback().await;
    written?;
    rollback?;
    ensure!(
        lookup(
            db,
            &rolled.receipt.installation_id,
            &rolled.receipt.tenant_id,
            &rolled.receipt.execution_id
        )
        .await?
        .is_none()
    );
    ensure!(evidence(db, &rolled).await?.is_none());
    ensure!(commit_pair(db, &counted).await?);
    ensure!(!commit_pair(db, &counted).await?);
    // Identical IDs/commitments in another namespace are independent records.
    let foreign = value("Tenant", "count-known", CountOutcome::Counted)?;
    ensure!(commit_pair(db, &foreign).await?);
    ensure!(
        counted
            .auxiliary
            .as_ref()
            .context("source evidence")?
            .digest
            != foreign
                .auxiliary
                .as_ref()
                .context("foreign evidence")?
                .digest
    );
    ensure!(
        commit_pair(
            db,
            &value("tenant", "count-unknown", CountOutcome::Unknown)?
        )
        .await?
    );
    // A real immutable conflict must rollback; no failed transaction is reused.
    let mut changed = counts("tenant", CountOutcome::Counted)?;
    if let Snapshot::Counts(envelope) = &mut changed {
        envelope.counts[0].input_tokens = Some(12);
    }
    let changed = candidate("installation", "tenant", "count-known", "count-creator")?
        .with_auxiliary(changed)?;
    ensure!(commit_pair(db, &changed).await.is_err());
    after_reconnect(db, &counted).await?;
    Ok(counted)
}
pub(super) async fn after_reconnect(db: &DatabaseConnection, value: &Candidate) -> Result<()> {
    let loaded = lookup_candidate(
        db,
        &value.receipt.installation_id,
        &value.receipt.tenant_id,
        &value.receipt.execution_id,
    )
    .await?
    .context("count receipt after reconnect")?;
    ensure!(loaded.json == value.json && loaded.digest == value.digest);
    ensure!(
        loaded.receipt.principal_id.as_str() == "count-creator"
            && loaded.receipt.attempts.is_empty()
    );
    ensure!(
        loaded.receipt.usage.input_tokens.is_none()
            && loaded.receipt.usage.provenance == Provenance::Unknown
    );
    let expected = &value.auxiliary.as_ref().context("count evidence")?.json;
    ensure!(evidence(db, value).await?.as_ref() == Some(expected));
    ensure!(loaded.auxiliary.as_ref().context("decoded evidence")?.json == *expected);
    ensure!(
        lookup(
            db,
            &"other-installation".try_into()?,
            &value.receipt.tenant_id,
            &value.receipt.execution_id
        )
        .await?
        .is_none()
    );
    ensure!(
        lookup(
            db,
            &value.receipt.installation_id,
            &"TENANT".try_into()?,
            &value.receipt.execution_id
        )
        .await?
        .is_none()
    );
    let inventory = inventory(db, &value.receipt.installation_id).await?;
    ensure!(
        inventory
            .unknown
            .iter()
            .any(|entry| entry.0.as_str() == "tenant" && entry.1.as_str() == "count-unknown")
    );
    ensure!(
        !inventory
            .unknown
            .iter()
            .any(|entry| entry.1.as_str() == "count-known")
    );
    Ok(())
}
#[tokio::test]
async fn count_pair_commits_atomically_with_scope_and_known_usage_kept_separate() -> Result<()> {
    let db = open().await?;
    let result = before_reconnect(&db).await;
    let closed = db.close().await;
    result?;
    closed?;
    Ok(())
}
#[test]
fn historical_refresh_array_bytes_and_digest_are_unchanged_and_count_is_strictly_versioned()
-> Result<()> {
    use crate::tenancy::receipts::auxiliary::{Fact as RefreshFact, Outcome as RefreshOutcome};
    let plain = candidate("installation", "tenant", "refresh", "creator")?;
    let reference = reference("tenant")?;
    let slot = crate::tenancy::oauth::storage::slot(&plain.receipt.installation_id, &reference)?;
    let facts = vec![RefreshFact {
        credential: reference,
        slot,
        work: "c53bbdf9-7771-41db-88af-a5017795a020".into(),
        generation: 1,
        request_digest: Some("03".repeat(32)),
        http_status: Some(200),
        outcome: RefreshOutcome::Refreshed,
    }];
    let original = serde_json::to_string(&facts)?;
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.receipt.auxiliary.v1");
    for part in ["installation", "tenant", "refresh", original.as_str()] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let expected = hex::encode(hash.finalize());
    let wrapped = plain.with_auxiliary(facts)?;
    let evidence = wrapped.auxiliary.context("refresh evidence")?;
    ensure!(evidence.json == original && evidence.digest == expected);
    let encoded = serde_json::to_value(counts("tenant", CountOutcome::Counted)?)?;
    for mutation in [
        "mixed",
        "version",
        "ordinal",
        "foreign",
        "tokens",
        "status",
        "duplicate",
    ] {
        let mut changed = encoded.clone();
        match mutation {
            "mixed" => changed["refresh"] = serde_json::json!([]),
            "version" => changed["version"] = serde_json::json!(3),
            "ordinal" => changed["counts"][0]["ordinal"] = serde_json::json!(1),
            "foreign" => {
                changed["counts"][0]["credential"]["owner_id"] = serde_json::json!("other-tenant")
            }
            "tokens" => changed["counts"][0]["input_tokens"] = serde_json::Value::Null,
            "status" => changed["counts"][0]["http_status"] = serde_json::json!(403),
            "duplicate" => changed["counts"]
                .as_array_mut()
                .context("counts array")?
                .push(encoded["counts"][0].clone()),
            _ => anyhow::bail!("unknown corruption"),
        }
        let result = serde_json::from_value::<Snapshot>(changed)
            .map_err(anyhow::Error::from)
            .and_then(|snapshot| {
                candidate("installation", "tenant", "count", "creator")?.with_auxiliary(snapshot)
            });
        ensure!(result.is_err(), "accepted corruption {mutation}");
    }
    Ok(())
}
#[tokio::test]
async fn point_lookup_and_inventory_reject_noncanonical_or_corrupt_count_evidence() -> Result<()> {
    for corruption in [
        "UPDATE tenancy_receipt_auxiliary SET unknown_outcome=1",
        "UPDATE tenancy_receipt_auxiliary SET evidence_json=' ' || evidence_json",
        "UPDATE tenancy_receipt_auxiliary SET evidence_digest='invalid'",
        "UPDATE tenancy_receipt_auxiliary SET tenant_id='other-tenant'",
        "UPDATE tenancy_receipt_auxiliary SET evidence_json=CAST(zeroblob(16385) AS TEXT)",
    ] {
        let db = open().await?;
        let result = async {
            let value = value("tenant", "count", CountOutcome::Counted)?;
            commit_pair(&db, &value).await?;
            db.execute_unprepared(corruption).await?;
            ensure!(
                lookup(
                    &db,
                    &value.receipt.installation_id,
                    &value.receipt.tenant_id,
                    &value.receipt.execution_id
                )
                .await
                .is_err()
            );
            ensure!(
                inventory(&db, &value.receipt.installation_id)
                    .await
                    .is_err()
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let closed = db.close().await;
        result?;
        closed?;
    }
    Ok(())
}

#[test]
fn unknown_auxiliary_never_rewrites_already_known_model_usage() -> Result<()> {
    use bitrouter_ai::tenancy::execution::{Attempt, Target};
    let mut receipt = candidate("installation", "tenant", "known-model", "creator")?.receipt;
    receipt.usage = Usage {
        input_tokens: Some(4),
        output_tokens: Some(2),
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        provenance: Provenance::Provider,
        complete: false,
    };
    receipt.attempts.push(Attempt {
        attempt_id: "model-attempt".try_into()?,
        target: Some(Target {
            provider_id: "provider".try_into()?,
            model_id: "model".into(),
            account_id: "account".try_into()?,
            credential_id: "credential".try_into()?,
            credential_version: "r1".try_into()?,
        }),
        outcome: Outcome::Completed,
        usage: receipt.usage.clone(),
        native_usage: Vec::new(),
    });
    let before = serde_json::to_string(&receipt)?;
    let value = Candidate::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Started,
            statuses: vec![Some(200)],
            auxiliary_digest: None,
        },
    )?
    .with_auxiliary(counts("tenant", CountOutcome::Unknown)?)?;
    ensure!(value.auxiliary_unknown() && value.auxiliary_unclean());
    ensure!(
        value.json == before
            && value.receipt.usage.input_tokens == Some(4)
            && value.receipt.usage.output_tokens == Some(2)
    );
    ensure!(
        value.receipt.attempts.len() == 1 && value.receipt.usage.provenance == Provenance::Provider
    );
    Ok(())
}
