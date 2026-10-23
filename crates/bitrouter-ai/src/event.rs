//! Shared contract for typed request-lifecycle events.
//!
//! Events carry serializable data; storage, dispatch and observation remain
//! responsibilities of the host. Serialization is required for diagnostics,
//! while deserialization is not required for process-local event producers.

use serde::Serialize;
use std::any::Any;

/// A typed lifecycle event emitted by an implementation or extension.
pub trait PipelineEvent: Serialize + Any + Send + Sync + 'static {
    /// Stable event name used by diagnostics and serialized observations.
    fn event_name(&self) -> &'static str;
}
