//! Authenticated profile associations and monotonic route-key protection.
//!
//! Immutable resource definitions are the durable protection record. The
//! authority snapshot derives its bounded index from those authenticated rows;
//! rotation, revocation and later legacy imports cannot remove old definitions.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    tenancy::{
        admission::AdmissionProfileV1,
        authority::{ResourceKey, ResourceKind},
        configuration::ResourceRef,
        operation::ErrorCode,
        resources::Definition as PublicDefinition,
    },
    types::ApiProtocol,
};
use sea_orm::ConnectionTrait;

use crate::tenancy::configuration_store;

use super::{
    Resources, lifecycle,
    manifest::{Artifact, Definition, Import},
    storage,
};

#[cfg(test)]
mod tests;

// Limit the feature's retained operational facts, independent of the directory
// and ordinary resource catalog. One conservative allowance covers each retained
// profile/route identity, its compact rules, lifecycle facts and map capacity.
const MAX_ADMISSION_FACTS: usize = 4096;
const FACT_BYTES: usize = 4096;

/// Shared authenticated admission metadata, published under the authority gate.
#[derive(Clone, Default)]
pub(in crate::tenancy) struct State {
    profiles: HashMap<ResourceRef, AdmissionProfileV1>,
    lifecycle: lifecycle::State,
    policy_keys: HashSet<ResourceKey>,
    protected_keys: HashSet<ResourceKey>,
    profiled_routes: HashSet<ResourceRef>,
}

impl State {
    pub(in crate::tenancy) fn working_bytes(&self) -> std::result::Result<usize, ErrorCode> {
        let count = self.profiles.len() + self.profiled_routes.len();
        if count > MAX_ADMISSION_FACTS {
            return Err(ErrorCode::CapacityExceeded);
        }
        Ok(count * FACT_BYTES)
    }

    pub(in crate::tenancy) fn has_profile_key(&self, key: &ResourceKey) -> bool {
        self.profiles
            .keys()
            .any(|reference| &reference.key() == key)
    }

    pub(in crate::tenancy) fn tracks(&self, reference: &ResourceRef) -> bool {
        self.profiles.contains_key(reference)
    }

    pub(in crate::tenancy) fn apply_lifecycle(&mut self, record: &lifecycle::Record) {
        self.lifecycle.insert(record.clone());
    }

    pub(in crate::tenancy) fn current_profile(
        &self,
        pinned: &ResourceRef,
    ) -> Option<(ResourceRef, &AdmissionProfileV1)> {
        if !self.profiles.contains_key(pinned) || self.lifecycle.revoked(pinned) {
            return None;
        }
        let mut current = pinned.clone();
        current.revision_id = self.lifecycle.head(&pinned.key())?.clone();
        if self.lifecycle.revoked(&current) {
            return None;
        }
        Some((current.clone(), self.profiles.get(&current)?))
    }

    pub(in crate::tenancy) fn protected(&self, route: &ResourceRef) -> bool {
        self.protected_keys.contains(&route.key())
    }

    pub(in crate::tenancy) fn configuration(
        &self,
        routes: &[ResourceRef],
    ) -> std::result::Result<(), ErrorCode> {
        if routes
            .iter()
            .any(|route| self.protected(route) && !self.profiled_routes.contains(route))
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        Ok(())
    }

    pub(in crate::tenancy) fn profile(
        &self,
        reference: &ResourceRef,
    ) -> Option<&AdmissionProfileV1> {
        self.profiles.get(reference)
    }

    pub(in crate::tenancy) fn validate_put(
        &self,
        key: &ResourceKey,
        definition: &PublicDefinition,
    ) -> std::result::Result<(), ErrorCode> {
        match definition {
            PublicDefinition::AdmissionProfile { profile } => {
                if self.policy_keys.contains(key)
                    || self.profiles.iter().any(|(reference, previous)| {
                        reference.key() == *key && !previous.permits_rotation_to(profile)
                    })
                {
                    return Err(ErrorCode::InvalidRequest);
                }
            }
            PublicDefinition::Policy { .. }
                if self
                    .profiles
                    .keys()
                    .any(|reference| reference.key() == *key) =>
            {
                return Err(ErrorCode::InvalidRequest);
            }
            _ => {}
        }
        Ok(())
    }

