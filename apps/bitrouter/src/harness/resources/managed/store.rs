//! Backend contract for persisted native managed sessions.

use async_trait::async_trait;
use bitrouter_ai::core::protocol::CoreError;

#[cfg(feature = "core")]
pub(super) mod local;

pub const MAX_STORE_BYTES: usize = crate::storage::checkpoint::MAX_NATIVE_STORE_BYTES;

/// One database row per session. `save` atomically compares the old revision and
/// persists the complete replacement; an ACK may only follow durable success.
/// Implementations bound reads before allocating the stored payload.
#[async_trait]
pub trait NativeStore: Send + Sync {
    async fn load(&self) -> Result<Option<(u64, Vec<u8>)>, CoreError>;
    async fn save(&self, expected_revision: u64, bytes: Vec<u8>) -> Result<(), CoreError>;
}
