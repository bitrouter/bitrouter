//! Typed pipeline event system (`PipelineEvent` trait + `EventBus`).
//!
//! See design doc 003 §3.4. The core crate defines only the trait and the bus;
//! concrete event types are declared by the plugin crates that emit them.
//!
//! `Serialize` is a mandatory supertrait used as a guardrail: closures, pointers
//! and channel senders are not `Serialize`, so events are structurally forced to
//! be plain data. It also lets the whole bus be dumped to JSON for logging /
//! receipts. The core trait deliberately does **not** require `Deserialize` —
//! JSON is dump-only.

use serde::Serialize;
use std::any::{Any, TypeId};

/// A typed pipeline event. Each event is an independent struct declared by the
/// crate that emits it.
pub trait PipelineEvent: Serialize + Any + Send + Sync + 'static {
    /// Human-readable event name, used for logs and JSON dumps.
    fn event_name(&self) -> &'static str;
}

/// A single stored event: a typed handle plus its pre-serialized JSON.
///
/// `typed` backs the compile-time-safe `has::<E>()` / `get::<E>()` queries;
/// `json` backs `dump_json()` and is always available without re-deriving.
struct EventEntry {
    type_id: TypeId,
    name: &'static str,
    typed: Box<dyn Any + Send + Sync>,
    json: serde_json::Value,
}

/// Per-request, pull-based event bus. Stores every event emitted during the
/// pipeline lifecycle. Used only for in-request hook coordination — it never
/// leaves the process and there is no app-level subscription.
#[derive(Default)]
pub struct EventBus {
    events: Vec<EventEntry>,
}

impl EventBus {
    /// Create an empty bus.
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Emit a typed event. Serialized once, at emit time.
    pub fn emit<E: PipelineEvent>(&mut self, event: E) {
        let json = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
        let name = event.event_name();
        tracing::debug!(event = name, "pipeline event emitted");
        self.events.push(EventEntry {
            type_id: TypeId::of::<E>(),
            name,
            typed: Box::new(event),
            json,
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

    /// Number of events emitted so far.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether no events have been emitted.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Dump all events as a JSON array (logging / debugging / receipt storage).
    /// Type-independent, always available.
    pub fn dump_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.events
                .iter()
                .map(|e| serde_json::json!({ "event": e.name, "data": e.json }))
                .collect(),
        )
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

    #[test]
    fn dump_json_exports_all_events() {
        let mut bus = EventBus::new();
        bus.emit(DemoEvent { n: 1 });
        bus.emit(OtherEvent);
        let dump = bus.dump_json();
        let arr = dump.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["event"], "test.demo");
        assert_eq!(arr[0]["data"]["n"], 1);
        assert_eq!(arr[1]["event"], "test.other");
    }
}
