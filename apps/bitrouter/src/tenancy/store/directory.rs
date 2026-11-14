//! Indexed directory reads in the caller's connection/transaction custody.
//!
//! Page bounds constrain one read, never directory cardinality. Callers reserve
//! decode/view memory and own deadlines, cleanup and publication; no loader opens
//! a transaction or treats these unregistered facts as current authorization.

mod facts;
pub(in crate::tenancy) mod pages;
pub(super) mod query;

use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::{BindingId, CredentialId, PrincipalId, TenantId},
};
use sea_orm::{
    ConnectionTrait,
    sea_query::{Alias, Expr},
};

use super::{Binding, Credential, Principal, Tenant, records};
use query::{Table, one, select};

/// A retryable owner-budget refusal, not malformed storage or an instance fence.
/// Callers can use `anyhow::Error::is::<CapacityExceeded>()` to map this to the
/// existing CapacityExceeded response while preserving transaction cleanup.
#[derive(Debug, thiserror::Error)]
#[error("authority directory working capacity exceeded")]
pub(in crate::tenancy) struct CapacityExceeded;

// Lifecycle reads have fixed SQL cell bounds, not Credential-sized payloads.
// Reserve one sequential driver/decode window plus map growth and the actual
// identifier bytes to be cloned. Existing payloads remain in caller custody;
// their slots are included here to cover rehash overlap without cloning them.
// The same conservative insert-only hash allowance is used by live views.
fn lifecycle_budget(
    snapshot: &crate::tenancy::model::Snapshot,
    loaded: usize,
    references: &[ResourceRef],
    bytes: usize,
) -> Result<()> {
    if references.is_empty() {
        return Ok(());
    }
    let count = snapshot
        .resource_lifecycle
        .records()
        .count()
        .checked_add(snapshot.resource_lifecycle.absences().count())
        .and_then(|count| count.checked_add(loaded))
        .and_then(|count| count.checked_add(references.len()))
        .and_then(|count| count.checked_add(1))
        .ok_or(CapacityExceeded)?;
    let slots = count
        .checked_next_power_of_two()
        .and_then(|slots| slots.checked_mul(4))
        .ok_or(CapacityExceeded)?;
    let slot_bytes =
        std::mem::size_of::<(ResourceRef, crate::tenancy::resources::lifecycle::Record)>()
            + std::mem::size_of::<ResourceRef>()
            + std::mem::size_of::<(Lookup, Presence)>()
            + 4 * std::mem::size_of::<usize>();
    // Keep the existing 16x bounded-row decode margin and two-row allowance
    // once for sequential point reads, not once per retained dependency.
    let window = 2 * 16 * facts::LIFECYCLE_ROW_BYTES;
    let initial = slots
        .checked_mul(slot_bytes)
        .and_then(|bytes| bytes.checked_add(window))
        .ok_or(CapacityExceeded)?;
    let required = references.iter().try_fold(initial, |total, reference| {
        let identifiers = reference.owner_id.as_str().len()
            + reference.resource_id.as_str().len()
            + reference.revision_id.as_str().len();
        // Lookup/key clones use the supplied IDs. Decoded records retain at
        // most the SQL-bounded row payload, including on a foreign-row error.
        identifiers
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(2 * facts::LIFECYCLE_ROW_BYTES))
            .and_then(|bytes| total.checked_add(bytes))
            .ok_or(CapacityExceeded)
    })?;
    if required > bytes || bytes > isize::MAX as usize {
        return Err(CapacityExceeded.into());
    }
    Ok(())
}

/// Limits for one page. `bytes` bounds encoded cells, not decoded object/RSS size.
/// Divide this budget evenly across rows so even corrupt cells cannot cause an
/// unbounded driver allocation. A large row may require a smaller page.
#[derive(Clone, Copy)]
pub(in crate::tenancy) struct PageBudget {
    pub rows: usize,
    pub bytes: usize,
}

impl PageBudget {
    fn row_bytes(self) -> Result<usize> {
        if self.rows == 0
            || self.bytes == 0
            || self.bytes > isize::MAX as usize
            || !self
                .rows
                .checked_mul(std::mem::size_of::<sea_orm::QueryResult>())
                .is_some_and(|bytes| bytes <= self.bytes)
        {
            return Err(CapacityExceeded.into());
        }
        Ok(self.bytes / self.rows)
    }
}

/// Point-read a tenant, with a finite encoded-row allocation budget.
pub(in crate::tenancy) async fn tenant_in(
    db: &impl ConnectionTrait,
    id: &TenantId,
    bytes: usize,
) -> Result<Option<Tenant>> {
    let mut query = select(db, Table::Tenant, bytes)?;
    query.and_where(Expr::col(Alias::new("id")).eq(id.as_str()));
    one(db, query, records::tenant_from).await
}

