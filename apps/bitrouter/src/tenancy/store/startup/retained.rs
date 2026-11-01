use super::*;
use crate::tenancy::{configuration_store, resources::lifecycle};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::{AuthorityRevision, RevisionId, TenantId},
};
use sea_orm::{
    DatabaseBackend,
    sea_query::{ExprTrait, Func, Query, SimpleExpr},
};

pub(super) mod memory;

const RESOURCE_KEYS: [&str; 5] = [
    "owner_kind",
    "owner_id",
    "resource_kind",
    "resource_id",
    "revision_id",
];
const CONFIGURATION_KEYS: [&str; 2] = ["tenant_id", "revision_id"];

fn reference(row: &QueryResult) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: super::super::enum_from(row, "owner_kind")?,
        owner_id: super::super::string(row, "owner_id")?.try_into()?,
        resource_kind: super::super::enum_from(row, "resource_kind")?,
        resource_id: super::super::string(row, "resource_id")?.try_into()?,
        revision_id: super::super::string(row, "revision_id")?.try_into()?,
    })
}

fn resource_cursor(reference: &ResourceRef) -> Result<Vec<String>> {
    Ok(vec![
        super::super::enum_text(&reference.owner_kind)?,
        reference.owner_id.to_string(),
        super::super::enum_text(&reference.resource_kind)?,
        reference.resource_id.to_string(),
        reference.revision_id.to_string(),
    ])
}

fn configuration_key(row: &QueryResult) -> Result<(TenantId, RevisionId)> {
    Ok((
        super::super::string(row, "tenant_id")?.try_into()?,
        super::super::string(row, "revision_id")?.try_into()?,
    ))
}

fn active(row: &QueryResult) -> Result<configuration_store::StoredActiveConfiguration> {
    let (tenant, revision) = configuration_key(row)?;
    Ok(configuration_store::StoredActiveConfiguration {
        tenant,
        revision,
        authority_revision: authority(row)?,
    })
}

fn authority(row: &QueryResult) -> Result<AuthorityRevision> {
    Ok(u64::try_from(row.try_get::<i64>("", "authority_revision")?)?.try_into()?)
}

struct Lifecycle {
    installation: bitrouter_ai::tenancy::identity::InstallationId,
    record: lifecycle::Record,
}

fn status(row: &QueryResult) -> Result<Lifecycle> {
    Ok(Lifecycle {
        installation: super::super::string(row, "installation_id")?.try_into()?,
        record: lifecycle::Record {
            reference: reference(row)?,
            is_head: row.try_get("", "is_head")?,
            revoked: row.try_get("", "revoked")?,
            authority_revision: authority(row)?,
        },
    })
}

// Only qualified keys cross the page boundary. Body bytes never accumulate in
// a page and never inherit its per-row metadata allowance.
pub(super) fn key_page(
    db: &impl ConnectionTrait,
    table: Table,
    keys: &[&str],
    after: Option<&[String]>,
    budget: PageBudget,
) -> Result<SelectStatement> {
    let mut query = Query::select();
    query.from(Alias::new(table.name()));
    let mut bytes: SimpleExpr = Expr::val(0_i64).into();
    let mut valid: SimpleExpr = Expr::val(true).into();
    for key in keys {
        let column = Expr::col((Alias::new(table.name()), Alias::new(*key)));
        let length = if db.get_database_backend() == DatabaseBackend::Sqlite {
            Func::cust(Alias::new("length")).arg(column.clone().cast_as(Alias::new("BLOB")))
        } else {
            Func::cust(Alias::new("octet_length")).arg(column.clone())
        };
        let mut condition = Expr::expr(length.clone()).between(1_i64, 128_i64);
        if db.get_database_backend() == DatabaseBackend::Sqlite {
            condition = condition
                .and(Expr::expr(Func::cust(Alias::new("typeof")).arg(column.clone())).eq("text"));
        }
        valid = Expr::expr(valid).and(condition.clone());
        bytes = Expr::expr(bytes).add(length);
        query.expr_as(
            Expr::case(condition, column).finally(Expr::val(Option::<String>::None)),
            Alias::new(*key),
        );
    }
    query.expr_as(
        Expr::case(valid, 1_i32).finally(0_i32),
        Alias::new("directory_valid"),
    );
    query.expr_as(bytes, Alias::new("directory_bytes"));
    query.expr_as(Expr::val(1_i32), Alias::new("directory_fits"));
    order_page(query.to_owned(), table, keys, after, budget.rows)
}

async fn body(
    db: &impl ConnectionTrait,
    table: Table,
    keys: &[&str],
    values: &[String],
    bytes: usize,
) -> Result<()> {
    // SQL checks length/type before transferring any body. The first pass only
    // counts JSON nodes; its row and scratch are dropped before canonical decode.
    let mut selected = query::select(db, table, bytes / 6)?;
    for (key, value) in keys.iter().zip(values) {
        selected.and_where(Expr::col(Alias::new(*key)).eq(value.as_str()));
    }
    let decode: fn(&QueryResult) -> Result<usize> = match table {
        Table::Resource => memory::resource,
        Table::ConfigurationBody => memory::configuration,
        _ => anyhow::bail!("invalid immutable body table"),
    };
    let required = query::one(db, selected, decode)
        .await?
        .context("immutable body disappeared")?;
    if required > bytes {
        return Err(CapacityExceeded.into());
    }
    Ok(())
}

