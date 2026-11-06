//! Optional actor-owned retention of already registered, caller-scoped views.

use super::*;
use crate::tenancy::model::{AuthorityClass, Caller};
use bitrouter_ai::tenancy::authority::ResourceKind;

pub(super) struct Entry {
    hash: [u8; 64],
    pub(super) lease: Lease,
}

impl State {
    pub(super) fn clear_reuse(&mut self) -> Result<()> {
        // Release capacity as well as soft owners. Active aliases stay charged.
        self.reuse = Vec::new();
        self.views.retain(|view| view.strong_count() > 0);
        if self.views.is_empty() {
            self.views = Vec::new();
        } else if self.views.len() < self.views.capacity() {
            let mut compact = Vec::new();
            compact
                .try_reserve_exact(self.views.len())
                .map_err(|_| ErrorCode::CapacityExceeded)?;
            // The existing registry charge reserves TWO Weak vectors at the
            // old capacity. Keep that overlap charged until the old allocation
            // is dropped; only then can a larger cold view use the released room.
            if compact.capacity() > self.views.capacity() {
                return Err(ErrorCode::CapacityExceeded);
            }
            compact.extend(self.views.iter().cloned());
            self.views = compact;
        }
        Ok(())
    }

    pub(super) fn registry_bytes(&self) -> Result<usize> {
        let mut bytes = memory::registry(self.views.capacity())?;
        memory::add(&mut bytes, memory::reuse(self.reuse.capacity())?)?;
        Ok(bytes)
    }
}

impl View {
    pub fn covers(&self, dependencies: &[ResourceRef], intent: DependencyIntent) -> Result<bool> {
        let mut complete = true;
        for reference in dependencies {
            complete &= self.facts.resource_lifecycle.loaded(reference);
            if reference.resource_kind == ResourceKind::Credential {
                if self.oauth.iter().any(|fact| &fact.reference == reference) {
                    // Loaded absence is different from a missing or mismatched fact.
                    self.oauth(reference)?;
                } else {
                    complete = false;
                }
            }
        }
        if intent == DependencyIntent::Fresh {
            complete &= self
                .facts
                .resource_lifecycle
                .records()
                .all(|record| dependencies.contains(&record.reference))
                && self
                    .facts
                    .resource_lifecycle
                    .absences()
                    .all(|reference| dependencies.contains(reference))
                && self.oauth.iter().all(|fact| {
                    fact.reference.resource_kind == ResourceKind::Credential
                        && dependencies.contains(&fact.reference)
                });
        }
        Ok(complete)
    }

    /// Match the indexed caller closure, including the optional managed owner.
    /// Missing newly linked facts require SQL; audit readers need no binding.
    fn caller_scoped(&self, caller: &Caller) -> Result<bool> {
        let credential = self.facts.credential(caller, chrono::Utc::now())?;
        let facts = &self.facts;
        if facts.credentials.len() != 1
            || facts.tenants.len() != 1
            || !facts.tenants.contains_key(&credential.tenant)
            || !facts.principals.contains_key(&credential.principal)
            || facts
                .active_configurations
                .keys()
                .any(|id| id != &credential.tenant)
        {
            return Ok(false);
        }
        let binding = match &credential.binding {
            Some(id) => {
                let Some(binding) = facts.bindings.get(&(credential.tenant.clone(), id.clone()))
                else {
                    return Ok(false);
                };
                if facts.bindings.len() != 1 || !facts.principals.contains_key(&binding.principal) {
                    return Ok(false);
                }
                Some(binding)
            }
            None if facts.bindings.is_empty() => None,
            None => return Ok(false),
        };
        let managed = binding
            .and_then(|binding| binding.managed_by.as_ref())
            .filter(|owner| owner.class == AuthorityClass::Tenant);
        if managed.is_some_and(|owner| !facts.principals.contains_key(&owner.principal)) {
            return Ok(false);
        }
        Ok(facts.principals.keys().all(|id| {
            id == &credential.principal
                || binding.is_some_and(|binding| id == &binding.principal)
                || managed.is_some_and(|owner| id == &owner.principal)
        }))
    }
}

