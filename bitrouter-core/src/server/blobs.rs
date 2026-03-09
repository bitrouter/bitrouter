use std::future::Future;

use super::{
    errors::Result,
    ids::BlobId,
    pagination::{Page, PaginationRequest},
    time::{RetentionPolicy, Timestamp},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMetadata {
    pub blob_id: BlobId,
    pub content_type: Option<String>,
    pub size_bytes: u64,
    pub checksum: Option<String>,
    pub created_at: Timestamp,
    pub retention: Option<RetentionPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutBlobRequest {
    pub blob_id: Option<BlobId>,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub retention: Option<RetentionPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetBlobRequest {
    pub blob_id: BlobId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteBlobRequest {
    pub blob_id: BlobId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectBinding {
    pub object_id: String,
    pub blob_id: BlobId,
    pub name: String,
    pub metadata: BlobMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindObjectRequest {
    pub object_id: String,
    pub blob_id: BlobId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetObjectRequest {
    pub object_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListObjectsRequest {
    pub pagination: PaginationRequest,
}

pub trait BlobReader {
    fn get_blob_metadata(
        &self,
        request: GetBlobRequest,
    ) -> impl Future<Output = Result<BlobMetadata>> + Send;

    fn get_blob(
        &self,
        request: GetBlobRequest,
    ) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

pub trait BlobWriter {
    fn put_blob(
        &self,
        request: PutBlobRequest,
    ) -> impl Future<Output = Result<BlobMetadata>> + Send;

    fn delete_blob(
        &self,
        request: DeleteBlobRequest,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub trait BlobStore: BlobReader + BlobWriter {}

impl<T> BlobStore for T where T: BlobReader + BlobWriter {}

pub trait ObjectCatalog {
    fn get_object(
        &self,
        request: GetObjectRequest,
    ) -> impl Future<Output = Result<ObjectBinding>> + Send;

    fn bind_object(
        &self,
        request: BindObjectRequest,
    ) -> impl Future<Output = Result<ObjectBinding>> + Send;

    fn list_objects(
        &self,
        request: ListObjectsRequest,
    ) -> impl Future<Output = Result<Page<ObjectBinding>>> + Send;
}
