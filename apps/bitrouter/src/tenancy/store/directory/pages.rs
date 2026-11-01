//! Complete keyset walks with a released working set after each page.

use super::*;
use crate::tenancy::model::AuthorityClass;
use anyhow::Context;
use bitrouter_ai::tenancy::identity::Digest;
use sea_orm::sea_query::Order;

async fn page<T>(
    db: &impl ConnectionTrait,
    table: Table,
    after: Option<&str>,
    budget: PageBudget,
    decode: fn(&sea_orm::QueryResult) -> Result<T>,
) -> Result<Vec<T>> {
    let mut query = select(db, table, budget.row_bytes()?)?;
    if let Some(after) = after {
        query.and_where(Expr::col((Alias::new(table.name()), Alias::new("id"))).gt(after));
    }
    // Qualify the source column: ordering by the bounded projection's alias
    // would sort a CASE expression instead of walking the primary-key index.
    query
        .order_by((Alias::new(table.name()), Alias::new("id")), Order::Asc)
        .limit(u64::try_from(budget.rows)?);
    query::read(db, query, decode).await
}

pub(super) async fn tenant_page_in(
    db: &impl ConnectionTrait,
    after: Option<&TenantId>,
    budget: PageBudget,
) -> Result<Vec<Tenant>> {
    page(
        db,
        Table::Tenant,
        after.map(TenantId::as_str),
        budget,
        records::tenant_from,
    )
    .await
}

pub(super) async fn principal_page_in(
    db: &impl ConnectionTrait,
    after: Option<&PrincipalId>,
    budget: PageBudget,
) -> Result<Vec<Principal>> {
    page(
        db,
        Table::Principal,
        after.map(PrincipalId::as_str),
        budget,
        records::principal_from,
    )
    .await
}

pub(super) async fn credential_page_in(
    db: &impl ConnectionTrait,
    after: Option<&CredentialId>,
    budget: PageBudget,
) -> Result<Vec<Credential>> {
    page(
        db,
        Table::Credential,
        after.map(CredentialId::as_str),
        budget,
        records::credential_from,
    )
    .await
}

pub(super) async fn binding_page_in(
    db: &impl ConnectionTrait,
    after: Option<&(TenantId, BindingId)>,
    budget: PageBudget,
) -> Result<Vec<Binding>> {
    let mut query = select(db, Table::Binding, budget.row_bytes()?)?;
    if let Some((tenant, binding)) = after {
        // Seek the composite index directly instead of scanning prior tenants.
        // https://sqlite.org/rowvalue.html#scrolling_window_queries
        query.and_where(
            Expr::tuple([
                Expr::col((Alias::new(Table::Binding.name()), Alias::new("tenant_id"))).into(),
                Expr::col((Alias::new(Table::Binding.name()), Alias::new("id"))).into(),
            ])
            .gt(Expr::tuple([
                Expr::val(tenant.as_str()).into(),
                Expr::val(binding.as_str()).into(),
            ])),
        );
    }
    query
        .order_by(
            (Alias::new(Table::Binding.name()), Alias::new("tenant_id")),
            Order::Asc,
        )
        .order_by(
            (Alias::new(Table::Binding.name()), Alias::new("id")),
            Order::Asc,
        )
        .limit(u64::try_from(budget.rows)?);
    query::read(db, query, records::binding_from).await
}

