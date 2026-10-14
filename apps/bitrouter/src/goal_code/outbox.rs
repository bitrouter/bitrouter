//! Persist client uncertainty before sending. This is not a second Goal store.
use super::*;
use tokio::io::AsyncWriteExt;

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    protocol: u32,
    socket: PathBuf,
    pending: Pending,
}

pub(super) struct Outbox {
    root: PathBuf,
    socket: PathBuf,
}

impl Outbox {
    pub(super) fn new(root: PathBuf, socket: PathBuf) -> Self {
        Self { root, socket }
    }
    fn path(&self, pending: &Pending) -> Result<PathBuf> {
        uuid::Uuid::parse_str(pending.id()?)
            .context("client outbox operation ID must be a UUID")?;
        Ok(self.root.join(format!("{}.json", pending.id()?)))
    }
    pub(super) async fn save(&self, pending: &Pending) -> Result<()> {
        tokio::fs::create_dir_all(&self.root).await?;
        let metadata = tokio::fs::symlink_metadata(&self.root).await?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "client outbox must be a real private directory"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let path = self.path(pending)?;
        let saved = Saved {
            protocol: bitrouter_orchestrator::goal::PROTOCOL_VERSION,
            socket: self.socket.clone(),
            pending: Pending {
                command: pending.command.clone(),
                text: pending.text.clone(),
                target: pending.target.clone(),
            },
        };
        let bytes = serde_json::to_vec(&saved)?;
        anyhow::ensure!(
            bytes.len() <= 2 * bitrouter_orchestrator::goal::MAX_RECORD_BYTES,
            "client outbox exceeds byte limit"
        );
        if tokio::fs::try_exists(&path).await? {
            anyhow::ensure!(
                tokio::fs::read(&path).await? == bytes,
                "outbox identity already has different content"
            );
            return Ok(());
        }
        let temporary = self.root.join(format!(".{}", uuid::Uuid::new_v4()));
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary).await?;
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        tokio::fs::rename(temporary, path).await?;
        #[cfg(unix)]
        {
            let directory = self.root.clone();
            tokio::task::spawn_blocking(move || std::fs::File::open(directory)?.sync_all())
                .await??;
        }
        Ok(())
    }
    pub(super) async fn remove(&self, pending: &Pending) -> Result<()> {
        match tokio::fs::remove_file(self.path(pending)?).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    pub(super) async fn recover(&self) -> Result<Vec<Pending>> {
        if !tokio::fs::try_exists(&self.root).await? {
            return Ok(Vec::new());
        }
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut pending = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
            {
                continue;
            }
            let metadata = tokio::fs::symlink_metadata(entry.path()).await?;
            anyhow::ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len()
                        <= (2 * bitrouter_orchestrator::goal::MAX_RECORD_BYTES) as u64,
                "invalid client outbox file"
            );
            let saved: Saved = serde_json::from_slice(&tokio::fs::read(entry.path()).await?)?;
            if saved.socket != self.socket {
                continue;
            }
            anyhow::ensure!(
                saved.protocol == bitrouter_orchestrator::goal::PROTOCOL_VERSION
                    && self.path(&saved.pending)? == entry.path(),
                "outbox protocol/identity changed; no automatic replay"
            );
            pending.push(saved.pending);
            anyhow::ensure!(
                pending.len() <= 128,
                "too many unresolved client admissions"
            );
        }
        Ok(pending)
    }
}
