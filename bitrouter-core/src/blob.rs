//! Blob storage abstraction.
//!
//! [`BlobStore`] defines a transport-agnostic interface for storing and
//! retrieving opaque binary objects keyed by string paths. Concrete
//! implementations (filesystem, S3, GCS, …) live in downstream crates
//! behind feature flags.

use dynosaur::dynosaur;

use crate::errors::Result;

/// Metadata about a stored blob.
#[derive(Debug, Clone)]
pub struct BlobMeta {
    /// The key (path) of the blob.
    pub key: String,
    /// Size in bytes, if known.
    pub size: Option<u64>,
}

/// A transport-agnostic blob storage interface.
///
/// Keys are `/`-separated logical paths (e.g. `"sessions/abc/file.png"`).
/// Implementations map these to whatever physical layout they use.
#[dynosaur(pub DynBlobStore = dyn(box) BlobStore)]
pub trait BlobStore: Send + Sync {
    /// Store `data` at `key`, overwriting any existing blob.
    fn put(&self, key: &str, data: Vec<u8>) -> impl Future<Output = Result<()>> + Send;

    /// Retrieve the blob at `key`.
    fn get(&self, key: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;

    /// Delete the blob at `key`. No-op if the key does not exist.
    fn delete(&self, key: &str) -> impl Future<Output = Result<()>> + Send;

    /// Check whether a blob exists at `key`.
    fn exists(&self, key: &str) -> impl Future<Output = Result<bool>> + Send;

    /// List blobs whose keys start with `prefix`.
    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<BlobMeta>>> + Send;
}
