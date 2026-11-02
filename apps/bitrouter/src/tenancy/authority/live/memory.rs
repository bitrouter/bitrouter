//! Conservative allocation payload charges for private, normalized live views.
//! Opaque IDs are rebuilt through `Box<str>`, as at the execution-cache boundary.
//! Tree allowances cover an internal node per entry; hash allowances cover fresh
//! insert-only maps. Allocator bookkeeping and caller-owned Lease handles are not
//! an RSS promise. Reaudit opaque collection allowances with toolchain upgrades.

use std::{collections::BTreeMap, mem::size_of};

use bitrouter_ai::tenancy::{
    authority::{GrantScope, Permission, ResourceKey},
    configuration::ResourceRef,
    identity::{BindingId, CredentialId, PrincipalId, RevisionId, TenantId},
};

use super::{Authority, Inner, OAuthFact, Result, Staged, View};
use crate::tenancy::{model, oauth::storage, resources::lifecycle};
use bitrouter_ai::tenancy::operation::ErrorCode;

pub(super) fn add(total: &mut usize, bytes: usize) -> Result<()> {
    *total = total
        .checked_add(bytes)
        .ok_or(ErrorCode::CapacityExceeded)?;
    Ok(())
}

fn product(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right).ok_or(ErrorCode::CapacityExceeded)
}

fn tight<T: Clone + Into<String> + TryFrom<String>>(value: &mut T) -> Result<()> {
    let text: String = value.clone().into();
    *value = text
        .into_boxed_str()
        .into_string()
        .try_into()
        .map_err(|_| ErrorCode::AuthorityUnavailable)?;
    Ok(())
}

fn reference(value: &mut ResourceRef) -> Result<()> {
    tight(&mut value.owner_id)?;
    tight(&mut value.resource_id)?;
    tight(&mut value.revision_id)
}

fn scope(value: &mut GrantScope) -> Result<()> {
    let (GrantScope::Tenant {
        operator_pools: keys,
    }
    | GrantScope::Resources { resources: keys }) = value;
    for key in keys {
        tight(&mut key.owner_id)?;
        tight(&mut key.resource_id)?;
    }
    Ok(())
}

fn map<K: Ord, V>(
    values: &mut BTreeMap<K, V>,
    mut entry: impl FnMut(&mut K, &mut V) -> Result<()>,
) -> Result<()> {
    let old = std::mem::take(values);
    for (mut key, mut value) in old {
        entry(&mut key, &mut value)?;
        values.insert(key, value);
    }
    Ok(())
}

pub(super) fn normalize(view: &mut View) -> Result<()> {
    let value = &mut view.facts;
    tight(&mut value.installation)?;
    map(&mut value.tenants, |key, value| {
        tight(key)?;
        tight(&mut value.id)?;
        tight(&mut value.revision)
    })?;
    map(&mut value.principals, |key, value| {
        tight(key)?;
        tight(&mut value.id)?;
        tight(&mut value.revision)
    })?;
    map(&mut value.bindings, |key, value| {
        tight(&mut key.0)?;
        tight(&mut key.1)?;
        tight(&mut value.tenant)?;
        tight(&mut value.id)?;
        tight(&mut value.principal)?;
        tight(&mut value.revision)?;
        tight(&mut value.expires.wire)?;
        if let Some(owner) = &mut value.managed_by {
            tight(&mut owner.principal)?;
        }
        scope(&mut value.scope)
    })?;
    map(&mut value.credentials, |key, value| {
        tight(key)?;
        tight(&mut value.id)?;
        tight(&mut value.tenant)?;
        tight(&mut value.principal)?;
        if let Some(binding) = &mut value.binding {
            tight(binding)?;
        }
        tight(&mut value.revision)?;
        tight(&mut value.expires.wire)?;
        scope(&mut value.scope)
    })?;
    value.credential_hashes.clear();
    map(&mut value.active_configurations, |key, value| {
        tight(key)?;
        tight(value)
    })?;
    let mut state = lifecycle::State::default();
    state.restrict();
    for record in value.resource_lifecycle.records() {
        let mut record = record.clone();
        reference(&mut record.reference)?;
        state.insert(record);
    }
    for absent in value.resource_lifecycle.absences() {
        let mut absent = absent.clone();
        reference(&mut absent)?;
        state.absent(absent);
    }
    value.resource_lifecycle = state;
    for fact in &mut view.oauth {
        reference(&mut fact.reference)?;
        if let Some(metadata) = &mut fact.metadata {
            tight(&mut metadata.installation)?;
            reference(&mut metadata.reference)?;
            if let storage::State::RefreshStarted(work) | storage::State::Quarantined(work) =
                &mut metadata.state
            {
                tight(&mut work.execution)?;
                tight(&mut work.tenant)?;
                tight(&mut work.principal)?;
                tight(&mut work.started_at)?;
            }
        }
    }
    Ok(())
}

