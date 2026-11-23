//! Authenticated profile associations and monotonic route-key protection.
//!
//! Permanent SQL witnesses address immutable definitions. Live authority keeps
//! only the requested pins and current heads; historical cardinality never
//! consumes publication memory or becomes an admission quota.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    admission::{AdmissionProfile, HealthProfileV1},
    authority::{ResourceKey, ResourceKind},
    configuration::ResourceRef,
    identity::{OwnerId, ResourceId},
    operation::ErrorCode,
    resources::Definition as PublicDefinition,
    validation::Validate,
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

pub(in crate::tenancy) mod index;
pub(in crate::tenancy) mod profile;

/// Shared authenticated admission metadata, published under the authority gate.
#[derive(Clone, Default)]
pub(in crate::tenancy) struct State {
    profiles: HashMap<ResourceRef, AdmissionProfile>,
    requested: HashSet<ResourceRef>,
    loaded_keys: HashSet<ResourceKey>,
    lifecycle: lifecycle::State,
    group_rules: HashMap<(OwnerId, ResourceId), HealthProfileV1>,
    policy_keys: HashSet<ResourceKey>,
    protected_keys: HashSet<ResourceKey>,
    profiled_routes: HashSet<ResourceRef>,
}

impl State {
    pub(in crate::tenancy) fn working_bytes(&self) -> std::result::Result<usize, ErrorCode> {
        // Each requested key reserves its permanent witness, current head,
        // lifecycle and map shapes before a later rotation/revocation arrives.
        self.loaded_keys
            .len()
            .checked_mul(5 * 4096)
            .and_then(|n| {
                self.requested
                    .len()
                    .checked_mul(8192)
                    .and_then(|m| n.checked_add(m))
            })
            .and_then(|n| {
                self.group_rules
                    .len()
                    .checked_mul(4096)
                    .and_then(|m| n.checked_add(m))
            })
            .ok_or(ErrorCode::CapacityExceeded)
    }

    pub(in crate::tenancy) fn affected(&self, updates: &Self) -> bool {
        self.loaded_keys
            .iter()
            .any(|key| updates.loaded_keys.contains(key))
    }

