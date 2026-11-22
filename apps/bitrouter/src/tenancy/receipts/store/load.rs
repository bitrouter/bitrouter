//! A single canonical candidate reader for inline and archived receipts.
use super::*;

pub(super) async fn candidate(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
    charge: Option<&archive::Charge<'_>>,
    fresh_inline: bool,
) -> Result<Option<Candidate>> {
    match terminal(db, installation, tenant, id, charge, fresh_inline).await? {
        Some(format::Terminal::Strict(candidate)) => Ok(Some(
            std::sync::Arc::into_inner(candidate).context("receipt shared during decode")?,
        )),
        Some(format::Terminal::Ordered(_)) => anyhow::bail!("receipt format mismatch"),
        None => Ok(None),
    }
}

pub(super) async fn terminal(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
    charge: Option<&archive::Charge<'_>>,
    fresh_inline: bool,
) -> Result<Option<format::Terminal>> {
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
    ensure!(
        !fresh_inline || !archived,
        "fresh terminal receipt must remain inline"
    );
    if format::Format::decode(row.try_get("", "receipt_format")?)? == format::Format::Ordered {
        ensure!(!archived, "ordered receipt storage kind");
        archive::ensure_inline(db, installation, tenant, id).await?;
        account(2, 1024)?;
        let terminal =
            super::super::ordered::decode(db, row, installation, tenant, id, charge).await?;
        account(0, 0)?;
        return Ok(Some(terminal));
    }
    let (json, usage) = if archived {
        ensure!(inline.is_empty(), "duplicate inline receipt payload");
        let (json, usage) = archive::load(db, installation, tenant, id, account).await?;
        (json, Some(usage))
    } else {
        archive::ensure_inline(db, installation, tenant, id).await?;
        (inline, None)
    };
    let private_json: String = row.try_get("", "disposition_json")?;
    let (receipt, disposition, digest) =
        decode(&row, &json, &private_json, installation, tenant, id)?;
    let auxiliary = if disposition.auxiliary_digest.is_none() {
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
        None
    } else {
        account(1, super::super::auxiliary::MAX_BYTES * 3 + 4096)?;
        super::super::auxiliary_store::validate(
            db,
            &receipt,
            &digest,
            disposition.auxiliary_digest.as_deref(),
        )
        .await?
    };
    let expected = disposition.usage_evidence_digest.as_str();
    let usage_evidence = if let Some(usage) = usage {
        let evidence = super::super::usage_evidence::storage::Evidence::new(
            serde_json::from_str(&usage)?,
            &receipt,
        )?;
        let bytes = archive::usage_metadata(db, &receipt, &digest, expected, true, account).await?;
        ensure!(
            evidence.json == usage && evidence.json.len() == bytes && evidence.digest == expected,
            "archived usage evidence integrity"
        );
        evidence
    } else {
        super::super::usage_evidence::storage::validate_inline(
            db, &receipt, &digest, expected, charge,
        )
        .await?
    };
    let candidate = Candidate {
        receipt,
        json,
        private_json,
        digest,
        auxiliary,
        usage_evidence,
    };
    if archived {
        // Unknown history is never a valid archived representation, even if
        // all projected digests were rewritten consistently.
        archive::known(&candidate)?;
    }
    account(0, 0)?;
    Ok(Some(format::Terminal::Strict(std::sync::Arc::new(
        candidate,
    ))))
}