pub(super) fn registry(capacity: usize) -> Result<usize> {
    let mut bytes = size_of::<Authority>() + size_of::<[usize; 8]>() + 256;
    // Includes the old/replacement registry and the strong staged handles.
    add(
        &mut bytes,
        product(
            capacity,
            2 * size_of::<std::sync::Weak<Inner>>() + size_of::<Staged>(),
        )?,
    )?;
    Ok(bytes)
}

pub(super) fn reuse(capacity: usize) -> Result<usize> {
    product(capacity, size_of::<super::reuse::Entry>())
}

fn tree<K, V>(len: usize) -> Result<usize> {
    let node = 11usize
        .checked_mul(
            size_of::<K>()
                .checked_add(size_of::<V>())
                .ok_or(ErrorCode::CapacityExceeded)?,
        )
        .and_then(|bytes| bytes.checked_add(20 * size_of::<usize>() + 4))
        .ok_or(ErrorCode::CapacityExceeded)?;
    product(len.checked_add(1).ok_or(ErrorCode::CapacityExceeded)?, node)
}

fn hash<T>(len: usize) -> Result<usize> {
    if len == 0 {
        return Ok(0);
    }
    let capacity = len
        .checked_next_power_of_two()
        .and_then(|n| n.checked_mul(4))
        .ok_or(ErrorCode::CapacityExceeded)?;
    product(capacity, size_of::<T>() + 1)?
        .checked_add(64)
        .ok_or(ErrorCode::CapacityExceeded)
}

fn reference_bytes(value: &ResourceRef) -> usize {
    // Each identity is independently bounded to 128 bytes by the public contract.
    value.owner_id.as_str().len()
        + value.resource_id.as_str().len()
        + value.revision_id.as_str().len()
}

fn text_bytes(value: &String, cloned: bool) -> usize {
    if cloned {
        value.len()
    } else {
        value.capacity()
    }
}

fn scope_bytes(value: &GrantScope, cloned: bool) -> Result<usize> {
    let (GrantScope::Tenant {
        operator_pools: keys,
    }
    | GrantScope::Resources { resources: keys }) = value;
    let mut bytes = product(
        if cloned { keys.len() } else { keys.capacity() },
        size_of::<ResourceKey>(),
    )?;
    for key in keys {
        add(
            &mut bytes,
            key.owner_id.as_str().len() + key.resource_id.as_str().len(),
        )?;
    }
    Ok(bytes)
}

// These charges exclude map keys and nodes, which remain in place on replacement.
// Forecasts use the same helpers as admitted views. `cloned` charges fresh String
// and Vec clones by length, rather than charging unused capacity in the source.
// Fixed revision slack keeps denial independent of ID length.
pub(super) fn tenant(value: &model::Tenant, cloned: bool) -> Result<usize> {
    let mut bytes = value.id.as_str().len() + 128;
    add(&mut bytes, text_bytes(&value.created_at, cloned))?;
    Ok(bytes)
}

pub(super) fn principal(value: &model::Principal, cloned: bool) -> Result<usize> {
    let mut bytes = value.id.as_str().len() + 128;
    for text in [&value.issuer, &value.subject, &value.created_at] {
        add(&mut bytes, text_bytes(text, cloned))?;
    }
    Ok(bytes)
}

pub(super) fn binding(value: &model::Binding, cloned: bool) -> Result<usize> {
    let mut bytes = value.tenant.as_str().len()
        + value.id.as_str().len()
        + value.principal.as_str().len()
        + 128
        + value.expires.wire.as_str().len();
    if let Some(owner) = &value.managed_by {
        add(&mut bytes, owner.principal.as_str().len())?;
    }
    add(&mut bytes, scope_bytes(&value.scope, cloned)?)?;
    Ok(bytes)
}

pub(super) fn credential(value: &model::Credential, cloned: bool) -> Result<usize> {
    let mut bytes = value.id.as_str().len()
        + value.tenant.as_str().len()
        + value.principal.as_str().len()
        + 128
        + value.expires.wire.as_str().len();
    if let Some(binding) = &value.binding {
        add(&mut bytes, binding.as_str().len())?;
    }
    add(&mut bytes, text_bytes(&value.key_hash, cloned))?;
    add(&mut bytes, text_bytes(&value.created_at, cloned))?;
    add(
        &mut bytes,
        product(
            if cloned {
                value.permissions.len()
            } else {
                value.permissions.capacity()
            },
            size_of::<Permission>(),
        )?,
    )?;
    add(&mut bytes, scope_bytes(&value.scope, cloned)?)?;
    Ok(bytes)
}

