//! Explicit database-wide virtual-key administration, never namespace authority.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::operator::{PrincipalMetadata, PrincipalPut};
use chrono::{DateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, DatabaseTransaction, EntityTrait, IntoActiveModel, QuerySelect, Set,
    TryInsertResult,
};

use crate::auth::entities::{api_keys, users};
use crate::gateway::shared::{authority::Authority, engine::Rejection, store::Store};

pub(super) fn validate(input: &PrincipalPut) -> Result<()> {
    crate::gateway::shared::engine::validate_id(&input.principal_id)?;
    crate::gateway::shared::engine::validate_id(&input.tenant_id)?;
    ensure!(
        !crate::auth::db::is_reserved_user_id(&input.tenant_id),
        "reserved principal tenant"
    );
    if let Some(id) = &input.policy_id {
        crate::gateway::shared::engine::validate_id(id)?;
    }
    ensure!(
        input
            .requests_per_minute
            .is_none_or(|limit| limit <= 1_000_000),
        "principal rate limit exceeds bound"
    );
    ensure!(
        input.expected_authorization_revision.is_some() || input.key_hash.is_some(),
        "principal create requires verifier"
    );
    for digest in [&input.key_hash, &input.expected_authorization_revision]
        .into_iter()
        .flatten()
    {
        ensure!(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid principal digest"
        );
    }
    input.expires_at_ms.map(timestamp).transpose()?;
    Ok(())
}

/// Head/receipt precede the same global API-key row locked by admission. A
/// namespace capability alone must never reach this writer.
pub(super) async fn put(
    authority: &Authority,
    tx: &DatabaseTransaction,
    input: &PrincipalPut,
) -> Result<PrincipalMetadata> {
    validate(input)?;
    let now = timestamp(Store::now(tx).await?)?;
    let expiry = input.expires_at_ms.map(timestamp).transpose()?;
    let row = if let Some(expected) = &input.expected_authorization_revision {
        // PostgreSQL row locking: https://www.postgresql.org/docs/current/explicit-locking.html#LOCKING-ROWS
        let row = api_keys::Entity::find_by_id(&input.principal_id)
            .lock_exclusive()
            .one(tx)
            .await?
            .ok_or(Rejection::NotFound)?;
        ensure!(
            row.user_id == input.tenant_id && revision(authority, &row)? == *expected,
            Rejection::AuthorityChanged
        );
        let mut model = row.into_active_model();
        if let Some(hash) = &input.key_hash {
            model.key_hash = Set(hash.clone());
        }
        model.active = Set(i32::from(input.active));
        model.expires_at = Set(expiry);
        model.rpm_limit = Set(input.requests_per_minute.map(i64::from));
        model.policy_id = Set(input.policy_id.clone());
        // An existing spend ceiling is preserved; shared admission explicitly
        // rejects unsupported hard ceilings instead of weakening them.
        model.update(tx).await?
    } else {
        users::Entity::insert(users::ActiveModel {
            id: Set(input.tenant_id.clone()),
            created_at: Set(now.clone()),
        })
        .on_conflict(
            OnConflict::column(users::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .do_nothing()
        .exec(tx)
        .await?;
        let inserted = api_keys::Entity::insert(api_keys::ActiveModel {
            id: Set(input.principal_id.clone()),
            key_hash: Set(input
                .key_hash
                .clone()
                .context("missing principal verifier")?),
            user_id: Set(input.tenant_id.clone()),
            spend_limit_micro_usd: Set(None),
            rpm_limit: Set(input.requests_per_minute.map(i64::from)),
            policy_id: Set(input.policy_id.clone()),
            expires_at: Set(expiry),
            active: Set(i32::from(input.active)),
            created_at: Set(now),
        })
        .on_conflict(OnConflict::new().do_nothing().to_owned())
        .do_nothing()
        .exec(tx)
        .await?;
        ensure!(
            matches!(inserted, TryInsertResult::Inserted(_)),
            Rejection::AuthorityChanged
        );
        api_keys::Entity::find_by_id(&input.principal_id)
            .one(tx)
            .await?
            .context("created principal is missing")?
    };
    metadata(authority, row)
}

pub(super) async fn read(
    authority: &Authority,
    tx: &DatabaseTransaction,
    id: &str,
) -> Result<PrincipalMetadata> {
    let row = api_keys::Entity::find_by_id(id)
        .one(tx)
        .await?
        .ok_or(Rejection::NotFound)?;
    metadata(authority, row)
}

fn revision(authority: &Authority, row: &api_keys::Model) -> Result<String> {
    authority.operator_fingerprint(
        "principal-authorization",
        &serde_json::to_vec(&(
            &row.id,
            &row.key_hash,
            &row.user_id,
            row.active,
            &row.expires_at,
            &row.policy_id,
            row.spend_limit_micro_usd,
            row.rpm_limit,
        ))?,
    )
}

fn metadata(authority: &Authority, row: api_keys::Model) -> Result<PrincipalMetadata> {
    let authorization_revision = revision(authority, &row)?;
    Ok(PrincipalMetadata {
        principal_id: row.id,
        tenant_id: row.user_id,
        authorization_revision,
        active: row.active == 1,
        expires_at_ms: row
            .expires_at
            .map(|value| DateTime::parse_from_rfc3339(&value).map(|time| time.timestamp_millis()))
            .transpose()?,
        requests_per_minute: row.rpm_limit.map(u32::try_from).transpose()?,
        policy_id: row.policy_id,
    })
}

fn timestamp(ms: i64) -> Result<String> {
    Ok(DateTime::<Utc>::from_timestamp_millis(ms)
        .context("principal timestamp is outside supported range")?
        .to_rfc3339())
}
