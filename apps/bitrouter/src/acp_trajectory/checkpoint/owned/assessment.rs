//! Load only the exact checked checkpoint's cited prefix for a private judge.
use super::{MANIFEST_BYTES, Manifest, Summary};
use crate::{
    acp_trajectory::CanonicalEvent,
    core::evolution::evidence::EvidencePacket,
    harness::trajectory::owned::{
        Association,
        entities::{checkpoints, events, members, records},
        io, prefix,
    },
    storage::operation_budget::OperationBudget,
};
use anyhow::{Context, Result, ensure};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder};
use std::collections::BTreeMap;

pub(crate) async fn evidence_in(
    tx: &DatabaseTransaction,
    association: &Association,
    native: &str,
    expected: &Summary,
    budget: &OperationBudget<'_>,
) -> Result<(String, EvidencePacket)> {
    // The connection/session lock precedes all consistent reads on MySQL RR.
    let current = association
        .stopped_prefix_in(tx, native, budget)
        .await?
        .context("assessment needs a stopped capture prefix")?;
    let row = io::one(
        tx,
        checkpoints::Entity::find_by_id(&expected.checkpoint_id)
            .filter(association.predicate("acp_checkpoints"))
            .filter(checkpoints::Column::SessionKey.eq(&current.session_key)),
        MANIFEST_BYTES + 4096,
        budget,
    )
    .await?
    .context("assessment checkpoint absent")?;
    association.validate(
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
    )?;
    let manifest: Manifest = serde_json::from_str(&row.manifest_json)?;
    ensure!(
        !row.deleted
            && manifest.version == 1
            && manifest.prefix == current
            && super::summary(&manifest) == *expected
            && row.watermark == current.watermark
            && manifest.prefix_digest == current.digest()?,
        "assessment checkpoint changed"
    );
    let evidence = project_in(tx, association, &manifest, budget).await?;
    Ok((current.session_key, evidence))
}

async fn project_in(
    tx: &DatabaseTransaction,
    association: &Association,
    manifest: &Manifest,
    budget: &OperationBudget<'_>,
) -> Result<EvidencePacket> {
    let current = &manifest.prefix;
    let member = io::one(
        tx,
        members::Entity::find_by_id((manifest.checkpoint.clone(), current.session_key.clone()))
            .filter(association.predicate("acp_checkpoint_members")),
        4096,
        budget,
    )
    .await?
    .context("assessment checkpoint member absent")?;
    association.validate(
        member.installation_id.as_deref(),
        member.tenant_id.as_deref(),
        member.creator_principal_id.as_deref(),
    )?;
    let count = current
        .references
        .len()
        .checked_add(current.setup.len())
        .context("assessment reference count overflow")?;
    ensure!(
        count <= crate::harness::trajectory::owned::MAX_EVENTS,
        "assessment reference bound exceeded"
    );
    let refs: BTreeMap<_, _> = current
        .references
        .iter()
        .chain(&current.setup)
        .map(|reference| (reference.sequence, reference))
        .collect();
    ensure!(refs.len() == count, "duplicate assessment reference");
    let rows = io::rows(
        tx,
        association
            .events()
            .filter(events::Column::Sequence.is_in(refs.keys().copied()))
            .order_by_asc(events::Column::Sequence),
        refs.len(),
        crate::harness::trajectory::owned::MAX_CAPTURE_BYTES,
        budget,
    )
    .await?;
    ensure!(rows.len() == refs.len(), "assessment reference is missing");
    let mut projected = Vec::with_capacity(rows.len());
    for row in rows {
        association.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        let reference = refs
            .get(&row.sequence)
            .context("unexpected assessment event")?;
        ensure!(
            prefix::reference(&row)? == **reference && !row.replay,
            "assessment evidence digest changed"
        );
        projected.push(CanonicalEvent {
            node_id: reference.node_id(),
            sequence: row.sequence,
            captured_at: row.captured_at,
            event: serde_json::from_str(&row.event_json)?,
        });
    }
    let evidence = EvidencePacket::from_owned(&manifest.checkpoint, current, &projected)?;
    io::json(
        &evidence,
        crate::harness::trajectory::owned::MAX_CAPTURE_BYTES,
    )?;
    budget.check()?;
    Ok(evidence)
}