async fn resources(db: &impl ConnectionTrait, budget: PageBudget, body_bytes: usize) -> Result<()> {
    let mut after = None::<Vec<String>>;
    loop {
        let page = query::read(
            db,
            key_page(
                db,
                Table::Resource,
                &RESOURCE_KEYS,
                after.as_deref(),
                budget,
            )?,
            reference,
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for reference in page {
            let cursor = resource_cursor(&reference)?;
            body(db, Table::Resource, &RESOURCE_KEYS, &cursor, body_bytes).await?;
            // Fixed transaction custody binds preflight to canonical decode.
            configuration_store::resource(db, &reference)
                .await?
                .context("resource disappeared during validation")?;
            after = Some(cursor);
        }
    }
    Ok(())
}

async fn configurations(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    budget: PageBudget,
    body_bytes: usize,
) -> Result<()> {
    let mut after = None::<Vec<String>>;
    let bytes = budget.bytes / budget.rows;
    loop {
        let page = query::read(
            db,
            key_page(
                db,
                Table::ConfigurationBody,
                &CONFIGURATION_KEYS,
                after.as_deref(),
                budget,
            )?,
            configuration_key,
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for (tenant, revision) in page {
            let cursor = vec![tenant.to_string(), revision.to_string()];
            body(
                db,
                Table::ConfigurationBody,
                &CONFIGURATION_KEYS,
                &cursor,
                body_bytes,
            )
            .await?;
            let record = configuration_store::configuration(db, &tenant, &revision)
                .await?
                .context("configuration disappeared during validation")?;
            let created = record.authority_revision;
            drop(record);
            ensure!(
                created <= header.revision
                    && directory::tenant_in(db, &tenant, bytes).await?.is_some(),
                "configuration is outside published authority"
            );
            after = Some(cursor);
        }
    }
    let mut after = None::<Vec<String>>;
    loop {
        let page = query::read(
            db,
            page_query(db, Table::Active, &["tenant_id"], after.as_deref(), budget)?,
            active,
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for pointer in page {
            let mut query = query::select(db, Table::Configuration, bytes)?;
            query
                .and_where(Expr::col(Alias::new("tenant_id")).eq(pointer.tenant.as_str()))
                .and_where(Expr::col(Alias::new("revision_id")).eq(pointer.revision.as_str()));
            let created = query::one(db, query, active)
                .await?
                .context("active configuration target missing")?;
            ensure!(
                pointer.authority_revision <= header.revision
                    && created.authority_revision <= pointer.authority_revision,
                "active configuration authority mismatch"
            );
            after = Some(vec![pointer.tenant.to_string()]);
        }
    }
    Ok(())
}

async fn lifecycles(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    budget: PageBudget,
) -> Result<()> {
    let mut after = None::<Vec<String>>;
    let bytes = budget.bytes / budget.rows;
    loop {
        let page = query::read(
            db,
            page_query(
                db,
                Table::Lifecycle,
                &RESOURCE_KEYS,
                after.as_deref(),
                budget,
            )?,
            status,
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let record = row.record;
            ensure!(
                row.installation == header.installation
                    && record.authority_revision <= header.revision,
                "resource lifecycle installation or authority mismatch"
            );
            let cursor = resource_cursor(&record.reference)?;
            let mut resource = sea_orm::sea_query::Query::select();
            resource
                .expr(Expr::val(1_i32))
                .from(Alias::new(Table::Resource.name()))
                .limit(1);
            for (key, value) in RESOURCE_KEYS.iter().zip(&cursor) {
                resource.and_where(Expr::col(Alias::new(*key)).eq(value.as_str()));
            }
            ensure!(
                db.query_one(db.get_database_backend().build(&resource))
                    .await?
                    .is_some(),
                "resource lifecycle revision missing"
            );
            if record.is_head {
                let mut head = query::select(db, Table::Lifecycle, bytes)?;
                for (key, value) in RESOURCE_KEYS[..4].iter().zip(&cursor) {
                    head.and_where(Expr::col(Alias::new(*key)).eq(value.as_str()));
                }
                head.and_where(Expr::col(Alias::new("is_head")).eq(true));
                // Detect duplicate heads using the exact key without retaining a
                // set proportional to the number of resources in the directory.
                query::one(db, head, status)
                    .await?
                    .context("resource head disappeared")?;
            }
            after = Some(cursor);
        }
    }
    Ok(())
}

pub(super) async fn validate_in(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    budget: Budget,
) -> Result<()> {
    let page = budget.page()?;
    // Includes driver key rows, decoded references, cursor copies and metadata.
    let keys = budget
        .rows
        .checked_add(4)
        .and_then(|n| n.checked_mul(4096))
        .ok_or(CapacityExceeded)?;
    let body_bytes = budget.bytes.checked_sub(keys).ok_or(CapacityExceeded)?;
    resources(db, page, body_bytes).await?;
    configurations(db, header, page, body_bytes).await?;
    lifecycles(db, header, page).await
}
