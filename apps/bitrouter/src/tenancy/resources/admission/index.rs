//! Permanent, byte-exact witnesses into the authenticated immutable store.
//!
//! Resource witnesses distinguish policy/profile keys and remember route
//! protection. Health witnesses preserve rules after every member is revoked.
//! A witness is never replaced with a later (possibly more permissive) revision.

use super::*;
use sea_orm::{
    Value,
    sea_query::{Alias, Cond, Expr, Func, OnConflict, Query},
};

const TABLE: &str = "tenancy_admission_index";
const COLUMNS: [&str; 5] = [
    "index_kind",
    "owner_kind",
    "owner_id",
    "resource_kind",
    "resource_id",
];

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize)]
pub(in crate::tenancy) struct Key([String; 5]);

#[derive(Clone, serde::Serialize)]
pub(in crate::tenancy) struct Entry {
    key: Key,
    reference: ResourceRef,
}

fn enum_text(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .context("admission index enum")
}

impl Key {
    pub(super) fn resource(key: &ResourceKey) -> Result<Self> {
        Ok(Self([
            "resource".into(),
            enum_text(&key.owner_kind)?,
            key.owner_id.as_str().into(),
            enum_text(&key.resource_kind)?,
            key.resource_id.as_str().into(),
        ]))
    }

    pub(super) fn group(owner: &OwnerId, group: &ResourceId) -> Self {
        // This intentionally matches the established owner-id/group namespace,
        // including members in distinct owner kinds with the same owner id.
        Self([
            "health".into(),
            String::new(),
            owner.as_str().into(),
            String::new(),
            group.as_str().into(),
        ])
    }
}

pub(in crate::tenancy::resources) fn entries(
    reference: &ResourceRef,
    definition: &Definition,
) -> Result<Vec<Entry>> {
    let relevant = definition.is_profiled() || matches!(definition, Definition::Policy { .. });
    if !relevant {
        return Ok(Vec::new());
    }
    let mut entries = vec![Entry {
        key: Key::resource(&reference.key())?,
        reference: reference.clone(),
    }];
    if let Definition::AdmissionProfileV2 { profile } = definition
        && let Some(group) = &profile.health_group_id
    {
        entries.push(Entry {
            key: Key::group(&reference.owner_id, group),
            reference: reference.clone(),
        });
    }
    Ok(entries)
}

fn predicate(key: &Key) -> Cond {
    COLUMNS
        .iter()
        .zip(&key.0)
        .fold(Cond::all(), |cond, (column, value)| {
            cond.add(Expr::col(Alias::new(*column)).eq(value.as_str()))
        })
}

/// Bounded SQL projection before driver allocation, including embedded NULs.
pub(super) fn bounded(
    db: &impl ConnectionTrait,
    column: &str,
    bytes: u64,
) -> sea_orm::sea_query::SimpleExpr {
    let value = Expr::col(Alias::new(column));
    let length = if db.get_database_backend() == sea_orm::DatabaseBackend::Sqlite {
        Func::cust(Alias::new("length")).arg(value.clone().cast_as(Alias::new("BLOB")))
    } else {
        Func::cust(Alias::new("octet_length")).arg(value.clone())
    };
    Expr::case(Expr::expr(length).lte(bytes), value)
        .finally(Expr::val(Option::<String>::None))
        .into()
}

/// Batch exact primary-key probes; query and result size follow the caller's keys.
pub(super) async fn lookup(db: &impl ConnectionTrait, keys: &[Key]) -> Result<Vec<Entry>> {
    let mut result = Vec::new();
    for batch in keys.chunks(32) {
        let mut query = Query::select();
        query.from(Alias::new(TABLE)).cond_where(
            batch
                .iter()
                .fold(Cond::any(), |c, key| c.add(predicate(key))),
        );
        for (name, limit) in COLUMNS
            .into_iter()
            .zip([32, 32, 128, 32, 128])
            .chain([("reference_json", 1024)])
        {
            query.expr_as(bounded(db, name, limit), Alias::new(name));
        }
        query.limit(u64::try_from(batch.len())? + 1);
        let rows = db
            .query_all(db.get_database_backend().build(&query))
            .await?;
        ensure!(rows.len() <= batch.len(), "duplicate admission witness");
        for row in rows {
            let mut values = Vec::new();
            for name in COLUMNS {
                values.push(row.try_get::<String>("", name)?);
            }
            let key = Key(values
                .try_into()
                .map_err(|_| anyhow::anyhow!("admission key width"))?);
            ensure!(batch.contains(&key), "foreign admission witness");
            let reference: ResourceRef =
                serde_json::from_str(&row.try_get::<String>("", "reference_json")?)?;
            result.push(Entry { key, reference });
        }
    }
    Ok(result)
}

/// Insert only; the caller owns validation and the resource/operation transaction.
pub(in crate::tenancy) async fn write(db: &impl ConnectionTrait, entry: &Entry) -> Result<()> {
    let mut insert = Query::insert();
    let mut values: Vec<Value> = entry.key.0.iter().map(|s| s.as_str().into()).collect();
    values.push(serde_json::to_string(&entry.reference)?.into());
    insert
        .into_table(Alias::new(TABLE))
        .columns(
            COLUMNS
                .into_iter()
                .chain(["reference_json"])
                .map(Alias::new),
        )
        .values(values.into_iter().map(Into::into))?
        .on_conflict(
            OnConflict::columns(COLUMNS.map(Alias::new))
                .do_nothing()
                .to_owned(),
        );
    db.execute(db.get_database_backend().build(&insert)).await?;
    Ok(())
}

