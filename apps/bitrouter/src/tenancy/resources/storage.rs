//! Adapter to the single immutable resource store. All cleartext metadata is
//! authenticated; a stored fingerprint is never accepted as ownership proof.

use super::super::configuration_store::{self, StoredResource};
use super::{
    COMPILER_VERSION, MAX_ARTIFACT_BYTES, MAX_CLOSURE, Resources,
    manifest::{Artifact, Import, bounded_bytes},
};
use crate::runtime_crypto::SealedSecret;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{configuration::ResourceRef, identity::Timestamp};
use sea_orm::{
    ConnectionTrait, DatabaseBackend,
    sea_query::{Alias, Expr, Func, Query},
};

pub(super) async fn has_resources(db: &impl ConnectionTrait) -> Result<bool> {
    let query = Query::select()
        .expr(Expr::val(1))
        .from(Alias::new("tenancy_resource_revisions"))
        .limit(1)
        .to_owned();
    Ok(db
        .query_one(db.get_database_backend().build(&query))
        .await?
        .is_some())
}

pub(super) async fn references(db: &impl ConnectionTrait) -> Result<Vec<ResourceRef>> {
    let mut query = Query::select();
    query
        .from(Alias::new("tenancy_resource_revisions"))
        .limit((MAX_CLOSURE + 1) as u64);
    // As in configuration_store, cap cells in SQL before the driver allocates
    // strings. Do not filter corrupt rows out: NULL projections fail catalog open.
    for (name, limit) in [
        ("owner_kind", 32_u64),
        ("owner_id", 128),
        ("resource_kind", 32),
        ("resource_id", 128),
        ("revision_id", 128),
    ] {
        let length = if db.get_database_backend() == DatabaseBackend::Sqlite {
            Func::cust(Alias::new("length"))
                .arg(Expr::col(Alias::new(name)).cast_as(Alias::new("BLOB")))
        } else {
            Func::cust(Alias::new("octet_length")).arg(Expr::col(Alias::new(name)))
        };
        query.expr_as(
            Expr::case(Expr::expr(length).lte(limit), Expr::col(Alias::new(name)))
                .finally(Expr::val(Option::<String>::None)),
            Alias::new(name),
        );
    }
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await?;
    ensure!(
        rows.len() <= MAX_CLOSURE,
        "retained resource capacity exceeded"
    );
    rows.into_iter()
        .map(|row| {
            let owner: String = row.try_get("", "owner_kind")?;
            let kind: String = row.try_get("", "resource_kind")?;
            Ok(ResourceRef {
                owner_kind: serde_json::from_value(serde_json::Value::String(owner))?,
                owner_id: row.try_get::<String>("", "owner_id")?.try_into()?,
                resource_kind: serde_json::from_value(serde_json::Value::String(kind))?,
                resource_id: row.try_get::<String>("", "resource_id")?.try_into()?,
                revision_id: row.try_get::<String>("", "revision_id")?.try_into()?,
            })
        })
        .collect()
}

pub(super) async fn retain(
    resources: &Resources,
    db: &impl ConnectionTrait,
    artifact: &Artifact,
) -> Result<()> {
    let key = resources.key.as_ref().context("resource key unavailable")?;
    let bytes = bounded_bytes(artifact, MAX_ARTIFACT_BYTES)?;
    let dependencies = artifact.definition.dependencies();
    let oauth = matches!(
        &artifact.definition,
        super::manifest::Definition::Credential { oauth: Some(_), .. }
    );
    let version = if oauth {
        "tenancy-oauth-resource-v1"
    } else {
        COMPILER_VERSION
    };
    let purpose = if oauth {
        "owned-oauth-seed-v1"
    } else {
        "resource"
    };
    let binding = (
        &resources.installation,
        &artifact.reference,
        version,
        &dependencies,
    );
    let fingerprint_input = bounded_bytes(&(&binding, &artifact), 2 * MAX_ARTIFACT_BYTES)?;
    let fingerprint = key.fingerprint(
        "tenancy-resource-v1",
        "immutable-definition",
        &fingerprint_input,
    )?;
    if let Some(existing) = configuration_store::resource(db, &artifact.reference).await? {
        let retained = authenticate(resources, existing.clone())?;
        ensure!(
            existing.content_digest.as_str() == fingerprint
                && bounded_bytes(&retained, MAX_ARTIFACT_BYTES)? == bytes,
            "immutable resource revision conflicts"
        );
        // Retain the original random nonce, ciphertext and creation time.
        return Ok(());
    }
    let sealed = key.seal_bound(purpose, &binding, &bytes)?;
    configuration_store::insert_resource(
        db,
        &StoredResource {
            reference: artifact.reference.clone(),
            compiler_version: version.into(),
            payload: serde_json::to_value(sealed)?,
            dependencies: serde_json::to_value(dependencies)?,
            content_digest: fingerprint.try_into()?,
            created_at: Timestamp::try_from(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            )?,
        },
    )
    .await
}

pub(super) fn authenticate(resources: &Resources, record: StoredResource) -> Result<Artifact> {
    ensure!(
        record.compiler_version == COMPILER_VERSION
            || record.compiler_version == "tenancy-oauth-resource-v1",
        "unsupported resource compiler"
    );
    let key = resources.key.as_ref().context("resource key unavailable")?;
    let dependencies: Vec<ResourceRef> = serde_json::from_value(record.dependencies)?;
    ensure!(
        dependencies.len() <= MAX_CLOSURE,
        "resource dependency bound exceeded"
    );
    let binding = (
        &resources.installation,
        &record.reference,
        record.compiler_version.as_str(),
        &dependencies,
    );
    let sealed: SealedSecret = serde_json::from_value(record.payload)?;
    let oauth = record.compiler_version == "tenancy-oauth-resource-v1";
    let purpose = if oauth {
        "owned-oauth-seed-v1"
    } else {
        "resource"
    };
    let bytes = key.open_bound(purpose, &binding, &sealed)?;
    let artifact: Artifact = serde_json::from_slice(&bytes)?;
    ensure!(
        oauth
            == matches!(
                &artifact.definition,
                super::manifest::Definition::Credential { oauth: Some(_), .. }
            ),
        "OAuth resource purpose mismatch"
    );
    ensure!(
        artifact.reference == record.reference
            && artifact.definition.dependencies() == dependencies,
        "resource identity mismatch"
    );
    Import {
        reference: artifact.reference.clone(),
        definition: artifact.definition.clone(),
    }
    .validate()?;
    let fingerprint = key.fingerprint(
        "tenancy-resource-v1",
        "immutable-definition",
        &bounded_bytes(&(&binding, &artifact), 2 * MAX_ARTIFACT_BYTES)?,
    )?;
    ensure!(
        fingerprint == record.content_digest.as_str(),
        "resource fingerprint mismatch"
    );
    Ok(artifact)
}