    pub(in crate::tenancy) fn apply_put(
        &mut self,
        reference: &ResourceRef,
        definition: &PublicDefinition,
    ) {
        match definition {
            PublicDefinition::AdmissionProfile { profile } => {
                self.profiles.insert(reference.clone(), profile.clone());
            }
            PublicDefinition::ProfiledRoute { .. } => {
                self.protected_keys.insert(reference.key());
                self.profiled_routes.insert(reference.clone());
            }
            PublicDefinition::Policy { .. } => {
                self.policy_keys.insert(reference.key());
            }
            _ => {}
        }
    }

    fn insert(&mut self, artifact: &Artifact) -> Result<()> {
        let reference = &artifact.reference;
        match &artifact.definition {
            Definition::AdmissionProfile { profile } => {
                ensure!(
                    !self.policy_keys.contains(&reference.key()),
                    "admission profile key changes type"
                );
                for (previous, rules) in &self.profiles {
                    ensure!(
                        previous.key() != reference.key() || rules.permits_rotation_to(profile),
                        "admission profile immutable rules changed"
                    );
                }
                self.profiles.insert(reference.clone(), profile.clone());
            }
            Definition::Policy { .. } => {
                ensure!(
                    !self
                        .profiles
                        .keys()
                        .any(|previous| previous.key() == reference.key()),
                    "admission profile key changes type"
                );
                self.policy_keys.insert(reference.key());
            }
            Definition::ProfiledRoute { .. } => {
                self.protected_keys.insert(reference.key());
                self.profiled_routes.insert(reference.clone());
            }
            _ => {}
        }
        Ok(())
    }
}

impl Resources {
    pub(in crate::tenancy) async fn admission_state(
        &self,
        db: &impl ConnectionTrait,
        budget: usize,
    ) -> Result<State> {
        let mut state = State::default();
        let mut after = None;
        loop {
            let page = storage::reference_page(db, after.as_ref()).await?;
            if page.is_empty() {
                break;
            }
            after = page.last().cloned();
            for reference in page {
                if !matches!(
                    reference.resource_kind,
                    ResourceKind::Policy | ResourceKind::Route
                ) {
                    continue;
                }
                let artifact = self
                    .load(db, &reference)
                    .await
                    .map_err(|_| anyhow::anyhow!("admission resource unavailable"))?;
                if !artifact.definition.is_profiled() {
                    continue;
                }
                ensure_admission_capacity(&state, budget)?;
                state.insert(&artifact)?;
                if artifact.definition.is_admission_profile() {
                    let record = lifecycle::exact(db, &self.installation, &reference)
                        .await?
                        .context("admission profile has no managed lifecycle")?;
                    state.apply_lifecycle(&record);
                }
            }
        }
        for reference in state.profiles.keys() {
            let revision = state
                .lifecycle
                .head(&reference.key())
                .context("admission profile has no managed head")?;
            let mut head = reference.clone();
            head.revision_id = revision.clone();
            ensure!(
                state.profiles.contains_key(&head),
                "admission profile head changes type"
            );
        }
        Ok(state)
    }
}

fn ensure_admission_capacity(state: &State, budget: usize) -> Result<()> {
    let bytes = state
        .working_bytes()
        .map_err(|_| crate::tenancy::store::directory::CapacityExceeded)?;
    if bytes + FACT_BYTES > budget || bytes / FACT_BYTES >= MAX_ADMISSION_FACTS {
        return Err(crate::tenancy::store::directory::CapacityExceeded.into());
    }
    Ok(())
}