    pub(in crate::tenancy) fn publish(&mut self, updates: &Self) {
        for key in &self.loaded_keys {
            if !updates.loaded_keys.contains(key) {
                continue;
            }
            if updates.protected_keys.contains(key) {
                self.protected_keys.insert(key.clone());
            }
            // Keep the original pins; replace the transient witness/head closure.
            self.profiles.retain(|reference, _| {
                reference.key() != *key || self.requested.contains(reference)
            });
            for (reference, profile) in &updates.profiles {
                if reference.key() == *key
                    && (self.requested.contains(reference)
                        || updates
                            .lifecycle
                            .record(reference)
                            .is_some_and(|record| record.is_head))
                {
                    self.profiles.insert(reference.clone(), profile.clone());
                }
            }
        }
        let mut lifecycle = lifecycle::State::default();
        for record in self.lifecycle.records() {
            if self.profiles.contains_key(&record.reference) {
                lifecycle.insert(record.clone());
            }
        }
        for record in updates.lifecycle.records() {
            if self.profiles.contains_key(&record.reference) {
                lifecycle.insert(record.clone());
            }
        }
        self.lifecycle = lifecycle;
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
    ) -> Option<(ResourceRef, &AdmissionProfile)> {
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

    pub(in crate::tenancy) fn profile(&self, reference: &ResourceRef) -> Option<&AdmissionProfile> {
        self.profiles.get(reference)
    }

    pub(in crate::tenancy) fn validate_put(
        &self,
        key: &ResourceKey,
        definition: &PublicDefinition,
    ) -> std::result::Result<(), ErrorCode> {
        match definition {
            PublicDefinition::AdmissionProfile { profile } => {
                self.validate_profile(key, profile)?;
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

    fn validate_profile(
        &self,
        key: &ResourceKey,
        profile: &AdmissionProfile,
    ) -> std::result::Result<(), ErrorCode> {
        profile.validate().map_err(|_| ErrorCode::InvalidRequest)?;
        if self.policy_keys.contains(key)
            || self.profiles.iter().any(|(reference, previous)| {
                reference.key() == *key && !previous.permits_rotation_to(profile)
            })
        {
            return Err(ErrorCode::InvalidRequest);
        }
        if let (Some(group), Some(rules)) = (&profile.health_group_id, &profile.health)
            && self
                .group_rules
                .get(&(key.owner_id.clone(), group.clone()))
                .is_some_and(|previous| previous != rules)
        {
            return Err(ErrorCode::InvalidRequest);
        }
        Ok(())
    }

    pub(in crate::tenancy) fn record_configured_profile(
        &mut self,
        reference: &ResourceRef,
        profile: &AdmissionProfile,
    ) {
        self.record_profile(reference, profile);
    }

    pub(in crate::tenancy) fn record_profiled_route(&mut self, reference: &ResourceRef) {
        self.protected_keys.insert(reference.key());
        self.profiled_routes.insert(reference.clone());
    }

    fn record_profile(&mut self, reference: &ResourceRef, profile: &AdmissionProfile) {
        if let (Some(group), Some(rules)) = (&profile.health_group_id, &profile.health) {
            self.group_rules
                .entry((reference.owner_id.clone(), group.clone()))
                .or_insert_with(|| rules.clone());
        }
        self.profiles.insert(reference.clone(), profile.clone());
    }

    pub(in crate::tenancy) fn apply_put(
        &mut self,
        reference: &ResourceRef,
        definition: &PublicDefinition,
    ) {
        match definition {
            PublicDefinition::AdmissionProfile { profile } => {
                self.record_profile(reference, profile);
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
                self.validate_profile(&reference.key(), profile)
                    .map_err(|_| {
                        anyhow::anyhow!("admission profile or shared group immutable rules changed")
                    })?;
                self.record_profile(reference, profile);
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

// Complete materialization exists only in small component tests. Production
// startup authenticates retained witnesses and commands use admission_in.
#[cfg(test)]
impl Resources {
    pub(in crate::tenancy) async fn admission_state(
        &self,
        db: &impl ConnectionTrait,
        _budget: usize,
    ) -> Result<State> {
        let mut references = Vec::new();
        let mut after = None;
        loop {
            let page = storage::reference_page(db, after.as_ref()).await?;
            if page.is_empty() {
                break;
            }
            after = page.last().cloned();
            references.extend(page);
        }
        let header = crate::tenancy::store::startup::read_header_in(db, 4096)
            .await?
            .context("admission authority")?
            .header;
        self.admission_in(db, header.revision, &references, &[], &[])
            .await
    }
}

pub(super) fn validate_graph(artifacts: &[Artifact]) -> Result<()> {
    // A retained closure must be internally consistent even when read without
    // a live catalog. The writer additionally checks all historical members.
    let mut profiles = State::default();
    for artifact in artifacts {
        profiles.insert(artifact)?;
    }
    for artifact in artifacts {
        let Some((policy, attachments)) = artifact.definition.admission() else {
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
            if super::execution::generation_protocol(&chain.inbound).is_err() {
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
                    profile.definition.is_admission_profile(),
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
    let header = crate::tenancy::store::startup::read_header_in(db, 4096)
        .await?
        .context("admission import authority missing")?
        .header;
    let keys: Vec<_> = imported.iter().map(|item| item.reference.key()).collect();
    let groups: Vec<_> = imported
        .iter()
        .filter_map(|item| match &item.definition {
            Definition::AdmissionProfile { profile } => profile
                .health_group_id
                .as_ref()
                .map(|group| (item.reference.owner_id.clone(), group.clone())),
            _ => None,
        })
        .collect();
    let mut state = resources
        .admission_in(db, header.revision, &[], &keys, &groups)
        .await?;
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
