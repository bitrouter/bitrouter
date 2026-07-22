//! Session-scoped permission registry.
//!
//! Upstream `session/request_permission` requests arrive on the single
//! [`UpstreamConnection`](crate::up::UpstreamConnection) permission stream, which
//! is **take-once**: only one consumer can drain it. That is a problem for
//! detach/reattach — the down-facing endpoint drains it on the *first* manager
//! connection, so a reattached connection would get an empty stream and never
//! see permission prompts again (they would silently default to Deny).
//!
//! The [`PermissionRegistry`] fixes this by being the **sole** consumer of the
//! upstream stream (the engine spawns one pump into it) and re-exposing the
//! pending set as a **re-subscribable** stream. Each `subscribe` replays every
//! still-unresolved permission first, then streams new ones — so a manager that
//! reattaches immediately sees any permission that was outstanding when it left.
//!
//! Because [`PendingPermission`] carries a **shared once-only resolver**, the
//! registry's clone keeps the upstream resolver alive across a manager detach:
//! dropping a consumer's clone no longer defaults the upstream to Deny. A
//! permission is answered exactly once (first `resolve` wins) and is denied only
//! when the whole session tears down (the registry, and thus the last clone,
//! drops).

use std::collections::{HashSet, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::Stream;
use tokio::sync::watch;

use crate::up::PendingPermission;

/// The session-owned set of outstanding permission requests.
pub struct PermissionRegistry {
    /// Every permission the upstream has asked about and that has not yet torn
    /// down. Resolved entries are filtered on read and reaped on the next
    /// [`insert`](Self::insert), so the vec stays bounded by the live set.
    pending: Mutex<Vec<PendingPermission>>,
    /// Bumped on every [`insert`](Self::insert) so subscribers re-poll the
    /// snapshot. `watch` is level-triggered, so a subscriber never misses a
    /// wake-up even if several inserts land between polls.
    version: watch::Sender<u64>,
}

impl PermissionRegistry {
    /// A fresh, empty registry.
    pub fn new() -> Self {
        let (version, _rx) = watch::channel(0);
        Self {
            pending: Mutex::new(Vec::new()),
            version,
        }
    }

    /// Record a newly-arrived pending permission. Reaps already-resolved entries
    /// first so the set stays bounded, then wakes every subscriber.
    pub fn insert(&self, pending: PendingPermission) {
        if let Ok(mut guard) = self.pending.lock() {
            guard.retain(|p| !p.is_resolved());
            guard.push(pending);
        }
        self.version.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// Clones of the currently-unresolved pending permissions.
    fn snapshot_unresolved(&self) -> Vec<PendingPermission> {
        self.pending
            .lock()
            .map(|guard| guard.iter().filter(|p| !p.is_resolved()).cloned().collect())
            .unwrap_or_default()
    }

    /// A stream that yields every still-unresolved permission exactly once, then
    /// each newly-inserted one, until the registry is dropped. Re-subscribable:
    /// every call yields its own snapshot-then-live stream, which is what makes a
    /// manager reattach see the outstanding set.
    pub fn subscribe(self: &Arc<Self>) -> Pin<Box<dyn Stream<Item = PendingPermission> + Send>> {
        struct SubState {
            reg: Arc<PermissionRegistry>,
            rx: watch::Receiver<u64>,
            /// Request ids this subscriber has already yielded, so a re-poll
            /// after a version bump doesn't re-emit earlier permissions.
            seen: HashSet<String>,
            queue: VecDeque<PendingPermission>,
        }
        let state = SubState {
            reg: Arc::clone(self),
            rx: self.version.subscribe(),
            seen: HashSet::new(),
            queue: VecDeque::new(),
        };
        Box::pin(futures::stream::unfold(state, |mut st| async move {
            loop {
                if st.queue.is_empty() {
                    for pending in st.reg.snapshot_unresolved() {
                        if st.seen.insert(pending.request_id.clone()) {
                            st.queue.push_back(pending);
                        }
                    }
                }
                if let Some(pending) = st.queue.pop_front() {
                    return Some((pending, st));
                }
                // Nothing new to emit; park until an insert bumps the version.
                // `changed()` erroring means the registry (its `version` sender)
                // was dropped — the session is tearing down, so end the stream.
                if st.rx.changed().await.is_err() {
                    return None;
                }
            }
        }))
    }
}

impl Default for PermissionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
        SelectedPermissionOutcome, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };
    use futures::StreamExt;
    use futures::channel::oneshot;

    /// Build a `PendingPermission` plus the receiver the upstream handler would
    /// park on, so tests can assert what outcome (if any) reached the upstream.
    fn pending(
        id: &str,
    ) -> (
        PendingPermission,
        oneshot::Receiver<RequestPermissionOutcome>,
    ) {
        let (tx, rx) = oneshot::channel();
        let opts = vec![PermissionOption::new(
            PermissionOptionId::new("ok"),
            "Allow",
            PermissionOptionKind::AllowOnce,
        )];
        let tool_call = ToolCallUpdate::new(ToolCallId::new(id), ToolCallUpdateFields::new());
        (
            PendingPermission::new(id.to_string(), tool_call, opts, tx),
            rx,
        )
    }

    fn allow(id: &str) -> RequestPermissionOutcome {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(PermissionOptionId::new(
            id,
        )))
    }

    #[tokio::test]
    async fn subscribe_replays_pending_then_streams_new() {
        let reg = Arc::new(PermissionRegistry::new());
        let (p1, _r1) = pending("a");
        reg.insert(p1);

        let mut sub = reg.subscribe();
        // The already-pending "a" is replayed to a fresh subscriber.
        assert_eq!(sub.next().await.expect("a").request_id, "a");

        // A newly-inserted "b" streams live.
        let (p2, _r2) = pending("b");
        reg.insert(p2);
        assert_eq!(sub.next().await.expect("b").request_id, "b");
    }

    #[tokio::test]
    async fn reattach_sees_outstanding_permission() {
        let reg = Arc::new(PermissionRegistry::new());
        let (p, _r) = pending("x");
        reg.insert(p);

        // First subscriber (the "detached" manager) sees it but does not resolve.
        let mut first = reg.subscribe();
        assert_eq!(first.next().await.expect("x").request_id, "x");
        drop(first);

        // A reattached subscriber still sees the outstanding "x".
        let mut second = reg.subscribe();
        assert_eq!(second.next().await.expect("x again").request_id, "x");
    }

    #[tokio::test]
    async fn resolving_a_clone_answers_upstream_and_hides_it() {
        let reg = Arc::new(PermissionRegistry::new());
        let (p, upstream_rx) = pending("y");
        reg.insert(p);

        let mut sub = reg.subscribe();
        let got = sub.next().await.expect("y");
        got.resolve(allow("ok"));

        // The upstream (the handler's receiver) got the exact outcome.
        let outcome = upstream_rx.await.expect("resolved");
        assert!(matches!(outcome, RequestPermissionOutcome::Selected(_)));

        // A reattached subscriber does NOT re-offer the resolved permission.
        let mut fresh = reg.subscribe();
        assert!(
            futures::poll!(fresh.next()).is_pending(),
            "resolved permission must not be replayed"
        );
    }

    #[tokio::test]
    async fn detach_without_resolving_does_not_deny() {
        let reg = Arc::new(PermissionRegistry::new());
        let (p, mut upstream_rx) = pending("z");
        reg.insert(p);

        // A subscriber receives then drops the clone without resolving (detach).
        let mut sub = reg.subscribe();
        let got = sub.next().await.expect("z");
        drop(got);
        drop(sub);

        // The upstream is NOT answered — the registry's clone keeps it alive.
        assert!(
            upstream_rx.try_recv().expect("sender alive").is_none(),
            "a detach must not answer the upstream"
        );

        // Dropping the registry (session teardown) finally releases the resolver.
        drop(reg);
        assert!(
            upstream_rx.await.is_err(),
            "session teardown drops the resolver"
        );
    }
}
