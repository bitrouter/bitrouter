//! Request-owned authority facts and one synchronous publication boundary.
//!
//! The owning actor performs SQL reads and all registry mutations. Readers only
//! borrow prepared facts; they never perform I/O, refresh a view, or infer that
//! an unloaded resource/OAuth slot is absent. Publication allocation happens
//! before COMMIT, while final send holds the same gate through journal arm.

mod forecast;
mod memory;
mod reuse;
#[cfg(test)]
mod tests;

use std::sync::{
    Arc, Mutex, RwLock, RwLockReadGuard, Weak,
    atomic::{AtomicBool, Ordering},
};

use bitrouter_ai::tenancy::{configuration::ResourceRef, operation::ErrorCode};

use crate::tenancy::{
    model::{AuthorityHeader, Snapshot},
    oauth::storage::Metadata,
    store::Change,
};

type Result<T> = std::result::Result<T, ErrorCode>;

#[derive(Clone)]
pub(in crate::tenancy) struct OAuthFact {
    pub reference: ResourceRef,
    pub slot: String,
    pub metadata: Option<Metadata>,
}

/// Only selected facts, including successful negative dependency lookups.
#[derive(Clone)]
pub(in crate::tenancy) struct View {
    facts: Snapshot,
    oauth: Vec<OAuthFact>,
    publication: u64,
    bytes: usize,
}

impl View {
    pub fn facts(&self) -> &Snapshot {
        &self.facts
    }
    pub fn new(mut facts: Snapshot, oauth: Vec<OAuthFact>) -> Result<Self> {
        let mut seen = std::collections::HashSet::new();
        seen.try_reserve(oauth.len())
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        for fact in &oauth {
            if !seen.insert(&fact.reference)
                || fact.slot
                    != crate::tenancy::oauth::storage::slot(&facts.installation, &fact.reference)
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?
            {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            if let Some(metadata) = &fact.metadata {
                metadata
                    .validate()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                if metadata.slot != fact.slot || metadata.installation != facts.installation {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
            }
        }
        facts.resource_lifecycle.restrict();
        let mut value = Self {
            facts,
            oauth,
            publication: 0,
            bytes: 0,
        };
        memory::normalize(&mut value)?;
        value.bytes = memory::bytes(&value)?;
        Ok(value)
    }

    pub fn oauth(&self, reference: &ResourceRef) -> Result<Option<&Metadata>> {
        let fact = self
            .oauth
            .iter()
            .find(|fact| &fact.reference == reference)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        if fact.metadata.as_ref().is_some_and(|metadata| {
            metadata.reference != *reference || metadata.installation != self.facts.installation
        }) {
            return Err(ErrorCode::CapabilityMismatch);
        }
        Ok(fact.metadata.as_ref())
    }

    fn apply(&mut self, changes: &[Change], oauth: Option<(&Metadata, &Metadata)>) -> Result<()> {
        for change in changes {
            match change {
                Change::Tenant(value) => {
                    if let Some(old) = self.facts.tenants.get_mut(&value.id) {
                        *old = value.clone();
                    }
                }
                Change::Principal(value) => {
                    if let Some(old) = self.facts.principals.get_mut(&value.id) {
                        *old = value.clone();
                    }
                }
                Change::Binding(value) => {
                    if let Some(old) = self
                        .facts
                        .bindings
                        .get_mut(&(value.tenant.clone(), value.id.clone()))
                    {
                        *old = value.clone();
                    }
                }
                Change::Credential(value) => {
                    if let Some(old) = self.facts.credentials.get_mut(&value.id) {
                        // Authentication looks up hashes in SQL. A live view never
                        // becomes an index through which a new identity is minted.
                        *old = value.clone();
                    }
                }
                Change::Activation {
                    tenant, revision, ..
                } => {
                    if self.facts.tenants.contains_key(tenant) {
                        self.facts
                            .active_configurations
                            .insert(tenant.clone(), revision.clone());
                    }
                }
                Change::ResourceLifecycle { record, .. } => {
                    if self.facts.resource_lifecycle.loaded(&record.reference) {
                        self.facts.resource_lifecycle.insert(record.clone());
                    }
                }
                Change::Configuration(_) | Change::Resource(_) => {}
            }
        }
        if let Some((old, new)) = oauth {
            for fact in &mut self.oauth {
                if fact.slot == new.slot {
                    if fact.metadata.as_ref().is_some_and(|current| current != old) {
                        return Err(ErrorCode::AuthorityUnavailable);
                    }
                    fact.metadata = Some(new.clone());
                }
            }
        }
        // Existing facts have already crossed the normalization boundary. These
        // replacements are fresh clones of validated records, not externally
        // reserved buffers. Do not rebuild all maps while staging publication.
        self.bytes = memory::bytes(self)?;
        Ok(())
    }

    fn publish_header(&mut self, header: &AuthorityHeader, publication: u64) {
        // Installation and creation identity never change during a process.
        // All assignments are allocation-free under the publication lock.
        self.facts.revision = header.revision;
        self.facts.operation_ids = header.operation_ids;
        self.facts.safety_operation_ids = header.safety_operation_ids;
        self.facts.saturated = header.saturated;
        self.publication = publication;
    }
}

struct Inner {
    view: Mutex<View>,
}

#[derive(Clone)]
pub(in crate::tenancy) struct Lease {
    owner: Weak<Authority>,
    inner: Arc<Inner>,
}

pub(in crate::tenancy) struct Authenticated {
    pub caller: crate::tenancy::model::Caller,
    pub lease: Lease,
    pub observed_revision: bitrouter_ai::tenancy::identity::AuthorityRevision,
}

/// Only a new ingress lease may discard inherited request dependencies.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::tenancy) enum DependencyIntent {
    Fresh,
    Owner,
}

struct State {
    header: AuthorityHeader,
    publication: u64,
    views: Vec<Weak<Inner>>,
    reuse: Vec<reuse::Entry>,
    preparing: bool,
}

pub(in crate::tenancy) struct Authority {
    live: Arc<AtomicBool>,
    max_entries: usize,
    working_bytes: usize,
    state: RwLock<State>,
    #[cfg(test)]
    staged_clones: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pub(in crate::tenancy) read_jobs: std::sync::atomic::AtomicUsize,
}

pub(in crate::tenancy) struct ReadGuard<'a> {
    owner: &'a Authority,
    state: RwLockReadGuard<'a, State>,
}