pub(super) async fn validate_directory_in(
    db: &impl ConnectionTrait,
    budget: PageBudget,
) -> Result<DirectoryTotals> {
    let bytes = budget.row_bytes()?;
    let mut totals = DirectoryTotals::default();
    let mut progress = std::time::Instant::now();
    let mut after = None;
    loop {
        let page = tenant_page_in(db, after.as_ref(), budget).await?;
        if page.is_empty() {
            break;
        }
        for tenant in page {
            chrono::DateTime::parse_from_rfc3339(&tenant.created_at)?;
            totals.tenants = totals
                .tenants
                .checked_add(1)
                .context("directory total overflow")?;
            after = Some(tenant.id);
        }
        if progress.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::info!(
                phase = "tenants",
                validated_rows = totals.tenants,
                outcome = "in_progress",
                "tenant directory validation progress"
            );
            progress = std::time::Instant::now();
        }
    }
    let mut after = None;
    loop {
        let page = principal_page_in(db, after.as_ref(), budget).await?;
        if page.is_empty() {
            break;
        }
        for principal in page {
            chrono::DateTime::parse_from_rfc3339(&principal.created_at)?;
            let exact = principal_by_identity_in(db, &principal.issuer, &principal.subject, bytes)
                .await?
                .context("principal identity index disagrees with directory")?;
            ensure!(exact.id == principal.id, "principal identity is not unique");
            totals.principals = totals
                .principals
                .checked_add(1)
                .context("directory total overflow")?;
            after = Some(principal.id);
        }
        if progress.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::info!(
                phase = "principals",
                validated_rows = totals.principals,
                outcome = "in_progress",
                "tenant directory validation progress"
            );
            progress = std::time::Instant::now();
        }
    }
    let mut after = None;
    loop {
        let page = binding_page_in(db, after.as_ref(), budget).await?;
        if page.is_empty() {
            break;
        }
        for binding in page {
            ensure!(
                tenant_in(db, &binding.tenant, bytes).await?.is_some()
                    && principal_in(db, &binding.principal, bytes).await?.is_some(),
                "orphan authority binding"
            );
            if let Some(owner) = &binding.managed_by
                && owner.class == AuthorityClass::Tenant
            {
                ensure!(
                    principal_in(db, &owner.principal, bytes).await?.is_some(),
                    "orphan binding manager"
                );
            }
            totals.bindings = totals
                .bindings
                .checked_add(1)
                .context("directory total overflow")?;
            after = Some((binding.tenant, binding.id));
        }
        if progress.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::info!(
                phase = "bindings",
                validated_rows = totals.bindings,
                outcome = "in_progress",
                "tenant directory validation progress"
            );
            progress = std::time::Instant::now();
        }
    }
    let mut after = None;
    loop {
        let page = credential_page_in(db, after.as_ref(), budget).await?;
        if page.is_empty() {
            break;
        }
        for credential in page {
            chrono::DateTime::parse_from_rfc3339(&credential.created_at)?;
            let _: Digest = credential.key_hash.clone().try_into()?;
            ensure!(
                tenant_in(db, &credential.tenant, bytes).await?.is_some()
                    && principal_in(db, &credential.principal, bytes)
                        .await?
                        .is_some(),
                "orphan authority credential"
            );
            let exact = credential_by_hash_in(db, &credential.key_hash, bytes)
                .await?
                .context("credential digest index disagrees with directory")?;
            ensure!(exact.id == credential.id, "credential digest is not unique");
            ensure!(
                credential.permissions.len() <= 10
                    && credential
                        .permissions
                        .iter()
                        .enumerate()
                        .all(|(index, permission)| !credential.permissions[..index]
                            .contains(permission)),
                "invalid credential permissions"
            );
            if credential.audit_reader {
                ensure!(
                    credential.binding.is_none() && credential.permissions.is_empty(),
                    "invalid audit-reader grant"
                );
            } else {
                let id = credential
                    .binding
                    .as_ref()
                    .context("credential binding missing")?;
                let binding = binding_in(db, &credential.tenant, id, bytes)
                    .await?
                    .context("orphan credential binding")?;
                // A binding can legitimately be reassigned after issuance. Its
                // current principal mismatch denies use; it is not corruption.
                ensure!(
                    !credential.permissions.is_empty() && binding.tenant == credential.tenant,
                    "invalid credential grant"
                );
            }
            totals.credentials = totals
                .credentials
                .checked_add(1)
                .context("directory total overflow")?;
            after = Some(credential.id);
        }
        if progress.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::info!(
                phase = "credentials",
                validated_rows = totals.credentials,
                outcome = "in_progress",
                "tenant directory validation progress"
            );
            progress = std::time::Instant::now();
        }
    }
    Ok(totals)
}
