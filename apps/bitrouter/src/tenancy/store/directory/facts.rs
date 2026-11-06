//! Exact caller and command read sets, with successful negative reads retained.

use super::*;
use crate::tenancy::{
    command::Command,
    model::{AuthorityClass, AuthorityHeader, Caller, Snapshot},
    resources::lifecycle,
};
use anyhow::Context;
use bitrouter_ai::tenancy::{
    configuration::ResourceRef, identity::AuthorityRevision, resources::Definition,
};

// The lifecycle projection contains four 128-byte IDs, two 32-byte enum fields
// and three integers. Its point/head helpers already bound those cells in SQL.
pub(super) const LIFECYCLE_ROW_BYTES: usize = 4 * 128 + 2 * 32 + 3 * 8;

fn after_header(bytes: usize, header: &AuthorityHeader) -> Result<usize> {
    let fixed = std::mem::size_of::<Snapshot>()
        .checked_add(header.created_at.len())
        .and_then(|n| n.checked_add(header.installation.as_str().len()))
        .ok_or(CapacityExceeded)?;
    bytes
        .checked_sub(fixed)
        .filter(|bytes| *bytes <= isize::MAX as usize)
        .ok_or_else(|| CapacityExceeded.into())
}
impl CommandFacts {
    fn mark(&mut self, lookup: Lookup, present: bool) {
        self.loaded.insert(
            lookup,
            if present {
                Presence::Present
            } else {
                Presence::Absent
            },
        );
    }

