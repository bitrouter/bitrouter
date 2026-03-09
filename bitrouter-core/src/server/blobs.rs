use super::{
    errors::ServerResult,
    ids::{AccountId, BlobId},
    pagination::{CursorPage, PageRequest},
    time::Timestamp,
};

#[derive(Debug, Clone)]
pub struct BlobMetadata {
    pub id: BlobId,
    pub account_id: AccountId,
    pub content_type: String,
    pub size_bytes: u64,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone)]
pub struct PutBlobRequest {
    pub account_id: AccountId,
    pub content_type: String,
    pub size_bytes: u64,
}

/// Blob content returned from the store.
#[derive(Debug, Clone)]
pub struct BlobContent {
    pub metadata: BlobMetadata,
    /// Raw blob bytes. A streaming variant using `tokio::io::AsyncRead` can be
    /// introduced later for large payloads.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ObjectBinding {
    pub name: String,
    pub blob_id: BlobId,
    pub content_type: String,
}

#[derive(Debug, Clone)]
pub struct ObjectEntry {
    pub name: String,
    pub blob_id: BlobId,
    pub content_type: String,
    pub size_bytes: u64,
    pub created_at: Timestamp,
}

/// Store for raw blob bytes.
pub trait BlobStore {
    fn put_blob(
        &self,
        request: PutBlobRequest,
        data: Vec<u8>,
    ) -> impl Future<Output = ServerResult<BlobMetadata>> + Send;

    fn get_blob(&self, id: &BlobId) -> impl Future<Output = ServerResult<BlobContent>> + Send;

    fn delete_blob(&self, id: &BlobId) -> impl Future<Output = ServerResult<()>> + Send;
}

/// Catalog for named object bindings.
pub trait ObjectCatalog {
    fn bind_object(
        &self,
        account_id: &AccountId,
        binding: ObjectBinding,
    ) -> impl Future<Output = ServerResult<()>> + Send;

    fn get_object(
        &self,
        account_id: &AccountId,
        name: &str,
    ) -> impl Future<Output = ServerResult<ObjectEntry>> + Send;

    fn list_objects(
        &self,
        account_id: &AccountId,
        page: PageRequest,
    ) -> impl Future<Output = ServerResult<CursorPage<ObjectEntry>>> + Send;

    fn unbind_object(
        &self,
        account_id: &AccountId,
        name: &str,
    ) -> impl Future<Output = ServerResult<()>> + Send;
}