/// Point-read a principal by its stable ID.
pub(in crate::tenancy) async fn principal_in(
    db: &impl ConnectionTrait,
    id: &PrincipalId,
    bytes: usize,
) -> Result<Option<Principal>> {
    let mut query = select(db, Table::Principal, bytes)?;
    query.and_where(Expr::col(Alias::new("id")).eq(id.as_str()));
    one(db, query, records::principal_from).await
}

/// Use the byte-exact unique issuer/subject index, including case and whitespace.
pub(in crate::tenancy) async fn principal_by_identity_in(
    db: &impl ConnectionTrait,
    issuer: &str,
    subject: &str,
    bytes: usize,
) -> Result<Option<Principal>> {
    ensure!(
        !issuer.is_empty()
            && issuer.chars().count() <= 256
            && !subject.is_empty()
            && subject.chars().count() <= 256,
        "invalid principal identity"
    );
    let mut query = select(db, Table::Principal, bytes)?;
    query
        .and_where(Expr::col(Alias::new("issuer")).eq(issuer))
        .and_where(Expr::col(Alias::new("subject")).eq(subject));
    one(db, query, records::principal_from).await
}

/// Resolve the composite tenant/binding primary key.
pub(in crate::tenancy) async fn binding_in(
    db: &impl ConnectionTrait,
    tenant: &TenantId,
    id: &BindingId,
    bytes: usize,
) -> Result<Option<Binding>> {
    let mut query = select(db, Table::Binding, bytes)?;
    query
        .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
        .and_where(Expr::col(Alias::new("id")).eq(id.as_str()));
    one(db, query, records::binding_from).await
}

/// Read any credential state, including expired/revoked history for management.
pub(in crate::tenancy) async fn credential_in(
    db: &impl ConnectionTrait,
    id: &CredentialId,
    bytes: usize,
) -> Result<Option<Credential>> {
    let mut query = select(db, Table::Credential, bytes)?;
    query.and_where(Expr::col(Alias::new("id")).eq(id.as_str()));
    one(db, query, records::credential_from).await
}

/// Resolve an already-hashed token through the unique digest index. Never pass a token.
pub(in crate::tenancy) async fn credential_by_hash_in(
    db: &impl ConnectionTrait,
    hash: &str,
    bytes: usize,
) -> Result<Option<Credential>> {
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid credential digest"
    );
    let mut query = select(db, Table::Credential, bytes)?;
    query.and_where(Expr::col(Alias::new("key_hash")).eq(hash));
    one(db, query, records::credential_from).await
}

/// Load one tenant page after an exclusive stable-ID cursor.
pub(in crate::tenancy) async fn tenant_page_in(
    db: &impl ConnectionTrait,
    after: Option<&TenantId>,
    budget: PageBudget,
) -> Result<Vec<Tenant>> {
    pages::tenant_page_in(db, after, budget).await
}

/// Load one principal page after an exclusive stable-ID cursor.
pub(in crate::tenancy) async fn principal_page_in(
    db: &impl ConnectionTrait,
    after: Option<&PrincipalId>,
    budget: PageBudget,
) -> Result<Vec<Principal>> {
    pages::principal_page_in(db, after, budget).await
}

/// Load one binding page ordered by the full `(tenant_id, id)` primary key.
pub(in crate::tenancy) async fn binding_page_in(
    db: &impl ConnectionTrait,
    after: Option<&(TenantId, BindingId)>,
    budget: PageBudget,
) -> Result<Vec<Binding>> {
    pages::binding_page_in(db, after, budget).await
}

/// Page all credential history; expired and revoked rows must also be validated.
pub(in crate::tenancy) async fn credential_page_in(
    db: &impl ConnectionTrait,
    after: Option<&CredentialId>,
    budget: PageBudget,
) -> Result<Vec<Credential>> {
    pages::credential_page_in(db, after, budget).await
}

/// Primary rows validated in one fixed snapshot; never a live directory ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tenancy) struct DirectoryTotals {
    pub tenants: u64,
    pub principals: u64,
    pub bindings: u64,
    pub credentials: u64,
}

/// Validate all four directory tables without retaining a directory snapshot.
/// This is only the directory portion of startup validation: the caller must
/// validate installation, operation, configuration and lifecycle state separately.
/// Run under exclusive startup ownership or a fixed read transaction.
pub(in crate::tenancy) async fn validate_directory_in(
    db: &impl ConnectionTrait,
    budget: PageBudget,
) -> Result<DirectoryTotals> {
    pages::validate_directory_in(db, budget).await
}

