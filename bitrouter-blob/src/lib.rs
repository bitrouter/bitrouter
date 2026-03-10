//! Concrete [`BlobStore`](bitrouter_core::blob::BlobStore) implementations.
//!
//! Enable backends via feature flags:
//!
//! | Feature | Backend        |
//! |---------|----------------|
//! | `fs`    | Local filesystem |

#[cfg(feature = "fs")]
pub mod fs;
