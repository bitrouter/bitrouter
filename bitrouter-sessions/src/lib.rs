use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bitrouter_core::server::blobs::{
    BlobContent, BlobMetadata, BlobStore, ObjectBinding, ObjectCatalog, ObjectEntry, PutBlobRequest,
};
use bitrouter_core::server::errors::{ServerError, ServerResult};
use bitrouter_core::server::ids::{AccountId, BlobId, SessionId};
use bitrouter_core::server::pagination::{CursorPage, PageRequest};
use bitrouter_core::server::sessions::{
    CreateSessionRequest, SessionDetail, SessionMutation, SessionQueryService, SessionSummary,
    SessionWriteService,
};
use bitrouter_core::server::time::Timestamp;
use tokio::sync::RwLock;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_id(prefix: &str) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{n:016x}")
}

// ---------------------------------------------------------------------------
// InMemorySessionService
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StoredSession {
    detail: SessionDetail,
}

pub struct InMemorySessionService {
    sessions: RwLock<HashMap<String, StoredSession>>,
}

impl InMemorySessionService {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemorySessionService {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionQueryService for InMemorySessionService {
    async fn get_session(&self, id: &SessionId) -> ServerResult<SessionDetail> {
        let guard = self.sessions.read().await;
        guard
            .get(id.as_str())
            .map(|s| s.detail.clone())
            .ok_or_else(|| ServerError::NotFound {
                entity: "session".into(),
                id: id.to_string(),
            })
    }

    async fn list_sessions(
        &self,
        account_id: &AccountId,
        page: PageRequest,
    ) -> ServerResult<CursorPage<SessionSummary>> {
        let guard = self.sessions.read().await;
        let mut items: Vec<SessionSummary> = guard
            .values()
            .filter(|s| s.detail.summary.account_id == *account_id)
            .map(|s| s.detail.summary.clone())
            .collect();

        // Sort by ID for deterministic cursor pagination.
        items.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

        // Apply cursor: skip items up to and including the cursor.
        if let Some(ref cursor) = page.cursor {
            items.retain(|s| s.id.as_str() > cursor.as_str());
        }

        let limit = page.limit as usize;
        let has_more = items.len() > limit;
        items.truncate(limit);

        let next_cursor = if has_more {
            items.last().map(|s| s.id.as_str().to_owned())
        } else {
            None
        };

        Ok(CursorPage {
            items,
            next_cursor,
            has_more,
        })
    }
}

impl SessionWriteService for InMemorySessionService {
    async fn create_session(&self, request: CreateSessionRequest) -> ServerResult<SessionDetail> {
        let id = SessionId::new(next_id("sess"));
        let now = Timestamp::now();
        let detail = SessionDetail {
            summary: SessionSummary {
                id: id.clone(),
                account_id: request.account_id,
                title: request.title,
                created_at: now,
                updated_at: now,
            },
            content: request.content,
        };
        self.sessions.write().await.insert(
            id.as_str().to_owned(),
            StoredSession {
                detail: detail.clone(),
            },
        );
        Ok(detail)
    }

    async fn update_session(
        &self,
        id: &SessionId,
        mutation: SessionMutation,
    ) -> ServerResult<SessionDetail> {
        let mut guard = self.sessions.write().await;
        let stored = guard
            .get_mut(id.as_str())
            .ok_or_else(|| ServerError::NotFound {
                entity: "session".into(),
                id: id.to_string(),
            })?;

        if let Some(title) = mutation.title {
            stored.detail.summary.title = Some(title);
        }
        if let Some(content) = mutation.content {
            stored.detail.content = content;
        }
        stored.detail.summary.updated_at = Timestamp::now();
        Ok(stored.detail.clone())
    }