/// Explicit fact keys; no missing entry in the sparse snapshot implies absence.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::tenancy) enum Lookup {
    Tenant(TenantId),
    Principal(PrincipalId),
    PrincipalIdentity(String, String),
    Binding(TenantId, BindingId),
    Credential(CredentialId),
    ActiveConfiguration(TenantId),
    Resource(bitrouter_ai::tenancy::configuration::ResourceRef),
    Head(bitrouter_ai::tenancy::authority::ResourceKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tenancy) enum Presence {
    NotLoaded,
    Absent,
    Present,
}

/// Temporary command-local facts. No directory counts or inherited directory maps.
/// Resource dependencies discovered by compilation must be loaded separately.
pub(in crate::tenancy) struct CommandFacts {
    /// Raw exact records, including credential history. A positive lookup is
    /// not an active grant: credential state and expiry must still be checked.
    pub snapshot: crate::tenancy::model::Snapshot,
    pub loaded: std::collections::HashMap<Lookup, Presence>,
}

impl CommandFacts {
    pub(in crate::tenancy) fn presence(&self, lookup: &Lookup) -> Presence {
        self.loaded
            .get(lookup)
            .copied()
            .unwrap_or(Presence::NotLoaded)
    }
}

/// Load only the caller's exact credential/tenant/principal/binding closure.
/// Installation identities require no directory row. `bytes` is the aggregate
/// working reservation for this load, including the conservative decode allowance.
pub(in crate::tenancy) async fn load_caller_in(
    db: &impl ConnectionTrait,
    header: &crate::tenancy::model::AuthorityHeader,
    caller: &crate::tenancy::model::Caller,
    bytes: usize,
) -> Result<CommandFacts> {
    facts::load_caller_in(db, header, caller, bytes).await
}

/// Load the caller and an exact target tenant for subsequent authorization.
/// `include_active` also loads the active pointer and validates its creation
/// metadata. `bytes` reserves all temporary facts: at most 7 reads, or 9 with
/// active metadata (installation callers need at most 1 or 3 respectively).
pub(in crate::tenancy) async fn load_tenant_in(
    db: &impl ConnectionTrait,
    header: &crate::tenancy::model::AuthorityHeader,
    caller: &crate::tenancy::model::Caller,
    tenant: &TenantId,
    include_active: bool,
    bytes: usize,
) -> Result<CommandFacts> {
    facts::load_tenant_in(db, header, caller, tenant, include_active, bytes).await
}

/// Extend the caller with exact command targets and principal-identity uniqueness.
/// `bytes` reserves aggregate working memory, as in `load_caller_in`.
/// This is preparation only; authorization/CAS checks still run in their existing
/// order, and no lookup error is converted to a proven absent fact.
pub(in crate::tenancy) async fn load_command_in(
    db: &impl ConnectionTrait,
    header: &crate::tenancy::model::AuthorityHeader,
    caller: &crate::tenancy::model::Caller,
    command: &crate::tenancy::command::Command,
    bytes: usize,
) -> Result<CommandFacts> {
    facts::load_command_in(db, header, caller, command, bytes).await
}

/// Extend already-scoped facts with a caller-budgeted compile dependency slice.
/// Call this before any synchronous lifecycle predicate; absence is recorded only
/// after a successful exact lookup. No dependency is assumed unrevoked on a miss.
/// `bytes` is additional reserved working memory, alongside the existing facts.
pub(in crate::tenancy) async fn load_dependencies_in(
    db: &impl ConnectionTrait,
    facts: &mut CommandFacts,
    references: &[bitrouter_ai::tenancy::configuration::ResourceRef],
    bytes: usize,
) -> Result<()> {
    lifecycle_budget(&facts.snapshot, facts.loaded.len(), references, bytes)?;
    for reference in references {
        facts::resource_in(db, facts, reference).await?;
    }
    Ok(())
}

/// Extend a retained snapshot with exact dependencies without copying its maps.
/// `bytes` is an additional reservation alongside the existing snapshot. The
/// caller keeps connection/revision custody and discards partial work on error.
pub(in crate::tenancy) async fn extend_dependencies_in(
    db: &impl ConnectionTrait,
    snapshot: &mut crate::tenancy::model::Snapshot,
    references: &[bitrouter_ai::tenancy::configuration::ResourceRef],
    bytes: usize,
) -> Result<()> {
    lifecycle_budget(snapshot, 0, references, bytes)?;
    for reference in references {
        facts::snapshot_resource_in(db, snapshot, reference).await?;
    }
    Ok(())
}
