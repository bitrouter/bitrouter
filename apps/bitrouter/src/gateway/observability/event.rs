//! Per-request storage and queries for typed lifecycle events.
//!
//! Event producers and the per-request bus are application-owned.

use serde::Serialize;
use std::any::{Any, TypeId};
use std::sync::Arc;

/// A typed event retained for request-local lifecycle coordination.
#[derive(Clone)]
struct EventEntry {
    type_id: TypeId,
    typed: Arc<dyn Any + Send + Sync>,
}

/// Per-request, pull-based event bus. Stores every event emitted during the
/// pipeline lifecycle. Used only for in-request hook coordination — it never
/// leaves the process and there is no app-level subscription.
#[derive(Clone, Default)]
pub struct EventBus {
    events: Vec<EventEntry>,
}

impl EventBus {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Emit a typed event without serializing its payload.
    pub fn emit<E: PipelineEvent>(&mut self, event: E) {
        let name = event.event_name();
        tracing::debug!(event = name, "pipeline event emitted");
        self.events.push(EventEntry {
            type_id: TypeId::of::<E>(),
            typed: Arc::new(event),
        });
    }

    /// Whether an event of type `E` has been emitted (compile-time type-safe).
    pub fn has<E: PipelineEvent>(&self) -> bool {
        let id = TypeId::of::<E>();
        self.events.iter().any(|e| e.type_id == id)
    }

    /// The first emitted event of type `E`, if any.
    pub fn get<E: PipelineEvent>(&self) -> Option<&E> {
        let id = TypeId::of::<E>();
        self.events
            .iter()
            .find(|e| e.type_id == id)
            .and_then(|e| e.typed.downcast_ref::<E>())
    }

    /// All emitted events of type `E`, in emission order.
    pub fn get_all<E: PipelineEvent>(&self) -> Vec<&E> {
        let id = TypeId::of::<E>();
        self.events
            .iter()
            .filter(|e| e.type_id == id)
            .filter_map(|e| e.typed.downcast_ref::<E>())
            .collect()
    }

    /// Move every event from `other` into this bus. Used to fold a stage-local
    /// bus (e.g. `StreamContext`'s) back into the request-level bus.
    pub fn merge_from(&mut self, mut other: EventBus) {
        self.events.append(&mut other.events);
    }
}

/// A typed lifecycle event emitted by an implementation or extension.
pub trait PipelineEvent: Serialize + Any + Send + Sync + 'static {
    /// Stable event name used by diagnostics and serialized observations.
    fn event_name(&self) -> &'static str;
}

impl PipelineEvent for bitrouter_ai::routing::RouterRequestIdentity {
    fn event_name(&self) -> &'static str {
        "router.resolved"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Serialize)]
    struct DemoEvent {
        n: u32,
    }
    impl PipelineEvent for DemoEvent {
        fn event_name(&self) -> &'static str {
            "test.demo"
        }
    }

    #[derive(Serialize)]
    struct OtherEvent;
    impl PipelineEvent for OtherEvent {
        fn event_name(&self) -> &'static str {
            "test.other"
        }
    }

    #[test]
    fn emit_and_query_is_type_safe() {
        let mut bus = EventBus::new();
        assert!(!bus.has::<DemoEvent>());
        bus.emit(DemoEvent { n: 7 });
        bus.emit(DemoEvent { n: 9 });
        assert!(bus.has::<DemoEvent>());
        assert!(!bus.has::<OtherEvent>());
        assert_eq!(bus.get::<DemoEvent>().unwrap().n, 7);
        assert_eq!(bus.get_all::<DemoEvent>().len(), 2);
        assert!(bus.get::<OtherEvent>().is_none());
    }
}
