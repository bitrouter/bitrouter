//! A single canonical candidate reader for inline and archived receipts.
use super::*;

pub(super) async fn candidate(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
    charge: Option<&archive::Charge<'_>>,
) -> Result<Option<Candidate>> {
    let Some(row) = bounded_row(db, point(installation, tenant, id), charge).await? else {
        archive::ensure_inline(db, installation, tenant, id).await?;
        return Ok(None);
    };
    let uncharged = |_, _| Ok(());
    let account = charge.unwrap_or(&uncharged);
    let inline: String = row.try_get("", "receipt_json")?;
    let archived = match row.try_get::<i64>("", "storage_kind")? {
        0 => false,
        1 => true,
        _ => anyhow::bail!("receipt storage kind"),
    };
    let (json, usage) = if archived {
        ensure!(inline.is_empty(), "duplicate inline receipt payload");
        archive::load(db, installation, tenant, id, account).await?
    } else {
        archive::ensure_inline(db, installation, tenant, id).await?;
        (inline, None)
    };
    let mut candidate = decode(&row, json, installation, tenant, id)?;
    let disposition: StoredDisposition = serde_json::from_str(&candidate.private_json)?;
    if disposition.auxiliary_digest.is_none() {
        account(1, 1024)?;
        let absent = Query::select()
            .expr(Expr::val(1))
            .from(Alias::new("tenancy_receipt_auxiliary"))
            .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
            .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
            .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
            .limit(1)
            .to_owned();
        ensure!(
            db.query_one(db.get_database_backend().build(&absent))
                .await?
                .is_none(),
            "unexpected auxiliary receipt evidence"
        );
    } else {
        account(1, super::super::auxiliary::MAX_BYTES * 3 + 4096)?;
        super::super::auxiliary_store::validate(
            db,
            &mut candidate,
            disposition.auxiliary_digest.as_deref(),
        )
        .await?;
    }
    if archived {
        if let Some(json) = usage {
            let evidence = super::super::usage_evidence::storage::Evidence::new(
                serde_json::from_str(&json)?,
                &candidate.receipt,
            )?;
            ensure!(evidence.json == json, "archived usage canonical bytes");
            candidate.usage_evidence = Some(evidence);
        }
        archive::usage_metadata(db, &candidate, true, account).await?;
        // Unknown history is never a valid archived representation, even if
        // all projected digests were rewritten consistently.
        archive::known(&candidate)?;
    } else {
        archive::usage_metadata(db, &candidate, false, account).await?;
        super::super::usage_evidence::storage::validate_inline(
            db,
            &mut candidate,
            disposition.usage_evidence_digest.as_deref(),
            charge,
        )
        .await?;
    }
    account(0, 0)?;
    Ok(Some(candidate))
}