    async fn delete_session(&self, id: &SessionId) -> ServerResult<()> {
        let mut guard = self.sessions.write().await;
        guard
            .remove(id.as_str())
            .map(|_| ())
            .ok_or_else(|| ServerError::NotFound {
                entity: "session".into(),
                id: id.to_string(),
            })
    }
}

// ---------------------------------------------------------------------------
// InMemoryBlobStore
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StoredBlob {
    metadata: BlobMetadata,
    data: Vec<u8>,
}

pub struct InMemoryBlobStore {
    blobs: RwLock<HashMap<String, StoredBlob>>,
}

impl InMemoryBlobStore {
    pub fn new() -> Self {
        Self {
            blobs: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryBlobStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BlobStore for InMemoryBlobStore {
    async fn put_blob(&self, request: PutBlobRequest, data: Vec<u8>) -> ServerResult<BlobMetadata> {
        let id = BlobId::new(next_id("blob"));
        let now = Timestamp::now();
        let metadata = BlobMetadata {
            id: id.clone(),
            account_id: request.account_id,
            content_type: request.content_type,
            size_bytes: request.size_bytes,
            created_at: now,
        };
        self.blobs.write().await.insert(
            id.as_str().to_owned(),
            StoredBlob {
                metadata: metadata.clone(),
                data,
            },
        );
        Ok(metadata)
    }

    async fn get_blob(&self, id: &BlobId) -> ServerResult<BlobContent> {
        let guard = self.blobs.read().await;
        guard
            .get(id.as_str())
            .map(|s| BlobContent {
                metadata: s.metadata.clone(),
                data: s.data.clone(),
            })
            .ok_or_else(|| ServerError::NotFound {
                entity: "blob".into(),
                id: id.to_string(),
            })
    }

    async fn delete_blob(&self, id: &BlobId) -> ServerResult<()> {
        let mut guard = self.blobs.write().await;
        guard
            .remove(id.as_str())
            .map(|_| ())
            .ok_or_else(|| ServerError::NotFound {
                entity: "blob".into(),
                id: id.to_string(),
            })
    }
}

// ---------------------------------------------------------------------------
// InMemoryObjectCatalog
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StoredEntry {
    entry: ObjectEntry,
}

pub struct InMemoryObjectCatalog {
    entries: RwLock<HashMap<(String, String), StoredEntry>>,
}

impl InMemoryObjectCatalog {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryObjectCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectCatalog for InMemoryObjectCatalog {
    async fn bind_object(
        &self,
        account_id: &AccountId,
        binding: ObjectBinding,
    ) -> ServerResult<()> {
        let now = Timestamp::now();
        let entry = ObjectEntry {
            name: binding.name.clone(),
            blob_id: binding.blob_id,
            content_type: binding.content_type,
            size_bytes: 0,
            created_at: now,
        };
        let key = (account_id.as_str().to_owned(), binding.name);
        self.entries
            .write()
            .await
            .insert(key, StoredEntry { entry });
        Ok(())
    }

    async fn get_object(&self, account_id: &AccountId, name: &str) -> ServerResult<ObjectEntry> {
        let guard = self.entries.read().await;
        let key = (account_id.as_str().to_owned(), name.to_owned());
        guard
            .get(&key)
            .map(|s| s.entry.clone())
            .ok_or_else(|| ServerError::NotFound {
                entity: "object".into(),
                id: name.to_owned(),
            })
    }

    async fn list_objects(
        &self,
        account_id: &AccountId,
        page: PageRequest,
    ) -> ServerResult<CursorPage<ObjectEntry>> {
        let guard = self.entries.read().await;
        let mut items: Vec<ObjectEntry> = guard
            .iter()
            .filter(|((acct, _), _)| acct == account_id.as_str())
            .map(|(_, s)| s.entry.clone())
            .collect();

        items.sort_by(|a, b| a.name.cmp(&b.name));

        if let Some(ref cursor) = page.cursor {
            items.retain(|e| e.name.as_str() > cursor.as_str());
        }

        let limit = page.limit as usize;
        let has_more = items.len() > limit;
        items.truncate(limit);

        let next_cursor = if has_more {
            items.last().map(|e| e.name.clone())
        } else {
            None
        };

        Ok(CursorPage {
            items,
            next_cursor,
            has_more,
        })
    }

    async fn unbind_object(&self, account_id: &AccountId, name: &str) -> ServerResult<()> {
        let mut guard = self.entries.write().await;
        let key = (account_id.as_str().to_owned(), name.to_owned());
        guard
            .remove(&key)
            .map(|_| ())
            .ok_or_else(|| ServerError::NotFound {
                entity: "object".into(),
                id: name.to_owned(),
            })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Session tests --

    #[tokio::test]
    async fn session_create_and_get() {
        let svc = InMemorySessionService::new();
        let detail = svc
            .create_session(CreateSessionRequest {
                account_id: AccountId::new("acct-1"),
                title: Some("My Session".into()),
                content: serde_json::json!({"messages": []}),
            })
            .await
            .unwrap();

        assert_eq!(detail.summary.title.as_deref(), Some("My Session"));
        assert_eq!(detail.summary.account_id, AccountId::new("acct-1"));

        let fetched = svc.get_session(&detail.summary.id).await.unwrap();
        assert_eq!(fetched.summary.id, detail.summary.id);
        assert_eq!(fetched.content, serde_json::json!({"messages": []}));
    }

    #[tokio::test]
    async fn session_get_not_found() {
        let svc = InMemorySessionService::new();
        let result = svc.get_session(&SessionId::new("nonexistent")).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    #[tokio::test]
    async fn session_list_filters_by_account() {
        let svc = InMemorySessionService::new();
        let acct_a = AccountId::new("acct-a");
        let acct_b = AccountId::new("acct-b");

        svc.create_session(CreateSessionRequest {
            account_id: acct_a.clone(),
            title: Some("A1".into()),
            content: serde_json::json!({}),
        })
        .await
        .unwrap();

        svc.create_session(CreateSessionRequest {
            account_id: acct_b.clone(),
            title: Some("B1".into()),
            content: serde_json::json!({}),
        })
        .await
        .unwrap();

        svc.create_session(CreateSessionRequest {
            account_id: acct_a.clone(),
            title: Some("A2".into()),
            content: serde_json::json!({}),
        })
        .await
        .unwrap();

        let page = svc
            .list_sessions(&acct_a, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.items.iter().all(|s| s.account_id == acct_a));

        let page_b = svc
            .list_sessions(&acct_b, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page_b.items.len(), 1);
    }

    #[tokio::test]
    async fn session_update() {
        let svc = InMemorySessionService::new();
        let detail = svc
            .create_session(CreateSessionRequest {
                account_id: AccountId::new("acct-1"),
                title: Some("Original".into()),
                content: serde_json::json!({"v": 1}),
            })
            .await
            .unwrap();

        let updated = svc
            .update_session(
                &detail.summary.id,
                SessionMutation {
                    title: Some("Updated".into()),
                    content: Some(serde_json::json!({"v": 2})),
                },
            )
            .await
            .unwrap();

        assert_eq!(updated.summary.title.as_deref(), Some("Updated"));
        assert_eq!(updated.content, serde_json::json!({"v": 2}));
        assert!(updated.summary.updated_at >= detail.summary.updated_at);
    }

    #[tokio::test]
    async fn session_update_not_found() {
        let svc = InMemorySessionService::new();
        let result = svc
            .update_session(
                &SessionId::new("ghost"),
                SessionMutation {
                    title: Some("x".into()),
                    content: None,
                },
            )
            .await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    #[tokio::test]
    async fn session_delete() {
        let svc = InMemorySessionService::new();
        let detail = svc
            .create_session(CreateSessionRequest {
                account_id: AccountId::new("acct-1"),
                title: None,
                content: serde_json::json!(null),
            })
            .await
            .unwrap();

        svc.delete_session(&detail.summary.id).await.unwrap();

        let result = svc.get_session(&detail.summary.id).await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    #[tokio::test]
    async fn session_delete_not_found() {
        let svc = InMemorySessionService::new();
        let result = svc.delete_session(&SessionId::new("ghost")).await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    // -- Blob tests --

    #[tokio::test]
    async fn blob_put_and_get() {
        let store = InMemoryBlobStore::new();
        let data = b"hello world".to_vec();
        let meta = store
            .put_blob(
                PutBlobRequest {
                    account_id: AccountId::new("acct-1"),
                    content_type: "text/plain".into(),
                    size_bytes: data.len() as u64,
                },
                data.clone(),
            )
            .await
            .unwrap();

        assert_eq!(meta.content_type, "text/plain");
        assert_eq!(meta.size_bytes, 11);

        let content = store.get_blob(&meta.id).await.unwrap();
        assert_eq!(content.data, data);
        assert_eq!(content.metadata.id, meta.id);
    }

    #[tokio::test]
    async fn blob_get_not_found() {
        let store = InMemoryBlobStore::new();
        let result = store.get_blob(&BlobId::new("nonexistent")).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    #[tokio::test]
    async fn blob_delete() {
        let store = InMemoryBlobStore::new();
        let meta = store
            .put_blob(
                PutBlobRequest {
                    account_id: AccountId::new("acct-1"),
                    content_type: "application/octet-stream".into(),
                    size_bytes: 3,
                },
                vec![1, 2, 3],
            )
            .await
            .unwrap();

        store.delete_blob(&meta.id).await.unwrap();

        let result = store.get_blob(&meta.id).await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    #[tokio::test]
    async fn blob_delete_not_found() {
        let store = InMemoryBlobStore::new();
        let result = store.delete_blob(&BlobId::new("ghost")).await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    // -- ObjectCatalog tests --

    #[tokio::test]
    async fn object_bind_and_get() {
        let catalog = InMemoryObjectCatalog::new();
        let acct = AccountId::new("acct-1");
        catalog
            .bind_object(
                &acct,
                ObjectBinding {
                    name: "my-file.txt".into(),
                    blob_id: BlobId::new("blob-1"),
                    content_type: "text/plain".into(),
                },
            )
            .await
            .unwrap();

        let entry = catalog.get_object(&acct, "my-file.txt").await.unwrap();
        assert_eq!(entry.name, "my-file.txt");
        assert_eq!(entry.blob_id, BlobId::new("blob-1"));
    }

    #[tokio::test]
    async fn object_get_not_found() {
        let catalog = InMemoryObjectCatalog::new();
        let result = catalog
            .get_object(&AccountId::new("acct-1"), "missing")
            .await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    #[tokio::test]
    async fn object_list() {
        let catalog = InMemoryObjectCatalog::new();
        let acct = AccountId::new("acct-1");

        catalog
            .bind_object(
                &acct,
                ObjectBinding {
                    name: "a.txt".into(),
                    blob_id: BlobId::new("blob-a"),
                    content_type: "text/plain".into(),
                },
            )
            .await
            .unwrap();

        catalog
            .bind_object(
                &acct,
                ObjectBinding {
                    name: "b.txt".into(),
                    blob_id: BlobId::new("blob-b"),
                    content_type: "text/plain".into(),
                },
            )
            .await
            .unwrap();

        // Different account; should not appear.
        catalog
            .bind_object(
                &AccountId::new("acct-2"),
                ObjectBinding {
                    name: "c.txt".into(),
                    blob_id: BlobId::new("blob-c"),
                    content_type: "text/plain".into(),
                },
            )
            .await
            .unwrap();

        let page = catalog
            .list_objects(&acct, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].name, "a.txt");
        assert_eq!(page.items[1].name, "b.txt");
    }

    #[tokio::test]
    async fn object_unbind() {
        let catalog = InMemoryObjectCatalog::new();
        let acct = AccountId::new("acct-1");

        catalog
            .bind_object(
                &acct,
                ObjectBinding {
                    name: "doomed.txt".into(),
                    blob_id: BlobId::new("blob-d"),
                    content_type: "text/plain".into(),
                },
            )
            .await
            .unwrap();

        catalog.unbind_object(&acct, "doomed.txt").await.unwrap();

        let result = catalog.get_object(&acct, "doomed.txt").await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    #[tokio::test]
    async fn object_unbind_not_found() {
        let catalog = InMemoryObjectCatalog::new();
        let result = catalog
            .unbind_object(&AccountId::new("acct-1"), "nope")
            .await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }
}