pub(crate) async fn validate_in(
    tx: &DatabaseTransaction,
    association: &Association,
    session: &str,
    checkpoint: &str,
    expected_digest: Option<&str>,
    budget: &OperationBudget<'_>,
) -> Result<i64> {
    let row = io::one(
        tx,
        checkpoints::Entity::find_by_id(checkpoint)
            .filter(association.predicate("acp_checkpoints"))
            .filter(checkpoints::Column::SessionKey.eq(session)),
        MANIFEST_BYTES + 4096,
        budget,
    )
    .await?
    .context("assessment checkpoint absent")?;
    association.validate(
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
    )?;
    let value: Manifest = serde_json::from_str(&row.manifest_json)?;
    ensure!(
        !row.deleted
            && value.version == 1
            && value.checkpoint == checkpoint
            && expected_digest.is_none_or(|digest| value.prefix_digest == digest)
            && value.prefix.digest()? == value.prefix_digest
            && value.prefix.descriptor == *association.descriptor()
            && value.prefix.connection == association.connection()
            && value.prefix.session_key == session
            && value.prefix.watermark == row.watermark,
        "retained assessment checkpoint mismatch"
    );
    let member = io::one(
        tx,
        members::Entity::find_by_id((checkpoint.to_owned(), session.to_owned()))
            .filter(association.predicate("acp_checkpoint_members")),
        4096,
        budget,
    )
    .await?
    .context("assessment checkpoint member absent")?;
    association.validate(
        member.installation_id.as_deref(),
        member.tenant_id.as_deref(),
        member.creator_principal_id.as_deref(),
    )?;
    Ok(row.watermark)
}

/// Read a historical owned prefix without weakening paid evidence_in's current-prefix check.
pub(crate) async fn review_in(
    tx: &DatabaseTransaction,
    association: &Association,
    native: &str,
    checkpoint: Option<&str>,
    budget: &OperationBudget<'_>,
) -> Result<(String, Summary, i64, EvidencePacket)> {
    use crate::storage::{operation_budget::Error, scope::KeyDomain};
    use bitrouter_ai::tenancy::{configuration::LearningMode, operation::ErrorCode};
    ensure!(
        association.descriptor().mode != LearningMode::Off,
        Error::Rejected(ErrorCode::NotFound)
    );
    let (_, state) = association.connection_in(tx, budget).await?;
    let session = association.session_in(tx, native, budget).await?;
    ensure!(
        state.sessions.get(native) == Some(&session.head),
        "owned review session watermark mismatch"
    );
    let selected;
    let mut cursor_watermark = None;
    let checkpoint = match checkpoint {
        Some(id) => id,
        None => {
            let id = association.key(
                KeyDomain::CaptureSchedule,
                &[association.connection(), &session.session_key],
            )?;
            let row = io::one(
                tx,
                records::Entity::find_by_id(id)
                    .filter(association.predicate("checkpoint_evolution"))
                    .filter(records::Column::ScopeId.eq(association.connection()))
                    .filter(records::Column::Kind.eq(super::CURSOR_KIND))
                    .filter(records::Column::SessionKey.eq(&session.session_key)),
                8192,
                budget,
            )
            .await?
            .ok_or(Error::Rejected(ErrorCode::NotFound))?;
            association.validate(
                row.installation_id.as_deref(),
                row.tenant_id.as_deref(),
                row.creator_principal_id.as_deref(),
            )?;
            let cursor: super::Cursor = serde_json::from_str(&row.body)?;
            ensure!(
                cursor.version == 1
                    && cursor.connection == association.connection()
                    && cursor.session == session.session_key
                    && cursor.watermark <= session.head
                    && row.revision >= 0
                    && io::json(&cursor, 4096)? == row.body,
                "owned review cursor mismatch"
            );
            cursor_watermark = Some(cursor.watermark);
            selected = cursor;
            &selected.checkpoint
        }
    };
    let row = io::one(
        tx,
        checkpoints::Entity::find_by_id(checkpoint)
            .filter(association.predicate("acp_checkpoints"))
            .filter(checkpoints::Column::SessionKey.eq(&session.session_key)),
        MANIFEST_BYTES + 4096,
        budget,
    )
    .await?
    .ok_or(Error::Rejected(ErrorCode::NotFound))?;
    association.validate(
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
    )?;
    let manifest: Manifest = serde_json::from_str(&row.manifest_json)?;
    ensure!(
        !row.deleted
            && manifest.version == 1
            && manifest.checkpoint == row.checkpoint_id
            && manifest.prefix.descriptor == *association.descriptor()
            && manifest.prefix.connection == association.connection()
            && manifest.prefix.native == native
            && manifest.prefix.session_key == session.session_key
            && manifest.prefix.watermark == row.watermark
            && row.watermark <= session.head
            && cursor_watermark.is_none_or(|watermark| watermark == row.watermark)
            && manifest.prefix_digest == manifest.prefix.digest()?
            && io::json(&manifest, MANIFEST_BYTES)? == row.manifest_json,
        "owned historical checkpoint mismatch"
    );
    let evidence = project_in(tx, association, &manifest, budget).await?;
    Ok((
        session.session_key,
        super::summary(&manifest),
        session.head,
        evidence,
    ))
}

/// Bounded historical namespace data; this cannot construct a live host binding.
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) fn recovery_prefix(bytes: &str) -> Result<super::Prefix> {
    ensure!(bytes.len() <= MANIFEST_BYTES, "paid checkpoint bound");
    let manifest: Manifest = serde_json::from_str(bytes)?;
    ensure!(
        manifest.version == 1
            && manifest.prefix_digest == manifest.prefix.digest()?
            && io::json(&manifest, MANIFEST_BYTES)? == bytes,
        "paid checkpoint encoding"
    );
    Ok(manifest.prefix)
}
