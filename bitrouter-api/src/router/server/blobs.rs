//! Warp filters for blob storage and object catalog endpoints.

use std::sync::Arc;

use bitrouter_core::server::{
    blobs::{BlobContent, BlobMetadata, BlobStore, ObjectBinding, ObjectCatalog, ObjectEntry},
    ids::{AccountId, BlobId},
    pagination::{CursorPage, PageRequest},
};
use serde::{Deserialize, Serialize};
use warp::Filter;

use crate::error::ServerRejection;

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UploadBlobQuery {
    account_id: String,
    content_type: String,
}

#[derive(Serialize)]
struct BlobMetadataResponse {
    id: String,
    account_id: String,
    content_type: String,
    size_bytes: u64,
    created_at: i64,
}

#[derive(Deserialize)]
struct BindObjectBody {
    account_id: String,
    name: String,
    blob_id: String,
    content_type: String,
}

#[derive(Deserialize)]
struct ObjectGetQuery {
    account_id: String,
}

#[derive(Deserialize)]
struct ObjectListQuery {
    account_id: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    20
}

#[derive(Serialize)]
struct ObjectEntryResponse {
    name: String,
    blob_id: String,
    content_type: String,
    size_bytes: u64,
    created_at: i64,
}

#[derive(Serialize)]
struct PageResponse<T: Serialize> {
    items: Vec<T>,
    next_cursor: Option<String>,
    has_more: bool,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn blob_metadata_to_response(m: BlobMetadata) -> BlobMetadataResponse {
    BlobMetadataResponse {
        id: m.id.to_string(),
        account_id: m.account_id.to_string(),
        content_type: m.content_type,
        size_bytes: m.size_bytes,
        created_at: m.created_at.as_secs(),
    }
}

fn object_entry_to_response(e: ObjectEntry) -> ObjectEntryResponse {
    ObjectEntryResponse {
        name: e.name,
        blob_id: e.blob_id.to_string(),
        content_type: e.content_type,
        size_bytes: e.size_bytes,
        created_at: e.created_at.as_secs(),
    }
}

// ---------------------------------------------------------------------------
// Blob filters
// ---------------------------------------------------------------------------

/// POST /v1/blobs?account_id=...&content_type=... — upload a blob.
///
/// The request body is the raw blob bytes.
pub fn upload_blob_filter<B>(
    store: Arc<B>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    B: BlobStore + Send + Sync + 'static,
{
    warp::path!("v1" / "blobs")
        .and(warp::post())
        .and(warp::query::<UploadBlobQuery>())
        .and(warp::body::bytes().map(|b: bytes::Bytes| b.to_vec()))
        .and(warp::any().map(move || store.clone()))
        .and_then(handle_upload_blob)
}

/// GET /v1/blobs/:id — download a blob (returns raw bytes).
pub fn get_blob_filter<B>(
    store: Arc<B>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    B: BlobStore + Send + Sync + 'static,
{
    warp::path!("v1" / "blobs" / String)
        .and(warp::get())
        .and(warp::any().map(move || store.clone()))
        .and_then(handle_get_blob)
}

/// DELETE /v1/blobs/:id — delete a blob.
pub fn delete_blob_filter<B>(
    store: Arc<B>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    B: BlobStore + Send + Sync + 'static,
{
    warp::path!("v1" / "blobs" / String)
        .and(warp::delete())
        .and(warp::any().map(move || store.clone()))
        .and_then(handle_delete_blob)
}

// ---------------------------------------------------------------------------
// Object catalog filters
// ---------------------------------------------------------------------------

/// POST /v1/objects/bind — bind an object name to a blob.
pub fn bind_object_filter<C>(
    catalog: Arc<C>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    C: ObjectCatalog + Send + Sync + 'static,
{
    warp::path!("v1" / "objects" / "bind")
        .and(warp::post())
        .and(warp::body::json::<BindObjectBody>())
        .and(warp::any().map(move || catalog.clone()))
        .and_then(handle_bind_object)
}

/// GET /v1/objects/:name?account_id=... — get a named object.
pub fn get_object_filter<C>(
    catalog: Arc<C>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    C: ObjectCatalog + Send + Sync + 'static,
{
    warp::path!("v1" / "objects" / String)
        .and(warp::get())
        .and(warp::query::<ObjectGetQuery>())
        .and(warp::any().map(move || catalog.clone()))
        .and_then(handle_get_object)
}

/// GET /v1/objects?account_id=...&cursor=...&limit=... — list objects.
pub fn list_objects_filter<C>(
    catalog: Arc<C>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    C: ObjectCatalog + Send + Sync + 'static,
{
    warp::path!("v1" / "objects")
        .and(warp::get())
        .and(warp::query::<ObjectListQuery>())
        .and(warp::any().map(move || catalog.clone()))
        .and_then(handle_list_objects)
}

// ---------------------------------------------------------------------------
// Blob handlers
// ---------------------------------------------------------------------------

async fn handle_upload_blob<B>(
    query: UploadBlobQuery,
    body: Vec<u8>,
    store: Arc<B>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    B: BlobStore + Send + Sync + 'static,
{
    let data = body;
    let size_bytes = data.len() as u64;

    let metadata = store
        .put_blob(
            bitrouter_core::server::blobs::PutBlobRequest {
                account_id: AccountId::new(query.account_id),
                content_type: query.content_type,
                size_bytes,
            },
            data,
        )
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&blob_metadata_to_response(metadata)),
        warp::http::StatusCode::CREATED,
    ))
}

async fn handle_get_blob<B>(
    id: String,
    store: Arc<B>,
) -> Result<Box<dyn warp::Reply>, warp::Rejection>
where
    B: BlobStore + Send + Sync + 'static,
{
    let BlobContent { metadata, data } = store
        .get_blob(&BlobId::new(id))
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    let response = warp::http::Response::builder()
        .header("content-type", metadata.content_type)
        .body(data)
        .expect("response builder should not fail");

    Ok(Box::new(response))
}

async fn handle_delete_blob<B>(
    id: String,
    store: Arc<B>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    B: BlobStore + Send + Sync + 'static,
{
    store
        .delete_blob(&BlobId::new(id))
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({})),
        warp::http::StatusCode::NO_CONTENT,
    ))
}

// ---------------------------------------------------------------------------
// Object catalog handlers
// ---------------------------------------------------------------------------

async fn handle_bind_object<C>(
    body: BindObjectBody,
    catalog: Arc<C>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    C: ObjectCatalog + Send + Sync + 'static,
{
    catalog
        .bind_object(
            &AccountId::new(body.account_id),
            ObjectBinding {
                name: body.name,
                blob_id: BlobId::new(body.blob_id),
                content_type: body.content_type,
            },
        )
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({})),
        warp::http::StatusCode::CREATED,
    ))
}

async fn handle_get_object<C>(
    name: String,
    query: ObjectGetQuery,
    catalog: Arc<C>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    C: ObjectCatalog + Send + Sync + 'static,
{
    let entry = catalog
        .get_object(&AccountId::new(query.account_id), &name)
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::json(&object_entry_to_response(entry)))
}

async fn handle_list_objects<C>(
    query: ObjectListQuery,
    catalog: Arc<C>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    C: ObjectCatalog + Send + Sync + 'static,
{
    let page = catalog
        .list_objects(
            &AccountId::new(query.account_id),
            PageRequest {
                cursor: query.cursor,
                limit: query.limit,
            },
        )
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::json(&cursor_page_response(page)))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn cursor_page_response(page: CursorPage<ObjectEntry>) -> PageResponse<ObjectEntryResponse> {
    PageResponse {
        items: page
            .items
            .into_iter()
            .map(object_entry_to_response)
            .collect(),
        next_cursor: page.next_cursor,
        has_more: page.has_more,
    }
}
