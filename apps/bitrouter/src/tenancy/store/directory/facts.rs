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

// Point reads are sequential. Reserve one 16x encoded-row decode/driver window,
// rather than simultaneously reserving that window for every possible lookup.
// The duplicate probe is decoded only after uniqueness is established. Charge
// each retained record by its actual encoded length (4x for Vec/String slack),
// before reading its links, including records still held in recursive frames.
// Fixed tree/hash/lookup slack retains the original per-lookup reservation.
struct WorkingBudget {
    remaining: usize,
}

impl WorkingBudget {
    fn new(bytes: usize, reads: usize) -> Result<Self> {
        let metadata = 16
            * (std::mem::size_of::<Credential>()
                + std::mem::size_of::<Lookup>()
                + 4 * std::mem::size_of::<usize>());
        let remaining = reads
            .checked_mul(metadata)
            .and_then(|fixed| bytes.checked_sub(fixed))
            .filter(|bytes| *bytes <= isize::MAX as usize)
            .ok_or(CapacityExceeded)?;
        let budget = Self { remaining };
        budget.row_bytes()?;
        Ok(budget)
    }

    fn row_bytes(&self) -> Result<usize> {
        self.remaining
            .checked_div(16)
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| CapacityExceeded.into())
    }

    fn retain(&mut self, record: &impl serde::Serialize) -> Result<()> {
        let bytes = crate::tenancy::storage::encoded_size(record).map_err(|_| CapacityExceeded)?;
        self.remaining = bytes
            .checked_mul(4)
            .and_then(|bytes| self.remaining.checked_sub(bytes))
            .ok_or(CapacityExceeded)?;
        Ok(())
    }
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
        budget: &mut WorkingBudget,
    ) -> Result<()> {
        let lookup = Lookup::Tenant(id.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let record = tenant_in(db, id, budget.row_bytes()?).await?;
        budget.retain(&record)?;
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
        budget: &mut WorkingBudget,
    ) -> Result<()> {
        let lookup = Lookup::Principal(id.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let record = principal_in(db, id, budget.row_bytes()?).await?;
        budget.retain(&record)?;
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
        budget: &mut WorkingBudget,
    ) -> Result<()> {
        let lookup = Lookup::Binding(tenant.clone(), id.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let record = binding_in(db, tenant, id, budget.row_bytes()?).await?;
        budget.retain(&record)?;
        self.tenant(db, tenant, budget).await?;
        if let Some(record) = &record {
            self.principal(db, &record.principal, budget).await?;
            if let Some(owner) = &record.managed_by
                && owner.class == AuthorityClass::Tenant
            {
                self.principal(db, &owner.principal, budget).await?;
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
        budget: &mut WorkingBudget,
    ) -> Result<()> {
        let lookup = Lookup::Credential(id.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let record = credential_in(db, id, budget.row_bytes()?).await?;
        budget.retain(&record)?;
        if let Some(record) = &record {
            self.tenant(db, &record.tenant, budget).await?;
            self.principal(db, &record.principal, budget).await?;
            if let Some(binding) = &record.binding {
                self.binding(db, &record.tenant, binding, budget).await?;
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
        budget: &mut WorkingBudget,
    ) -> Result<()> {
        let lookup = Lookup::ActiveConfiguration(tenant.clone());
        if self.presence(&lookup) != Presence::NotLoaded {
            return Ok(());
        }
        let mut query = select(db, Table::Active, budget.row_bytes()?)?;
        query.and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()));
        let record = one(db, query, active_from).await?;
        budget.retain(
            &record
                .as_ref()
                .map(|record| (&record.tenant, &record.revision)),
        )?;
        if let Some(record) = &record {
            ensure!(
                record.authority_revision <= self.snapshot.revision,
                "active configuration is outside published authority"
            );
            let mut query = select(db, Table::Configuration, budget.row_bytes()?)?;
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
    let reads = 6 * usize::from(matches!(caller, Caller::Credential(_)));
    let mut budget = WorkingBudget::new(after_header(bytes, header)?, reads)?;
    caller_with_row_budget(db, header, caller, &mut budget).await
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
    let mut budget = WorkingBudget::new(after_header(bytes, header)?, reads)?;
    let mut facts = caller_with_row_budget(db, header, caller, &mut budget).await?;
    facts.tenant(db, tenant, &mut budget).await?;
    if include_active {
        facts.active(db, tenant, &mut budget).await?;
    }
    Ok(facts)
}

async fn caller_with_row_budget(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    caller: &Caller,
    budget: &mut WorkingBudget,
) -> Result<CommandFacts> {
    let mut facts = CommandFacts {
        snapshot: Snapshot::from_header(header),
        loaded: Default::default(),
    };
    if let Caller::Credential(id) = caller {
        facts.credential(db, id, budget).await?;
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
        Command::ResourceV3(_, input) => {
            let bitrouter_ai::tenancy::resources_v3::DefinitionV3::NativeProfiledRouteV1 {
                policy,
                credentials,
                attachments,
                ..
            } = &input.definition;
            credentials
                .len()
                .checked_add(attachments.len())
                .and_then(|n| n.checked_add(usize::from(policy.is_some()) + 1))
        }
        Command::Resource(_, input) => match &input.definition {
            Definition::Credential { .. } | Definition::AdmissionProfile { .. } => Some(1),
            Definition::Route {
                policy,
                credentials,
                ..
            } => credentials
                .len()
                .checked_add(usize::from(policy.is_some()))
                .and_then(|n| n.checked_add(1)),
            Definition::ProfiledRoute {
                policy,
                credentials,
                attachments,
                ..
            } => credentials
                .len()
                .checked_add(attachments.len())
                .and_then(|n| n.checked_add(usize::from(policy.is_some()) + 1)),
            Definition::Policy { dependencies, .. } => dependencies.len().checked_add(1),
        },
        Command::RevokeResource(..) | Command::ResourceV2(..) => Some(1),
        _ => Some(0),
    }
    .ok_or(CapacityExceeded)?;
    let reads = 17usize.checked_add(resources).ok_or(CapacityExceeded)?;
    let mut budget = WorkingBudget::new(after_header(bytes, header)?, reads)?;
    if resources > 0 && budget.row_bytes()? < LIFECYCLE_ROW_BYTES {
        return Err(CapacityExceeded.into());
    }
    let mut facts = caller_with_row_budget(db, header, caller, &mut budget).await?;
    match command {
        Command::Principal(id, input) => {
            facts.principal(db, id, &mut budget).await?;
            let record =
                principal_by_identity_in(db, &input.issuer, &input.subject, budget.row_bytes()?)
                    .await?;
            budget.retain(&record)?;
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
            facts.tenant(db, tenant, &mut budget).await?;
        }
        Command::Binding(tenant, id, input) => {
            facts.binding(db, tenant, id, &mut budget).await?;
            facts
                .principal(db, &input.principal_id, &mut budget)
                .await?;
        }
        Command::Detach(tenant, id, _) => {
            facts.binding(db, tenant, id, &mut budget).await?;
        }
        Command::Issue(tenant, input) => {
            facts
                .binding(db, tenant, &input.binding_id, &mut budget)
                .await?;
            facts
                .principal(db, &input.principal_id, &mut budget)
                .await?;
            if let Some(id) = &input.rotate_credential_id {
                facts.credential(db, id, &mut budget).await?;
            }
        }
        Command::AuditReader(tenant, input) => {
            facts.tenant(db, tenant, &mut budget).await?;
            facts
                .principal(db, &input.principal_id, &mut budget)
                .await?;
        }
        Command::Revoke(tenant, id, _) => {
            facts.tenant(db, tenant, &mut budget).await?;
            facts.credential(db, id, &mut budget).await?;
        }
        Command::Activate(tenant, _) => {
            facts.tenant(db, tenant, &mut budget).await?;
            facts.active(db, tenant, &mut budget).await?;
        }
        Command::Configuration(tenant, input) => {
            facts.tenant(db, tenant, &mut budget).await?;
            for reference in &input.document.routes {
                resource_in(db, &mut facts, reference).await?;
            }
            if let Some(policy) = &input.document.policy {
                resource_in(db, &mut facts, policy).await?;
            }
        }
        Command::Resource(tenant, input) => {
            facts.tenant(db, tenant, &mut budget).await?;
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
                Definition::Credential { .. } | Definition::AdmissionProfile { .. } => {}
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
                Definition::ProfiledRoute {
                    policy,
                    credentials,
                    attachments,
                    ..
                } => {
                    if let Some(policy) = policy {
                        resource_in(db, &mut facts, policy).await?;
                    }
                    for binding in credentials {
                        resource_in(db, &mut facts, &binding.credential).await?;
                    }
                    for attachment in attachments {
                        resource_in(db, &mut facts, &attachment.profile).await?;
                    }
                }
                Definition::Policy { dependencies, .. } => {
                    for reference in dependencies {
                        resource_in(db, &mut facts, reference).await?;
                    }
                }
            }
        }
        Command::ResourceV2(tenant, input) => {
            facts.tenant(db, tenant, &mut budget).await?;
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
        }
        Command::ResourceV3(tenant, input) => {
            facts.tenant(db, tenant, &mut budget).await?;
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
            let bitrouter_ai::tenancy::resources_v3::DefinitionV3::NativeProfiledRouteV1 {
                policy,
                credentials,
                attachments,
                ..
            } = &input.definition;
            if let Some(policy) = policy {
                resource_in(db, &mut facts, policy).await?;
            }
            for binding in credentials {
                resource_in(db, &mut facts, &binding.credential).await?;
            }
            for attachment in attachments {
                resource_in(db, &mut facts, &attachment.profile).await?;
            }
        }
        Command::RevokeResource(tenant, input) => {
            facts.tenant(db, tenant, &mut budget).await?;
            resource_in(db, &mut facts, &input.reference).await?;
        }
    }
    Ok(facts)
}