impl Authority {
    /// Called only from the admitted actor job; no SQL or await under this gate.
    pub fn try_reuse_token(self: &Arc<Self>, hash: &str) -> Result<Option<Authenticated>> {
        let mut state = self
            .state
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !self.live.load(Ordering::Acquire) || state.preparing {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let Some(index) = state
            .reuse
            .iter()
            .position(|entry| entry.hash.as_slice() == hash.as_bytes())
        else {
            return Ok(None);
        };
        let (caller, bytes) = {
            let lease = &state.reuse[index].lease;
            if !std::ptr::eq(lease.owner.as_ptr(), self.as_ref()) {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let view = lease
                .inner
                .view
                .lock()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            if view.publication != state.publication {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            let Some(credential) = view.facts.credentials.values().next() else {
                return Err(ErrorCode::AuthorityUnavailable);
            };
            if credential.key_hash != hash {
                return Ok(None);
            }
            let caller = Caller::Credential(credential.id.clone());
            if !view.caller_scoped(&caller)? {
                return Ok(None);
            }
            (caller, view.bytes)
        };
        // No temporary source Arc survives this preflight/eviction boundary.
        let views = match self.reserve_registration(&mut state, bytes) {
            Ok(views) => views,
            Err(ErrorCode::CapacityExceeded) => {
                state.clear_reuse()?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut copied = state.reuse[index]
            .lease
            .inner
            .view
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .clone();
        copied.bytes = memory::bytes(&copied)?;
        if copied.bytes > bytes {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let lease = self.install(&mut state, copied, views);
        Ok(Some(Authenticated {
            caller,
            lease,
            observed_revision: state.header.revision,
        }))
    }

    /// Fresh exact preparation only. Optional retention must not reject success
    /// for memory pressure, and stores no entitlement, payload or duplicate graph.
    pub fn retain_prepared(
        &self,
        caller: &Caller,
        lease: &Lease,
        dependencies: &[ResourceRef],
    ) -> Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !self.live.load(Ordering::Acquire)
            || state.preparing
            || !std::ptr::eq(lease.owner.as_ptr(), self)
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let hash = {
            let view = lease
                .inner
                .view
                .lock()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            if view.publication != state.publication {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            if !view.covers(dependencies, DependencyIntent::Fresh)?
                || !view.caller_scoped(caller)?
            {
                return Ok(());
            }
            view.facts
                .credential(caller, chrono::Utc::now())?
                .key_hash
                .as_bytes()
                .try_into()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?
        };
        state
            .reuse
            .retain(|entry| entry.hash != hash && !Arc::ptr_eq(&entry.lease.inner, &lease.inner));
        let result = self.retain_entry(&mut state, hash, lease);
        match result {
            Err(ErrorCode::CapacityExceeded) => {
                state.clear_reuse()?;
                match self.retain_entry(&mut state, hash, lease) {
                    Err(ErrorCode::CapacityExceeded) => Ok(()),
                    result => result,
                }
            }
            result => result,
        }
    }

    fn retain_entry(&self, state: &mut State, hash: [u8; 64], lease: &Lease) -> Result<()> {
        let required = state
            .reuse
            .len()
            .checked_add(1)
            .ok_or(ErrorCode::CapacityExceeded)?;
        if required > self.max_entries {
            return Err(ErrorCode::CapacityExceeded);
        }
        // Charge unique registered payload once, even when it has many aliases.
        state.views.retain(|view| view.strong_count() > 0);
        let mut bytes = state.registry_bytes()?;
        let mut registered = false;
        for inner in state.views.iter().filter_map(Weak::upgrade) {
            registered |= Arc::ptr_eq(&inner, &lease.inner);
            memory::add(
                &mut bytes,
                inner
                    .view
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?
                    .bytes,
            )?;
        }
        if !registered {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(required)
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        // Both old and replacement container allocations exist during insertion.
        memory::add(&mut bytes, memory::reuse(entries.capacity())?)?;
        if bytes > self.working_bytes {
            return Err(ErrorCode::CapacityExceeded);
        }
        entries.extend(state.reuse.iter().map(|entry| Entry {
            hash: entry.hash,
            lease: entry.lease.clone(),
        }));
        entries.push(Entry {
            hash,
            lease: lease.clone(),
        });
        state.reuse = entries;
        Ok(())
    }
}