    async fn tenant(
        &mut self,
        db: &impl ConnectionTrait,
        id: &TenantId,
        bytes: usize,
    ) -> Result<()> {
        let lookup = Lookup::Tenant(id.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let record = tenant_in(db, id, bytes).await?;
        self.mark(lookup, record.is_some());
        if let Some(record) = record {
            self.snapshot.tenants.insert(record.id.clone(), record);
        }
        Ok(())
    }

    async fn principal(
        &mut self,
        db: &impl ConnectionTrait,
        id: &PrincipalId,
        bytes: usize,
    ) -> Result<()> {
        let lookup = Lookup::Principal(id.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let record = principal_in(db, id, bytes).await?;
        self.mark(lookup, record.is_some());
        if let Some(record) = record {
            self.snapshot.principals.insert(record.id.clone(), record);
        }
        Ok(())
    }

    async fn binding(
        &mut self,
        db: &impl ConnectionTrait,
        tenant: &TenantId,
        id: &BindingId,
        bytes: usize,
    ) -> Result<()> {
        let lookup = Lookup::Binding(tenant.clone(), id.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let record = binding_in(db, tenant, id, bytes).await?;
        self.tenant(db, tenant, bytes).await?;
        if let Some(record) = &record {
            self.principal(db, &record.principal, bytes).await?;
            if let Some(owner) = &record.managed_by
                && owner.class == AuthorityClass::Tenant
            {
                self.principal(db, &owner.principal, bytes).await?;
            }
        }
        self.mark(lookup, record.is_some());
        if let Some(record) = record {
            self.snapshot
                .bindings
                .insert((record.tenant.clone(), record.id.clone()), record);
        }
        Ok(())
    }

    async fn credential(
        &mut self,
        db: &impl ConnectionTrait,
        id: &CredentialId,
        bytes: usize,
    ) -> Result<()> {
        let lookup = Lookup::Credential(id.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let record = credential_in(db, id, bytes).await?;
        if let Some(record) = &record {
            self.tenant(db, &record.tenant, bytes).await?;
            self.principal(db, &record.principal, bytes).await?;
            if let Some(binding) = &record.binding {
                self.binding(db, &record.tenant, binding, bytes).await?;
            }
        }
        self.mark(lookup, record.is_some());
        if let Some(record) = record {
            self.snapshot
                .credential_hashes
                .insert(record.key_hash.clone(), record.id.clone());
            self.snapshot.credentials.insert(record.id.clone(), record);
        }
        Ok(())
    }

    async fn active(
        &mut self,
        db: &impl ConnectionTrait,
        tenant: &TenantId,
        bytes: usize,
    ) -> Result<()> {
        let lookup = Lookup::ActiveConfiguration(tenant.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let mut query = select(db, Table::Active, bytes)?;
        query.and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()));
        let record = one(db, query, active_from).await?;
        if let Some(record) = &record {
            ensure!(
                record.authority_revision <= self.snapshot.revision,
                "active configuration is outside published authority"
            );
            let mut query = select(db, Table::Configuration, bytes)?;
            query
                .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
                .and_where(Expr::col(Alias::new("revision_id")).eq(record.revision.as_str()));
            let created = one(db, query, active_from)
                .await?
                .context("active configuration target missing")?;
            ensure!(
                created.authority_revision <= record.authority_revision,
                "active pointer predates configuration creation"
            );
        }
        self.mark(lookup, record.is_some());
        if let Some(record) = record {
            self.snapshot
                .active_configurations
                .insert(tenant.clone(), record.revision);
        }
        Ok(())
    }
}

fn active_from(
    row: &sea_orm::QueryResult,
) -> Result<crate::tenancy::configuration_store::StoredActiveConfiguration> {
    Ok(
        crate::tenancy::configuration_store::StoredActiveConfiguration {
            tenant: row.try_get::<String>("", "tenant_id")?.try_into()?,
            revision: row.try_get::<String>("", "revision_id")?.try_into()?,
            authority_revision: AuthorityRevision::try_from(u64::try_from(
                row.try_get::<i64>("", "authority_revision")?,
            )?)?,
        },
    )
}

pub(super) async fn load_caller_in(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    caller: &Caller,
    bytes: usize,
) -> Result<CommandFacts> {
    let bytes = after_header(bytes, header)?;
    let row_bytes = if matches!(caller, Caller::Credential(_)) {
        working_row_bytes(bytes, 6)?
    } else {
        0
    };
    caller_with_row_budget(db, header, caller, row_bytes).await
}

pub(super) async fn load_tenant_in(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    caller: &Caller,
    tenant: &TenantId,
    include_active: bool,
    bytes: usize,
) -> Result<CommandFacts> {
    let reads = 1
        + 6 * usize::from(matches!(caller, Caller::Credential(_)))
        + 2 * usize::from(include_active);
    let row_bytes = working_row_bytes(after_header(bytes, header)?, reads)?;
    let mut facts = caller_with_row_budget(db, header, caller, row_bytes).await?;
    facts.tenant(db, tenant, row_bytes).await?;
    if include_active {
        facts.active(db, tenant, row_bytes).await?;
    }
    Ok(facts)
}

async fn caller_with_row_budget(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    caller: &Caller,
    row_bytes: usize,
) -> Result<CommandFacts> {
    let mut facts = CommandFacts {
        snapshot: Snapshot::from_header(header),
        loaded: Default::default(),
    };
    if let Caller::Credential(id) = caller {
        facts.credential(db, id, row_bytes).await?;
    }
    Ok(facts)
}

pub(super) async fn resource_in(
    db: &impl ConnectionTrait,
    facts: &mut CommandFacts,
    reference: &ResourceRef,
) -> Result<()> {
    let lookup = Lookup::Resource(reference.clone());
    if facts.presence(&lookup) != Presence::NotLoaded {
        return Ok(());
    }
    snapshot_resource_in(db, &mut facts.snapshot, reference).await?;
    facts.mark(
        lookup,
        facts
            .snapshot
            .resource_lifecycle
            .record(reference)
            .is_some(),
    );
    Ok(())
}

pub(super) async fn snapshot_resource_in(
    db: &impl ConnectionTrait,
    snapshot: &mut Snapshot,
    reference: &ResourceRef,
) -> Result<()> {
    if snapshot.resource_lifecycle.loaded(reference) {
        return Ok(());
    }
    let record = lifecycle::exact(db, &snapshot.installation, reference).await?;
    if let Some(record) = &record {
        ensure!(
            record.authority_revision <= snapshot.revision,
            "resource fact is outside published authority"
        );
    }
    if let Some(record) = record {
        snapshot.resource_lifecycle.insert(record);
    } else {
        snapshot.resource_lifecycle.absent(reference.clone());
    }
    Ok(())
}

pub(super) async fn load_command_in(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    caller: &Caller,
    command: &Command,
    bytes: usize,
) -> Result<CommandFacts> {
    command.validate()?;
    let resources = match command {
        Command::Configuration(_, input) => input
            .document
            .routes
            .len()
            .checked_add(usize::from(input.document.policy.is_some())),
        Command::Resource(_, input) => match &input.definition {
            Definition::Credential { .. } => Some(1),
            Definition::Route {
                policy,
                credentials,
                ..
            } => credentials
                .len()
                .checked_add(usize::from(policy.is_some()))
                .and_then(|n| n.checked_add(1)),
            Definition::Policy { dependencies, .. } => dependencies.len().checked_add(1),
        },
        Command::RevokeResource(..) => Some(1),
        _ => Some(0),
    }
    .ok_or(CapacityExceeded)?;
    let reads = 17usize.checked_add(resources).ok_or(CapacityExceeded)?;
    let row_bytes = working_row_bytes(after_header(bytes, header)?, reads)?;
    if resources > 0 && row_bytes < LIFECYCLE_ROW_BYTES {
        return Err(CapacityExceeded.into());
    }
    let mut facts = caller_with_row_budget(db, header, caller, row_bytes).await?;
    match command {
        Command::Principal(id, input) => {
            facts.principal(db, id, row_bytes).await?;
            let record =
                principal_by_identity_in(db, &input.issuer, &input.subject, row_bytes).await?;
            facts.mark(
                Lookup::PrincipalIdentity(input.issuer.clone(), input.subject.clone()),
                record.is_some(),
            );
            if let Some(record) = record {
                facts.mark(Lookup::Principal(record.id.clone()), true);
                facts.snapshot.principals.insert(record.id.clone(), record);
            }
        }
        Command::Tenant(tenant, _) => {
            facts.tenant(db, tenant, row_bytes).await?;
        }
        Command::Binding(tenant, id, input) => {
            facts.binding(db, tenant, id, row_bytes).await?;
            facts.principal(db, &input.principal_id, row_bytes).await?;
        }
        Command::Detach(tenant, id, _) => {
            facts.binding(db, tenant, id, row_bytes).await?;
        }
        Command::Issue(tenant, input) => {
            facts
                .binding(db, tenant, &input.binding_id, row_bytes)
                .await?;
            facts.principal(db, &input.principal_id, row_bytes).await?;
            if let Some(id) = &input.rotate_credential_id {
                facts.credential(db, id, row_bytes).await?;
            }
        }
        Command::AuditReader(tenant, input) => {
            facts.tenant(db, tenant, row_bytes).await?;
            facts.principal(db, &input.principal_id, row_bytes).await?;
        }
        Command::Revoke(tenant, id, _) => {
            facts.tenant(db, tenant, row_bytes).await?;
            facts.credential(db, id, row_bytes).await?;
        }
        Command::Activate(tenant, _) => {
            facts.tenant(db, tenant, row_bytes).await?;
            facts.active(db, tenant, row_bytes).await?;
        }
        Command::Configuration(tenant, input) => {
            facts.tenant(db, tenant, row_bytes).await?;
            for reference in &input.document.routes {
                resource_in(db, &mut facts, reference).await?;
            }
            if let Some(policy) = &input.document.policy {
                resource_in(db, &mut facts, policy).await?;
            }
        }
        Command::Resource(tenant, input) => {
            facts.tenant(db, tenant, row_bytes).await?;
            let record = lifecycle::head(db, &header.installation, &input.key).await?;
            if let Some(record) = &record {
                ensure!(
                    record.authority_revision <= header.revision,
                    "resource head is outside published authority"
                );
            }
            facts.mark(Lookup::Head(input.key.clone()), record.is_some());
            if let Some(record) = record {
                facts.mark(Lookup::Resource(record.reference.clone()), true);
                facts.snapshot.resource_lifecycle.insert(record);
            }
            match &input.definition {
                Definition::Credential { .. } => {}
                Definition::Route {
                    policy,
                    credentials,
                    ..
                } => {
                    if let Some(policy) = policy {
                        resource_in(db, &mut facts, policy).await?;
                    }
                    for binding in credentials {
                        resource_in(db, &mut facts, &binding.credential).await?;
                    }
                }
                Definition::Policy { dependencies, .. } => {
                    for reference in dependencies {
                        resource_in(db, &mut facts, reference).await?;
                    }
                }
            }
        }
        Command::RevokeResource(tenant, input) => {
            facts.tenant(db, tenant, row_bytes).await?;
            resource_in(db, &mut facts, &input.reference).await?;
        }
    }
    Ok(facts)
}