impl Resources {
    /// Load one consistent request closure, never the unrelated retained history.
    /// The caller holds the actor's fixed read transaction and publication stamp.
    pub(in crate::tenancy) async fn admission_in(
        &self,
        db: &impl ConnectionTrait,
        authority: bitrouter_ai::tenancy::identity::AuthorityRevision,
        references: &[ResourceRef],
        keys: &[ResourceKey],
        groups: &[(OwnerId, ResourceId)],
    ) -> Result<State> {
        let mut state = State::default();
        let references: Vec<_> = references
            .iter()
            .filter(|reference| {
                matches!(
                    reference.resource_kind,
                    ResourceKind::Policy | ResourceKind::Route
                )
            })
            .cloned()
            .collect();
        state.requested.extend(references.iter().cloned());
        state.loaded_keys.extend(
            keys.iter()
                .filter(|key| {
                    matches!(
                        key.resource_kind,
                        ResourceKind::Policy | ResourceKind::Route
                    )
                })
                .cloned(),
        );
        state
            .loaded_keys
            .extend(references.iter().map(ResourceRef::key));
        let mut query_keys: Vec<_> = state
            .loaded_keys
            .iter()
            .map(Key::resource)
            .collect::<Result<_>>()?;
        query_keys.extend(groups.iter().map(|(owner, group)| Key::group(owner, group)));
        let mut unique = HashSet::new();
        query_keys.retain(|key| unique.insert(key.clone()));
        let entries = lookup(db, &query_keys).await?;
        let mut pending: HashSet<_> = references.iter().cloned().collect();
        for entry in entries {
            let artifact = self
                .load(db, &entry.reference)
                .await
                .map_err(|_| anyhow::anyhow!("admission witness unavailable"))?;
            ensure!(
                self::entries(&artifact.reference, &artifact.definition)?
                    .iter()
                    .any(|item| item.key == entry.key),
                "admission witness identity mismatch"
            );
            state.insert(&artifact)?;
            pending.remove(&artifact.reference);
        }
        // Pins and heads authenticate independently: a valid historical pin must
        // never confer the current head's authority on an absent or revoked pin.
        for reference in pending {
            match self.load(db, &reference).await {
                Ok(artifact) => state.insert(&artifact)?,
                Err(ErrorCode::NotFound) => {}
                Err(_) => anyhow::bail!("admission pinned resource unavailable"),
            }
        }
        let profiles: Vec<_> = state.profiles.keys().cloned().collect();
        let mut heads = HashSet::new();
        for reference in profiles {
            let record = lifecycle::exact(db, &self.installation, &reference)
                .await?
                .context("admission profile has no managed lifecycle")?;
            ensure!(
                record.authority_revision <= authority,
                "admission lifecycle exceeds authority"
            );
            state.apply_lifecycle(&record);
            if !heads.insert(reference.key()) {
                continue;
            }
            let head = lifecycle::head(db, &self.installation, &reference.key())
                .await?
                .context("admission profile has no managed head")?;
            ensure!(
                head.authority_revision <= authority,
                "admission head exceeds authority"
            );
            let artifact = self
                .load(db, &head.reference)
                .await
                .map_err(|_| anyhow::anyhow!("admission head unavailable"))?;
            ensure!(
                artifact.definition.is_admission_profile(),
                "admission profile head changes type"
            );
            state.insert(&artifact)?;
            state.apply_lifecycle(&head);
        }
        Ok(state)
    }
}

/// Complete startup checks existence as well as authenticity. An erased witness
/// must not turn a retained profiled route into a never-protected key.
pub(in crate::tenancy::resources) async fn verify(
    resources: &Resources,
    db: &impl ConnectionTrait,
    artifact: &Artifact,
) -> Result<()> {
    let expected = entries(&artifact.reference, &artifact.definition)?;
    if expected.is_empty() {
        return Ok(());
    }
    let keys: Vec<_> = expected.iter().map(|entry| entry.key.clone()).collect();
    let actual = lookup(db, &keys).await?;
    ensure!(
        actual.len() == expected.len(),
        "retained admission witness missing"
    );
    let groups: Vec<_> = match &artifact.definition {
        Definition::AdmissionProfileV2 { profile } => profile
            .health_group_id
            .as_ref()
            .map(|group| (artifact.reference.owner_id.clone(), group.clone()))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    let header = crate::tenancy::store::startup::read_header_in(db, 4096)
        .await?
        .context("admission authority missing")?
        .header;
    let references = if artifact.definition.is_admission_profile() {
        std::slice::from_ref(&artifact.reference)
    } else {
        &[]
    };
    let mut state = resources
        .admission_in(
            db,
            header.revision,
            references,
            &[artifact.reference.key()],
            &groups,
        )
        .await?;
    state.insert(artifact)
}

/// Validate this definition against permanent witnesses before retaining it.
pub(in crate::tenancy::resources) async fn retain(
    resources: &Resources,
    db: &impl ConnectionTrait,
    artifact: &Artifact,
) -> Result<()> {
    if entries(&artifact.reference, &artifact.definition)?.is_empty() {
        return Ok(());
    }
    let groups = match &artifact.definition {
        Definition::AdmissionProfileV2 { profile } => profile
            .health_group_id
            .as_ref()
            .map(|group| (artifact.reference.owner_id.clone(), group.clone()))
            .into_iter()
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let header = crate::tenancy::store::startup::read_header_in(db, 4096)
        .await?
        .context("admission authority missing")?
        .header;
    let mut state = resources
        .admission_in(
            db,
            header.revision,
            &[],
            &[artifact.reference.key()],
            &groups,
        )
        .await?;
    state.insert(artifact)?;
    for entry in entries(&artifact.reference, &artifact.definition)? {
        write(db, &entry).await?;
    }
    Ok(())
}
