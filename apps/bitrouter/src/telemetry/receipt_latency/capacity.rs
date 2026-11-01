//! Fixed numeric observation; no admission mutation or retained Service owner.
use std::sync::OnceLock;

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub(crate) mod runtime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    Unavailable,
    Invalid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub completion_reserved: u64,
    pub physical_write_reserved: u64,
    /// Last successful owner-side volume probe, not a new callback probe.
    /// None means no pinned physical volume; never expose a test sentinel.
    pub observed_free: Option<u64>,
    /// Current inline receipt/evidence accounting, excluding archived history.
    pub accounted: u64,
    pub history_bytes: u64,
    pub history_rows: u64,
    /// Startup per-slot maximum durable envelope, unchanged by rotation. Not
    /// actual encoded bytes, allocated filesystem bytes or a new quota.
    pub oauth_reserved: u64,
    pub ingress_reserved: u64,
    pub potential_unknown: u64,
    pub live_unknown: u64,
    pub active: u64,
    pub queued: u64,
    pub ingress: u64,
    pub closed: bool,
    pub fenced: bool,
}
impl Snapshot {
    pub(crate) fn validate(self) -> Result<Self, ReadError> {
        if self
            .bytes()
            .into_iter()
            .chain(self.state())
            .any(|(_, n)| n > i64::MAX as u64)
            // Execution/repair decomposition is checked under the source's
            // Admission mutex. The wire aggregate includes repairs without an
            // active slot and with variable candidate-byte charges.
            || self.potential_unknown.checked_mul(16 * 1024)
                .is_none_or(|minimum| minimum > self.completion_reserved)
            || (self.potential_unknown == 0 && self.completion_reserved != 0)
        {
            return Err(ReadError::Invalid);
        }
        Ok(self)
    }
    fn quiescent(self) -> bool {
        self.closed
            && self.active == 0
            && self.queued == 0
            && self.ingress == 0
            && self.ingress_reserved == 0
            && self.completion_reserved == 0
            && self.physical_write_reserved == 0
            && self.potential_unknown == 0
    }
    pub(crate) fn bytes(self) -> [(&'static str, u64); 6] {
        [
            ("completion_reserved", self.completion_reserved),
            ("physical_write_reserved", self.physical_write_reserved),
            ("accounted", self.accounted),
            ("history_bytes", self.history_bytes),
            ("oauth_reserved", self.oauth_reserved),
            ("ingress_reserved", self.ingress_reserved),
        ]
    }
    pub(crate) fn state(self) -> [(&'static str, u64); 10] {
        [
            ("potential_unknown", self.potential_unknown),
            ("live_unknown", self.live_unknown),
            ("history_rows", self.history_rows),
            ("active", self.active),
            ("queued", self.queued),
            ("ingress", self.ingress),
            ("closed", u64::from(self.closed)),
            ("fenced", u64::from(self.fenced)),
            ("available", 1),
            ("volume_available", u64::from(self.observed_free.is_some())),
        ]
    }
}
pub(crate) type Checkpoint = (&'static str, Result<Snapshot, ReadError>);

type Read = Box<dyn Fn() -> Result<Snapshot, ReadError> + Send + Sync>;
struct Binding {
    initial: Snapshot,
    read: Read,
}
// Per-observation scheduling only: the hook must call the real collection. It
// holds only the test's weak owners and is absent from production builds.
#[cfg(all(test, feature = "otel-http"))]
pub(crate) type CollectHook =
    Box<dyn Fn(&dyn Fn() -> [Option<Checkpoint>; 2]) -> [Option<Checkpoint>; 2] + Send + Sync>;
#[derive(Default)]
pub(crate) struct Observation {
    #[cfg(all(test, feature = "otel-http"))]
    collect_hook: OnceLock<CollectHook>,
    binding: OnceLock<Binding>,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    runtime: OnceLock<Box<dyn Fn() -> runtime::Snapshot + Send + Sync>>,
    final_value: OnceLock<Result<Snapshot, ReadError>>,
}
impl Observation {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(crate) fn bind_runtime(
        &self,
        read: impl Fn() -> runtime::Snapshot + Send + Sync + 'static,
    ) -> bool {
        self.runtime.set(Box::new(read)).is_ok()
    }
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(crate) fn runtime(&self) -> Option<runtime::Snapshot> {
        self.runtime.get().map(|read| read())
    }
    /// The caller supplies a Weak admission closure after real inventory load.
    pub(crate) fn bind(
        &self,
        initial: Snapshot,
        read: impl Fn() -> Result<Snapshot, ReadError> + Send + Sync + 'static,
    ) -> bool {
        let Ok(initial) = initial.validate() else {
            return false;
        };
        self.final_value.get().is_none()
            && self
                .binding
                .set(Binding {
                    initial,
                    read: Box::new(read),
                })
                .is_ok()
    }
    pub(crate) fn seal(&self, producers_clean: bool) -> bool {
        self.final_value
            .get_or_init(|| {
                if !producers_clean {
                    return Err(ReadError::Unavailable);
                }
                let bound = self.binding.get().ok_or(ReadError::Unavailable)?;
                let value = (bound.read)()?.validate()?;
                if !value.quiescent() {
                    return Err(ReadError::Invalid);
                }
                Ok(value)
            })
            .is_ok()
    }
    #[cfg(all(test, feature = "otel-http"))]
    pub(crate) fn schedule_collection_for_test(&self, hook: CollectHook) -> bool {
        self.collect_hook.set(hook).is_ok()
    }
    /// Each callback copies under try_lock and releases it before SDK observation.
    /// A seal between the two callbacks is a legal incomplete collection.
    pub(crate) fn collect(&self) -> [Option<Checkpoint>; 2] {
        #[cfg(all(test, feature = "otel-http"))]
        if let Some(hook) = self.collect_hook.get() {
            return hook(&|| self.collect_unhooked());
        }
        self.collect_unhooked()
    }
    fn collect_unhooked(&self) -> [Option<Checkpoint>; 2] {
        let initial = self.binding.get().map(|b| ("initial", Ok(b.initial)));
        let later = if let Some(value) = self.final_value.get() {
            Some(("final", *value))
        } else {
            self.binding
                .get()
                .map(|b| ("current", (b.read)().and_then(Snapshot::validate)))
        };
        [initial, later]
    }
}

#[cfg(test)]
mod tests;