impl ReadGuard<'_> {
    pub fn header(&self) -> &AuthorityHeader {
        &self.state.header
    }
    pub fn publication(&self) -> u64 {
        self.state.publication
    }

    /// Sequential checks can share this gate, including checks of the same lease.
    /// Requiring mutable access prevents recursive view-lock acquisition.
    pub fn check<T>(&mut self, lease: &Lease, check: impl FnOnce(&View) -> Result<T>) -> Result<T> {
        if !self.owner.live.load(Ordering::Acquire)
            || !std::ptr::eq(lease.owner.as_ptr(), self.owner)
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let view = lease
            .inner
            .view
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if view.publication != self.state.publication {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        check(&view)
    }
}

struct Staged {
    inner: Arc<Inner>,
    next: Option<View>,
}

/// An actor-owned reservation. Drop before COMMIT discards only prepared memory.
pub(in crate::tenancy) struct Publication {
    owner: Arc<Authority>,
    expected: u64,
    header: AuthorityHeader,
    views: Vec<Staged>,
    finished: bool,
}

impl Authority {
    pub fn load_budget(&self) -> usize {
        self.working_bytes
    }
    pub fn new(
        header: AuthorityHeader,
        budget: &crate::config::tenancy::capacity::AuthorityViews,
        live: Arc<AtomicBool>,
    ) -> Result<Arc<Self>> {
        // Header identity is immutable and charged independently of live views.
        if header.created_at.len() > 128
            || chrono::DateTime::parse_from_rfc3339(&header.created_at).is_err()
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let header = AuthorityHeader {
            installation: header
                .installation
                .as_str()
                .try_into()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?,
            created_at: header.created_at.into_boxed_str().into_string(),
            ..header
        };
        // One third holds live views. The other two cover final replacements
        // plus the current old view during clone-and-replace construction.
        let working_bytes = budget.bytes / 3;
        if budget.entries == 0 || memory::registry(0)? > working_bytes {
            return Err(ErrorCode::CapacityExceeded);
        }
        Ok(Arc::new(Self {
            live,
            max_entries: budget.entries,
            working_bytes,
            state: RwLock::new(State {
                header,
                publication: 0,
                views: Vec::new(),
                reuse: Vec::new(),
                preparing: false,
            }),
            #[cfg(test)]
            staged_clones: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            read_jobs: std::sync::atomic::AtomicUsize::new(0),
        }))
    }

