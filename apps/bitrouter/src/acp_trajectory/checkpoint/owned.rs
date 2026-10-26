//! Passive checkpoint publication inside the existing authority transaction.
//! Only bounded references/cursors are stored here; raw capture stays in harness.

use crate::{
    harness::trajectory::owned::{
        Association,
        entities::{checkpoints, members, records},
        io,
        prefix::Prefix,
    },
    storage::{operation_budget::OperationBudget, scope::KeyDomain},
};
use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};

const MANIFEST_BYTES: usize = 256 * 1024;
const CURSOR_KIND: &str = "owned_stopped_prefix";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    checkpoint: String,
    prefix_digest: String,
    prefix: Prefix,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u32,
    connection: String,
    session: String,
    watermark: i64,
    checkpoint: String,
}

/// The facade will expose only this bounded reference after a live read check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Summary {
    pub(crate) checkpoint_id: String,
    pub(crate) watermark: i64,
    pub(crate) prefix_digest: String,
    pub(crate) observations: usize,
}

/// Off does no discovery. Observe and passive Explore share this reference-only
/// path; no judge job, paid reservation, route activation or cost coverage claim.
pub(crate) async fn freeze_in(
    tx: &DatabaseTransaction,
    association: &Association,
    native: &str,
    budget: &OperationBudget<'_>,
) -> Result<Option<Summary>> {
    let Some(prefix) = association.stopped_prefix_in(tx, native, budget).await? else {
        return Ok(None);
    };
    let digest = prefix.digest()?;
    let id = association.key(
        KeyDomain::CaptureCheckpoint,
        &[
            association.connection(),
            &prefix.session_key,
            &prefix.watermark.to_string(),
            &digest,
        ],
    )?;
    let cursor_id = association.key(
        KeyDomain::CaptureSchedule,
        &[association.connection(), &prefix.session_key],
    )?;
    let manifest = Manifest {
        version: 1,
        checkpoint: id.clone(),
        prefix_digest: digest.clone(),
        prefix,
    };
    let body = io::json(&manifest, MANIFEST_BYTES)?;
    let cursor = Cursor {
        version: 1,
        connection: association.connection().to_owned(),
        session: manifest.prefix.session_key.clone(),
        watermark: manifest.prefix.watermark,
        checkpoint: id.clone(),
    };
    let cursor_body = io::json(&cursor, 4096)?;
    let cp_query = checkpoints::Entity::find_by_id(&id)
        .filter(association.predicate("acp_checkpoints"))
        .filter(checkpoints::Column::SessionKey.eq(&manifest.prefix.session_key));
    let existing = io::one(tx, cp_query, MANIFEST_BYTES + 4096, budget).await?;
    let record_query = records::Entity::find_by_id(&cursor_id)
        .filter(association.predicate("checkpoint_evolution"))
        .filter(records::Column::ScopeId.eq(association.connection()))
        .filter(records::Column::Kind.eq(CURSOR_KIND))
        .filter(records::Column::SessionKey.eq(&manifest.prefix.session_key));
    let prior = io::one(tx, record_query, 8192, budget).await?;
    let old = prior
        .as_ref()
        .map(|row| -> Result<Cursor> {
            association.validate(
                row.installation_id.as_deref(),
                row.tenant_id.as_deref(),
                row.creator_principal_id.as_deref(),
            )?;
            let old: Cursor = serde_json::from_str(&row.body)?;
            ensure!(
                old.version == 1
                    && old.connection == association.connection()
                    && old.session == cursor.session
                    && old.watermark <= cursor.watermark
                    && row.revision >= 0,
                "capture checkpoint cursor mismatch"
            );
            Ok(old)
        })
        .transpose()?;
    if let Some(row) = existing {
        association.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        ensure!(
            !row.deleted && row.watermark == cursor.watermark && row.manifest_json == body,
            "capture checkpoint replay mismatch"
        );
        let member = io::one(
            tx,
            members::Entity::find_by_id((id.clone(), cursor.session.clone()))
                .filter(association.predicate("acp_checkpoint_members")),
            4096,
            budget,
        )
        .await?
        .context("capture checkpoint member absent")?;
        association.validate(
            member.installation_id.as_deref(),
            member.tenant_id.as_deref(),
            member.creator_principal_id.as_deref(),
        )?;
        ensure!(
            old.as_ref() == Some(&cursor),
            "capture checkpoint replay cursor absent or changed"
        );
        // Exact duplicate closure is fully checked BEFORE any DML. Rollback by
        // the actor must not be what hides a repaired or tentative graph.
        budget.check()?;
        return Ok(Some(summary(&manifest)));
    }
    ensure!(
        old.as_ref()
            .is_none_or(|old| old.watermark < cursor.watermark),
        "checkpoint cursor points at missing closure"
    );
    if let Some(old) = &old {
        // Advancing the cursor cannot conceal deletion/corruption of its prior
        // immutable closure. Only metadata/references, never a transcript load.
        let previous = io::one(
            tx,
            checkpoints::Entity::find_by_id(&old.checkpoint)
                .filter(association.predicate("acp_checkpoints"))
                .filter(checkpoints::Column::SessionKey.eq(&old.session)),
            MANIFEST_BYTES + 4096,
            budget,
        )
        .await?
        .context("prior owned checkpoint absent")?;
        association.validate(
            previous.installation_id.as_deref(),
            previous.tenant_id.as_deref(),
            previous.creator_principal_id.as_deref(),
        )?;
        let previous_manifest: Manifest = serde_json::from_str(&previous.manifest_json)?;
        let previous_len = usize::try_from(old.watermark)?;
        let previous_refs = manifest
            .prefix
            .references
            .get(..previous_len)
            .context("prior checkpoint watermark exceeds current prefix")?;
        let previous_id = association.key(
            KeyDomain::CaptureCheckpoint,
            &[
                association.connection(),
                &old.session,
                &old.watermark.to_string(),
                &previous_manifest.prefix_digest,
            ],
        )?;
        ensure!(
            !previous.deleted
                && previous.watermark == old.watermark
                && previous_manifest.version == 1
                && previous_manifest.checkpoint == old.checkpoint
                && previous_manifest.prefix.descriptor == *association.descriptor()
                && previous_manifest.prefix.connection == association.connection()
                && previous_manifest.prefix.native == manifest.prefix.native
                && previous_manifest.prefix.setup == manifest.prefix.setup
                && previous_manifest.prefix.references == previous_refs
                && previous_len > 0
                && previous_id == old.checkpoint
                && previous_manifest.prefix.session_key == old.session
                && previous_manifest.prefix.watermark == old.watermark
                && previous_manifest.prefix_digest == previous_manifest.prefix.digest()?,
            "prior owned checkpoint changed"
        );
        let previous_member = io::one(
            tx,
            members::Entity::find_by_id((old.checkpoint.clone(), old.session.clone()))
                .filter(association.predicate("acp_checkpoint_members")),
            4096,
            budget,
        )
        .await?
        .context("prior owned checkpoint member absent")?;
        association.validate(
            previous_member.installation_id.as_deref(),
            previous_member.tenant_id.as_deref(),
            previous_member.creator_principal_id.as_deref(),
        )?;
    }
    budget.charge(3, body.len() + cursor_body.len() + 8192)?;
    let meta = association.metadata()?;
    ensure!(
        checkpoints::Entity::insert(checkpoints::ActiveModel {
            checkpoint_id: Set(id.clone()),
            session_key: Set(cursor.session.clone()),
            watermark: Set(cursor.watermark),
            manifest_json: Set(body),
            deleted: Set(false),
            installation_id: Set(meta.installation.clone()),
            tenant_id: Set(meta.tenant.clone()),
            creator_principal_id: Set(meta.creator.clone()),
        })
        .exec_without_returning(tx)
        .await?
            == 1,
        "owned checkpoint insert missing"
    );
    budget.check()?;
    ensure!(
        members::Entity::insert(members::ActiveModel {
            checkpoint_id: Set(id),
            session_key: Set(cursor.session.clone()),
            installation_id: Set(meta.installation.clone()),
            tenant_id: Set(meta.tenant.clone()),
            creator_principal_id: Set(meta.creator.clone()),
        })
        .exec_without_returning(tx)
        .await?
            == 1,
        "owned checkpoint member insert missing"
    );
    budget.check()?;
    if let Some(prior) = prior {
        let revision = prior
            .revision
            .checked_add(1)
            .context("capture cursor revision overflow")?;
        let updated = records::Entity::update_many()
            .filter(association.predicate("checkpoint_evolution"))
            .filter(records::Column::RecordId.eq(cursor_id))
            .filter(records::Column::Revision.eq(prior.revision))
            .col_expr(records::Column::Revision, Expr::value(revision))
            .col_expr(records::Column::Body, Expr::value(cursor_body))
            .exec(tx)
            .await?;
        ensure!(updated.rows_affected == 1, "capture cursor CAS failed");
    } else {
        ensure!(
            records::Entity::insert(records::ActiveModel {
                record_id: Set(cursor_id),
                scope_id: Set(association.connection().to_owned()),
                kind: Set(CURSOR_KIND.into()),
                session_key: Set(Some(cursor.session)),
                revision: Set(0),
                body: Set(cursor_body),
                installation_id: Set(meta.installation),
                tenant_id: Set(meta.tenant),
                creator_principal_id: Set(meta.creator),
            })
            .exec_without_returning(tx)
            .await?
                == 1,
            "capture cursor insert missing"
        );
    }
    budget.check()?;
    Ok(Some(summary(&manifest)))
}

fn summary(manifest: &Manifest) -> Summary {
    Summary {
        checkpoint_id: manifest.checkpoint.clone(),
        watermark: manifest.prefix.watermark,
        prefix_digest: manifest.prefix_digest.clone(),
        observations: manifest.prefix.references.len() + manifest.prefix.setup.len(),
    }
}
