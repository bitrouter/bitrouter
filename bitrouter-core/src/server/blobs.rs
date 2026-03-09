use std::{future::Future, pin::Pin};

use futures_core::Stream;

use super::{
    errors::Result,
    ids::{BlobId, SessionId},
    pagination::{Page, PaginationRequest},
};

pub type BlobByteStream =
    Pin<Box<dyn Stream<Item = std::io::Result<Vec<u8>>> + Send + Sync + 'static>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMetadata {
    pub blob_id: BlobId,
    pub content_type: String,
    pub byte_len: u64,
    pub checksum: Option<String>,
}

pub struct PutBlobRequest {
    pub content_type: String,
    pub checksum: Option<String>,
    pub bytes: BlobByteStream,
}

pub struct BlobReadResult {
    pub metadata: BlobMetadata,
    pub bytes: BlobByteStream,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectBinding {
    pub session_id: SessionId,
    pub object_name: String,
    pub blob: BlobMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindObjectRequest {
    pub session_id: SessionId,
    pub object_name: String,
    pub blob_id: BlobId,
}

pub trait BlobStore {
    fn put(&self, request: PutBlobRequest) -> impl Future<Output = Result<BlobMetadata>> + Send;
    fn delete(&self, blob_id: BlobId) -> impl Future<Output = Result<()>> + Send;
}

pub trait BlobReader {
    fn get(&self, blob_id: BlobId) -> impl Future<Output = Result<BlobReadResult>> + Send;
}

pub trait ObjectCatalog {
    fn bind(
        &self,
        request: BindObjectRequest,
    ) -> impl Future<Output = Result<ObjectBinding>> + Send;
    fn list_for_session(
        &self,
        session_id: SessionId,
        pagination: PaginationRequest,
    ) -> impl Future<Output = Result<Page<ObjectBinding>>> + Send;
}