pub(super) fn activation(tenant: &TenantId, revision: &RevisionId) -> Result<usize> {
    // tree(len) is linear, so each new pointer adds exactly one node allowance.
    let mut bytes = tree::<TenantId, RevisionId>(1)?
        .checked_sub(tree::<TenantId, RevisionId>(0)?)
        .ok_or(ErrorCode::CapacityExceeded)?;
    add(&mut bytes, tenant.as_str().len() + revision.as_str().len())?;
    Ok(bytes)
}

pub(super) fn oauth_metadata(value: Option<&storage::Metadata>, cloned: bool) -> Result<usize> {
    let mut bytes = 0;
    if let Some(value) = value {
        add(
            &mut bytes,
            value.installation.as_str().len() + reference_bytes(&value.reference),
        )?;
        for text in [
            &value.slot,
            &value.seed,
            &value.profile.mechanism,
            &value.profile.provider,
            &value.profile.account,
            &value.profile.registration,
            &value.profile.token_endpoint,
            &value.profile.client_id,
        ] {
            add(&mut bytes, text_bytes(text, cloned))?;
        }
        if let storage::State::RefreshStarted(work) | storage::State::Quarantined(work) =
            &value.state
        {
            add(&mut bytes, text_bytes(&work.id, cloned))?;
            add(
                &mut bytes,
                work.execution.as_str().len()
                    + work.tenant.as_str().len()
                    + work.principal.as_str().len()
                    + work.started_at.as_str().len(),
            )?;
        }
    }
    // Existing encoded-object contract, reserved even for proven absence.
    Ok(bytes.max(8192))
}

pub(super) fn bytes(view: &View) -> Result<usize> {
    let state = &view.facts;
    let mut bytes = size_of::<Inner>() + 4 * size_of::<usize>();
    add(&mut bytes, state.installation.as_str().len())?;
    add(&mut bytes, state.created_at.capacity())?;
    add(
        &mut bytes,
        tree::<TenantId, model::Tenant>(state.tenants.len())?,
    )?;
    for (key, value) in &state.tenants {
        add(&mut bytes, key.as_str().len())?;
        add(&mut bytes, tenant(value, false)?)?;
    }
    add(
        &mut bytes,
        tree::<PrincipalId, model::Principal>(state.principals.len())?,
    )?;
    for (key, value) in &state.principals {
        add(&mut bytes, key.as_str().len())?;
        add(&mut bytes, principal(value, false)?)?;
    }
    add(
        &mut bytes,
        tree::<(TenantId, BindingId), model::Binding>(state.bindings.len())?,
    )?;
    for (key, value) in &state.bindings {
        add(&mut bytes, key.0.as_str().len() + key.1.as_str().len())?;
        add(&mut bytes, binding(value, false)?)?;
    }
    add(
        &mut bytes,
        tree::<CredentialId, model::Credential>(state.credentials.len())?,
    )?;
    for (key, value) in &state.credentials {
        add(&mut bytes, key.as_str().len())?;
        add(&mut bytes, credential(value, false)?)?;
    }
    add(&mut bytes, tree::<String, CredentialId>(0)?)?;
    add(
        &mut bytes,
        tree::<TenantId, RevisionId>(state.active_configurations.len())?,
    )?;
    for (key, value) in &state.active_configurations {
        add(&mut bytes, key.as_str().len() + value.as_str().len())?;
    }
    // A loaded absence can become an exact revocation. Reserve the positive
    // record and both map shapes now, so a full registry still accepts denial.
    let lifecycle_count = state
        .resource_lifecycle
        .records()
        .count()
        .checked_add(state.resource_lifecycle.absences().count())
        .ok_or(ErrorCode::CapacityExceeded)?;
    add(
        &mut bytes,
        hash::<(ResourceRef, lifecycle::Record)>(lifecycle_count)?,
    )?;
    add(&mut bytes, hash::<ResourceRef>(lifecycle_count)?)?;
    add(&mut bytes, product(lifecycle_count, 3 * 3 * 128)?)?;
    add(
        &mut bytes,
        product(view.oauth.capacity(), size_of::<OAuthFact>())?,
    )?;
    for fact in &view.oauth {
        add(&mut bytes, reference_bytes(&fact.reference))?;
        add(&mut bytes, fact.slot.capacity())?;
        add(&mut bytes, oauth_metadata(fact.metadata.as_ref(), false)?)?;
    }
    Ok(bytes)
}