    pub fn read(&self) -> Result<ReadGuard<'_>> {
        Ok(ReadGuard {
            owner: self,
            state: self
                .state
                .read()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?,
        })
    }

    /// Serialize stop/closing flags with final checks without advancing the header.
    /// The closure must be synchronous, perform no I/O, and never reacquire this
    /// gate. It may only take subordinate locks in publication lock order.
    pub fn with_write<T>(&self, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let _publication = self
            .state
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        action()
    }

    #[cfg(test)]
    pub(in crate::tenancy) fn mutate_views_for_test(
        &self,
        mut change: impl FnMut(&mut Snapshot),
    ) -> Result<()> {
        let state = self
            .state
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        for inner in state.views.iter().filter_map(Weak::upgrade) {
            let mut view = inner
                .view
                .lock()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            change(&mut view.facts);
            memory::normalize(&mut view)?;
            view.bytes = memory::bytes(&view)?;
        }
        Ok(())
    }

    /// Called by the actor after read-transaction cleanup, before it starts the next job.
    pub fn register(self: &Arc<Self>, view: View, expected: u64) -> Result<Lease> {
        let mut state = self
            .state
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if state.preparing
            || expected != state.publication
            || view.facts.installation != state.header.installation
            || view.facts.revision != state.header.revision
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let views = match self.reserve_registration(&mut state, view.bytes) {
            Err(ErrorCode::CapacityExceeded) => {
                state.clear_reuse()?;
                self.reserve_registration(&mut state, view.bytes)?
            }
            result => result?,
        };
        Ok(self.install(&mut state, view, views))
    }

    /// Preflight registry storage and all resident payload before making a copy.
    fn reserve_registration(&self, state: &mut State, extra: usize) -> Result<Vec<Weak<Inner>>> {
        state.views.retain(|view| view.strong_count() > 0);
        if state.views.len() >= self.max_entries {
            return Err(ErrorCode::CapacityExceeded);
        }
        let mut bytes = extra;
        memory::add(&mut bytes, memory::reuse(state.reuse.capacity())?)?;
        for live in state.views.iter().filter_map(Weak::upgrade) {
            bytes = bytes
                .checked_add(
                    live.view
                        .lock()
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?
                        .bytes,
                )
                .ok_or(ErrorCode::CapacityExceeded)?;
        }
        let required = state
            .views
            .len()
            .checked_add(1)
            .ok_or(ErrorCode::CapacityExceeded)?;
        // Reserve metadata on a replacement vector, so an allocator's rounding
        // cannot mutate the registry before its actual capacity has been charged.
        let mut views = Vec::new();
        views
            .try_reserve_exact(required)
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        let accounted_bytes = bytes
            .checked_add(memory::registry(
                state.views.capacity().max(views.capacity()),
            )?)
            .ok_or(ErrorCode::CapacityExceeded)?;
        if accounted_bytes > self.working_bytes {
            return Err(ErrorCode::CapacityExceeded);
        }
        views.extend(state.views.iter().cloned());
        tracing::debug!(
            phase = "register",
            as_of = "checked_owner_operation",
            accounted_bytes,
            "authority capacity observation"
        );
        Ok(views)
    }

    fn install(
        self: &Arc<Self>,
        state: &mut State,
        mut view: View,
        mut views: Vec<Weak<Inner>>,
    ) -> Lease {
        view.publish_header(&state.header, state.publication);
        let inner = Arc::new(Inner {
            view: Mutex::new(view),
        });
        views.push(Arc::downgrade(&inner));
        state.views = views;
        Lease {
            owner: Arc::downgrade(self),
            inner,
        }
    }

    /// Extend one existing owner after its actor-owned dependency read settles.
    /// This does not advance authority or turn facts from an earlier read current.
    pub fn replace(&self, lease: &Lease, mut replacement: View, expected: u64) -> Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !std::ptr::eq(lease.owner.as_ptr(), self)
            || state.preparing
            || expected != state.publication
            || replacement.facts.installation != state.header.installation
            || replacement.facts.revision != state.header.revision
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        // A source may later become a real owner's growing dependency union.
        // Remove its soft alias before every replacement, even if it fits.
        state
            .reuse
            .retain(|entry| !Arc::ptr_eq(&entry.lease.inner, &lease.inner));
        if state.reuse.is_empty() {
            state.clear_reuse()?;
        }
        let bytes = match self.replacement_bytes(&mut state, lease, replacement.bytes) {
            Err(ErrorCode::CapacityExceeded) => {
                state.clear_reuse()?;
                self.replacement_bytes(&mut state, lease, replacement.bytes)?
            }
            result => result?,
        };
        replacement.publish_header(&state.header, state.publication);
        *lease
            .inner
            .view
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)? = replacement;
        drop(state);
        tracing::debug!(
            phase = "replace",
            as_of = "checked_owner_operation",
            accounted_bytes = bytes,
            "authority capacity observation"
        );
        Ok(())
    }

    fn replacement_bytes(
        &self,
        state: &mut State,
        lease: &Lease,
        replacement: usize,
    ) -> Result<usize> {
        state.views.retain(|view| view.strong_count() > 0);
        let mut bytes = state.registry_bytes()?;
        let mut found = false;
        for live in state.views.iter().filter_map(Weak::upgrade) {
            let charge = if Arc::ptr_eq(&live, &lease.inner) {
                found = true;
                replacement
            } else {
                live.view
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?
                    .bytes
            };
            bytes = bytes
                .checked_add(charge)
                .ok_or(ErrorCode::CapacityExceeded)?;
        }
        if !found {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        if bytes > self.working_bytes {
            return Err(ErrorCode::CapacityExceeded);
        }
        Ok(bytes)
    }

    /// Stage every live view before writing durable authority. No directory scan.
    pub fn prepare(
        self: &Arc<Self>,
        header: AuthorityHeader,
        changes: &[Change],
        oauth: Option<(&Metadata, &Metadata)>,
    ) -> Result<Publication> {
        let mut state = self
            .state
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if state.preparing
            || header.installation != state.header.installation
            || header.created_at != state.header.created_at
            || header.revision < state.header.revision
            || state.publication == u64::MAX
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        forecast::unique(changes)?;
        let (predicted, actual_registry, mut views) =
            match self.reserve_publication(&mut state, changes, oauth) {
                Err(ErrorCode::CapacityExceeded) => {
                    state.clear_reuse()?;
                    self.reserve_publication(&mut state, changes, oauth)?
                }
                result => result?,
            };
        let mut bytes = actual_registry;
        for inner in state.views.iter().filter_map(Weak::upgrade) {
            let view = inner
                .view
                .lock()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            let next = if let Some(charge) = forecast::bytes(&view, changes, oauth)? {
                // With distinct changes, a partially updated clone and its
                // in-flight record fit old_view + final_view. Completed staged
                // views fit the rest of the final working set. Thus originals
                // + all final views + this old view fit three working budgets.
                #[cfg(test)]
                self.staged_clones
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mut next = view.clone();
                next.apply(changes, oauth)?;
                if next.bytes > charge {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                Some(next)
            } else {
                None
            };
            bytes = bytes
                .checked_add(next.as_ref().unwrap_or(&view).bytes)
                .ok_or(ErrorCode::CapacityExceeded)?;
            if bytes > predicted {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            drop(view);
            views.push(Staged { inner, next });
        }
        state.preparing = true;
        Ok(Publication {
            owner: self.clone(),
            expected: state.publication,
            header,
            views,
            finished: false,
        })
    }

    fn reserve_publication(
        &self,
        state: &mut State,
        changes: &[Change],
        oauth: Option<(&Metadata, &Metadata)>,
    ) -> Result<(usize, usize, Vec<Staged>)> {
        state.views.retain(|view| view.strong_count() > 0);
        let mut views = Vec::new();
        views
            .try_reserve_exact(state.views.len())
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        let registry = memory::registry(state.views.capacity().max(views.capacity()))?
            .checked_add(memory::reuse(state.reuse.capacity())?)
            .ok_or(ErrorCode::CapacityExceeded)?;
        let mut predicted = registry;
        // Complete the aggregate preflight before cloning any payload, including
        // when a later view is the one that would exceed the working budget.
        for inner in state.views.iter().filter_map(Weak::upgrade) {
            let view = inner
                .view
                .lock()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            memory::add(
                &mut predicted,
                forecast::bytes(&view, changes, oauth)?.unwrap_or(view.bytes),
            )?;
        }
        if predicted > self.working_bytes {
            return Err(ErrorCode::CapacityExceeded);
        }
        Ok((predicted, registry, views))
    }
}

impl Publication {
    /// Only call after a known COMMIT. Lock failure requires the existing fail-closed
    /// transaction quarantine; no success response may be sent on that path.
    pub fn publish(mut self) -> Result<()> {
        let mut state = self
            .owner
            .state
            .write()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if !state.preparing || state.publication != self.expected {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let publication = self
            .expected
            .checked_add(1)
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        for staged in &mut self.views {
            let mut view = staged
                .inner
                .view
                .lock()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?;
            if let Some(next) = staged.next.take() {
                *view = next;
            }
            view.publish_header(&self.header, publication);
        }
        std::mem::swap(&mut state.header, &mut self.header);
        state.publication = publication;
        state.preparing = false;
        self.finished = true;
        Ok(())
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        if !self.finished
            && let Ok(mut state) = self.owner.state.write()
            && state.publication == self.expected
        {
            state.preparing = false;
        }
    }
}
