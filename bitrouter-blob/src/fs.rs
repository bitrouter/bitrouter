//! Filesystem-backed blob storage.

use std::path::{Path, PathBuf};

use bitrouter_core::{
    blob::{BlobMeta, BlobStore},
    errors::{BitrouterError, Result},
};

/// A [`BlobStore`] that maps keys to files under a root directory.
///
/// Keys are `/`-separated logical paths. The store creates intermediate
/// directories as needed and refuses to traverse outside the root via
/// path-traversal components (`..`).
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    /// Create a new store rooted at `root`. The directory is created if it
    /// does not exist.
    pub async fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        tokio::fs::create_dir_all(&root)
            .await
            .map_err(|e| BitrouterError::transport(None, format!("blob root: {e}")))?;
        Ok(Self { root })
    }

    /// Resolve a logical key to a filesystem path, rejecting path-traversal.
    fn resolve(&self, key: &str) -> Result<PathBuf> {
        let rel = Path::new(key);
        // Reject absolute paths and `..` components.
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(BitrouterError::invalid_request(
                None,
                format!("invalid blob key: {key}"),
                None,
            ));
        }
        Ok(self.root.join(rel))
    }
}

impl BlobStore for FsBlobStore {
    async fn put(&self, key: &str, data: Vec<u8>) -> Result<()> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| BitrouterError::transport(None, format!("blob put mkdir: {e}")))?;
        }
        tokio::fs::write(&path, &data)
            .await
            .map_err(|e| BitrouterError::transport(None, format!("blob put: {e}")))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let path = self.resolve(key)?;
        tokio::fs::read(&path)
            .await
            .map_err(|e| BitrouterError::transport(None, format!("blob get: {e}")))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.resolve(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(BitrouterError::transport(None, format!("blob delete: {e}"))),
        }
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let path = self.resolve(key)?;
        Ok(path.try_exists().unwrap_or(false))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BlobMeta>> {
        let dir = self.resolve(prefix)?;
        let mut results = Vec::new();
        collect_entries(&self.root, &dir, &mut results).await?;
        Ok(results)
    }
}

/// Recursively walk `dir` and collect blob metadata relative to `root`.
async fn collect_entries(root: &Path, dir: &Path, out: &mut Vec<BlobMeta>) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(BitrouterError::transport(None, format!("blob list: {e}"))),
    };

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| BitrouterError::transport(None, format!("blob list entry: {e}")))?
    {
        let path = entry.path();
        let ft = entry
            .file_type()
            .await
            .map_err(|e| BitrouterError::transport(None, format!("blob list type: {e}")))?;

        if ft.is_dir() {
            Box::pin(collect_entries(root, &path, out)).await?;
        } else if ft.is_file() {
            let key = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let size = entry.metadata().await.ok().map(|m| m.len());
            out.push(BlobMeta { key, size });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_delete_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(tmp.path()).await.unwrap();

        store.put("a/b/test.bin", vec![1u8, 2, 3]).await.unwrap();
        assert!(store.exists("a/b/test.bin").await.unwrap());
        assert_eq!(store.get("a/b/test.bin").await.unwrap(), vec![1u8, 2, 3]);

        let blobs = store.list("a").await.unwrap();
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].key, "a/b/test.bin");

        store.delete("a/b/test.bin").await.unwrap();
        assert!(!store.exists("a/b/test.bin").await.unwrap());
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(tmp.path()).await.unwrap();

        assert!(store.put("../escape", Vec::<u8>::new()).await.is_err());
        assert!(store.get("foo/../../etc/passwd").await.is_err());
    }
}