pub(super) fn validate_graph(artifacts: &[Artifact]) -> Result<()> {
    for artifact in artifacts {
        let Definition::ProfiledRoute {
            policy,
            attachments,
            ..
        } = &artifact.definition
        else {
            continue;
        };
        ensure!(policy.is_none(), "profiled route requires a static root");
        let route = artifact
            .envelope
            .as_ref()
            .context("missing profiled route envelope")?;
        ensure!(
            route.policy.is_none() && route.policy_name.is_none(),
            "profiled policy is unsupported"
        );
        ensure!(
            ![
                "previous_response_id",
                "conversation",
                "context_management",
                "cachedContent"
            ]
            .iter()
            .any(|field| route
                .defaults
                .get(*field)
                .is_some_and(|value| !value.is_null())),
            "profiled continuation is unsupported"
        );
        let mut observed = HashSet::new();
        for chain in &route.chains {
            if !matches!(
                chain.inbound,
                ApiProtocol::ChatCompletions | ApiProtocol::Messages | ApiProtocol::Responses
            ) {
                continue;
            }
            ensure!(chain.selector == route.selector, "profiled non-root chain");
            let protocol = super::execution::generation_protocol(&chain.inbound)?;
            for (ordinal, target) in chain.targets.iter().enumerate() {
                let ordinal = u16::try_from(ordinal)?;
                ensure!(
                    observed.insert((protocol, ordinal)),
                    "duplicate profiled root"
                );
                let attachment = attachments
                    .iter()
                    .find(|item| {
                        item.inbound_protocol == protocol && item.candidate_index == ordinal
                    })
                    .context("missing admission attachment")?;
                ensure!(
                    attachment.credential == target.credential,
                    "admission credential association mismatch"
                );
                let profile = artifacts
                    .iter()
                    .find(|item| item.reference == attachment.profile)
                    .context("missing pinned admission profile")?;
                ensure!(
                    matches!(profile.definition, Definition::AdmissionProfile { .. }),
                    "admission association references another policy type"
                );
                super::execution::qualify_profiled_target(&target.target)?;
                if let Some(credential) = artifacts
                    .iter()
                    .find(|item| item.reference == target.credential)
                {
                    ensure!(matches!(&credential.definition, Definition::Credential {
                        credential: crate::gateway::providers::auth::credentials::Credential::ApiKey { .. }, oauth: None, oauth_generation: 0,
                    }), "profiled route requires static credentials");
                }
            }
        }
        ensure!(
            !observed.is_empty() && observed.len() == attachments.len(),
            "foreign admission attachment"
        );
    }
    Ok(())
}

pub(super) async fn preflight_import(
    resources: &Resources,
    db: &impl ConnectionTrait,
    imported: &[Import],
) -> Result<()> {
    if !imported.iter().any(|item| {
        item.definition.is_profiled() || matches!(item.definition, Definition::Policy { .. })
    }) {
        return Ok(());
    }
    let budget = crate::config::tenancy::capacity::AuthorityViews::default().bytes / 3;
    let mut state = resources.admission_state(db, budget).await?;
    let mut initial = HashSet::new();
    for item in imported {
        let key = item.reference.key();
        if item.definition.is_admission_profile() {
            if configuration_store::resource(db, &item.reference)
                .await?
                .is_none()
            {
                ensure!(
                    !storage::resource_key_exists(db, &key).await? && initial.insert(key.clone()),
                    "profile rotations require authenticated CAS"
                );
            } else {
                ensure!(
                    lifecycle::head(db, &resources.installation, &key)
                        .await?
                        .is_some(),
                    "retained admission profile has no managed head"
                );
            }
        }
        if item.definition.is_profiled() {
            ensure_admission_capacity(&state, budget)?;
        }
        state.insert(&Artifact {
            reference: item.reference.clone(),
            definition: item.definition.clone(),
            envelope: None,
        })?;
    }
    Ok(())
}

pub(super) async fn initialize_import_heads(
    resources: &Resources,
    db: &impl ConnectionTrait,
    imported: &[Import],
) -> Result<()> {
    if !imported
        .iter()
        .any(|item| item.definition.is_admission_profile())
    {
        return Ok(());
    }
    let state = crate::tenancy::store::startup::read_header_in(db, 4096)
        .await?
        .context("admission import authority missing")?
        .header;
    ensure!(
        state.installation == resources.installation,
        "admission import installation mismatch"
    );
    for item in imported {
        if !item.definition.is_admission_profile() {
            continue;
        }
        if lifecycle::head(db, &resources.installation, &item.reference.key())
            .await?
            .is_some()
        {
            continue;
        }
        lifecycle::write(
            db,
            &resources.installation,
            &lifecycle::Record {
                reference: item.reference.clone(),
                is_head: true,
                revoked: false,
                authority_revision: state.revision,
            },
        )
        .await?;
    }
    Ok(())
}
