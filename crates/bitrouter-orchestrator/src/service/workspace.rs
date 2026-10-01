//! Cooperating runtimes share an OS lock and a durable exclusion marker outside
//! the model workspace. The marker is not an execution or model-history log.

use super::*;
use crate::store::ExecutionOwner;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};

const MARKER_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct WorkspaceClaim {
    pub format_version: u32,
    pub inspection: bool,
    pub workspace: PathBuf,
    pub lease_id: String,
    pub server_instance_id: String,
    pub generation: u64,
    pub execution_id: String,
    pub acquired_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Marker {
    Active {
        claim: WorkspaceClaim,
    },
    Idle {
        claim: WorkspaceClaim,
        released_at_ms: u64,
    },
}

pub(super) struct WorkspaceFence {
    _lock: File,
    pub(super) marker: PathBuf,
    claim: std::sync::Mutex<WorkspaceClaim>,
    can_finish: std::sync::atomic::AtomicBool,
}

impl WorkspaceFence {
    pub(super) fn acquire(
        workspace: &Path,
        owner: &ExecutionOwner,
        execution_id: &str,
    ) -> Result<Self, ServiceError> {
        if owner.stopped_at_ms.is_some()
            || owner.generation == 0
            || execution_id.is_empty()
            || execution_id.len() > 128
        {
            return Err(
                "active execution owner and bounded execution identity are required".into(),
            );
        }
        let (lock, marker, fresh) = open_lock(workspace)?;
        match read_marker(&marker)? {
            Some(Marker::Idle { claim, .. }) if claim.workspace == workspace => {}
            Some(Marker::Active { claim }) if claim.workspace == workspace => {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "workspace lease {} from {} has no confirmed release",
                        claim.lease_id, claim.server_instance_id
                    ),
                ));
            }
            Some(_) => {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "workspace fence identity is invalid",
                ));
            }
            None if fresh => {}
            None => {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "existing workspace lock has no release marker",
                ));
            }
        }
        let claim = WorkspaceClaim {
            format_version: 1,
            inspection: false,
            workspace: workspace.into(),
            lease_id: uuid::Uuid::new_v4().to_string(),
            server_instance_id: owner.server_instance_id.clone(),
            generation: owner.generation,
            execution_id: execution_id.into(),
            acquired_at_ms: crate::store::owner_time_ms().map_err(storage)?,
        };
        write_marker(
            &marker,
            &Marker::Active {
                claim: claim.clone(),
            },
        )?;
        Ok(Self {
            _lock: lock,
            marker,
            claim: std::sync::Mutex::new(claim),
            can_finish: std::sync::atomic::AtomicBool::new(true),
        })
    }

    /// A cold reader may hold the kernel lock but never manufactures release
    /// proof or execution authority. An existing active marker stays unchanged.
    pub(super) fn inspect(
        workspace: &Path,
        epoch: &str,
        execution_id: &str,
    ) -> Result<Option<Self>, ServiceError> {
        let (lock, marker, _) = match open_lock(workspace) {
            Ok(opened) => opened,
            Err(error) if error.code == ErrorCode::Conflict => return Ok(None),
            Err(error) => return Err(error),
        };
        let claim = match read_marker(&marker)? {
            Some(Marker::Active { claim } | Marker::Idle { claim, .. })
                if claim.workspace == workspace =>
            {
                claim
            }
            Some(_) => {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "workspace fence identity is invalid",
                ));
            }
            None => {
                let claim = WorkspaceClaim {
                    format_version: 1,
                    inspection: true,
                    workspace: workspace.into(),
                    lease_id: uuid::Uuid::new_v4().to_string(),
                    server_instance_id: epoch.into(),
                    generation: 0,
                    execution_id: execution_id.into(),
                    acquired_at_ms: crate::store::owner_time_ms().map_err(storage)?,
                };
                write_marker(
                    &marker,
                    &Marker::Active {
                        claim: claim.clone(),
                    },
                )?;
                claim
            }
        };
        Ok(Some(Self {
            _lock: lock,
            marker,
            claim: std::sync::Mutex::new(claim),
            can_finish: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    pub(super) fn validate(&self) -> Result<(), ServiceError> {
        let claim = self
            .claim
            .lock()
            .map_err(|_| storage("workspace claim lock poisoned"))?;
        if read_marker(&self.marker)?
            != Some(Marker::Active {
                claim: claim.clone(),
            })
        {
            return Err(ServiceError::new(
                ErrorCode::RecoveryRequired,
                "workspace execution fence changed",
            ));
        }
        Ok(())
    }

    /// A confirmed terminal checkpoint may release its inspection reservation,
    /// never an active execution claim. The service retains this kernel guard
    /// until its recovery checkpoint is durably acknowledged.
    pub(super) fn finish_idle_inspection(
        &self,
        owner: &ExecutionOwner,
    ) -> Result<(), ServiceError> {
        let mut current = self
            .claim
            .lock()
            .map_err(|_| storage("workspace claim lock poisoned"))?;
        match read_marker(&self.marker)? {
            Some(Marker::Idle { claim, .. }) if claim == *current => Ok(()),
            Some(Marker::Active { mut claim }) if claim == *current && claim.inspection => {
                if owner.generation == 0 || owner.stopped_at_ms.is_some() {
                    return Err(storage("inspection release needs a current owner"));
                }
                claim.inspection = false;
                claim.generation = owner.generation;
                claim.server_instance_id = owner.server_instance_id.clone();
                write_marker(
                    &self.marker,
                    &Marker::Idle {
                        claim: claim.clone(),
                        released_at_ms: crate::store::owner_time_ms().map_err(storage)?,
                    },
                )?;
                *current = claim;
                Ok(())
            }
            _ => Err(ServiceError::new(
                ErrorCode::RecoveryRequired,
                "active or changed workspace claim requires effect investigation",
            )),
        }
    }

    pub(super) fn activate_recovered_checkpoint(
        &self,
        source: &ExecutionOwner,
        owner: &ExecutionOwner,
        execution_id: &str,
    ) -> Result<(), ServiceError> {
        if source.stopped_at_ms.is_none()
            || owner.stopped_at_ms.is_some()
            || owner.generation <= source.generation
        {
            return Err(storage(
                "checkpoint activation requires a stopped source and newer owner",
            ));
        }
        let mut current = self
            .claim
            .lock()
            .map_err(|_| storage("workspace claim lock poisoned"))?;
        match read_marker(&self.marker)? {
            Some(Marker::Idle { claim, .. }) if claim == *current => {}
            Some(Marker::Active { claim })
                if claim == *current
                    && (claim.inspection
                        || (claim.server_instance_id == source.server_instance_id
                            && claim.generation == source.generation
                            && claim.execution_id == execution_id)) => {}
            _ => {
                return Err(ServiceError::new(
                    ErrorCode::RecoveryRequired,
                    "workspace claim does not match the confirmed checkpoint",
                ));
            }
        }
        let claim = WorkspaceClaim {
            format_version: 1,
            inspection: false,
            workspace: current.workspace.clone(),
            lease_id: uuid::Uuid::new_v4().to_string(),
            server_instance_id: owner.server_instance_id.clone(),
            generation: owner.generation,
            execution_id: execution_id.into(),
            acquired_at_ms: crate::store::owner_time_ms().map_err(storage)?,
        };
        write_marker(
            &self.marker,
            &Marker::Active {
                claim: claim.clone(),
            },
        )?;
        *current = claim;
        self.can_finish
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Called only after joined execution and durable release preparation.
    /// Dropping a fence never manufactures this proof, including unwinding.
    pub(super) fn finish(&self) -> Result<(), ServiceError> {
        if !self.can_finish.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ServiceError::new(
                ErrorCode::RecoveryRequired,
                "inspection cannot release an unresolved workspace",
            ));
        }
        self.validate()?;
        let claim = self
            .claim
            .lock()
            .map_err(|_| storage("workspace claim lock poisoned"))?
            .clone();
        write_marker(
            &self.marker,
            &Marker::Idle {
                claim,
                released_at_ms: crate::store::owner_time_ms().map_err(storage)?,
            },
        )
    }
}

fn paths(workspace: &Path) -> Result<(PathBuf, PathBuf), ServiceError> {
    use sha2::{Digest, Sha256};
    if !workspace.is_absolute()
        || workspace.canonicalize().map_err(storage)? != workspace
        || !workspace.is_dir()
    {
        return Err("workspace fence requires a canonical directory".into());
    }
    let parent = workspace
        .parent()
        .ok_or("workspace requires a writable parent for runtime coordination")?;
    let identity = workspace
        .to_str()
        .ok_or("workspace identity is not UTF-8")?;
    let digest: String = Sha256::digest(identity.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((
        parent.join(format!(".bro-workspace-{digest}.lock")),
        parent.join(format!(".bro-workspace-{digest}.json")),
    ))
}

fn open_lock(workspace: &Path) -> Result<(File, PathBuf, bool), ServiceError> {
    let (lock_path, marker) = paths(workspace)?;
    reject_symlink(&lock_path)?;
    let (lock, fresh) = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(storage)?,
            false,
        ),
        Err(error) => return Err(storage(error)),
    };
    lock.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => {
            ServiceError::new(ErrorCode::Conflict, "another runtime holds this workspace")
        }
        TryLockError::Error(error) => storage(error),
    })?;
    reject_symlink(&lock_path)?;
    if !lock.metadata().map_err(storage)?.is_file() {
        return Err(storage("workspace lock is not a regular file"));
    }
    Ok((lock, marker, fresh))
}

fn reject_symlink(path: &Path) -> Result<(), ServiceError> {
    match path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ServiceError::new(
                ErrorCode::RecoveryRequired,
                "workspace coordination path is not a regular file",
            ))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(storage(error)),
    }
}

fn read_marker(path: &Path) -> Result<Option<Marker>, ServiceError> {
    reject_symlink(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(storage(error)),
    };
    let mut bytes = Vec::new();
    file.take(MARKER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(storage)?;
    if bytes.len() as u64 > MARKER_BYTES {
        return Err(ServiceError::new(
            ErrorCode::RecoveryRequired,
            "workspace marker exceeds its byte bound",
        ));
    }
    let marker: Marker = serde_json::from_slice(&bytes).map_err(|error| {
        ServiceError::new(
            ErrorCode::RecoveryRequired,
            format!("invalid workspace marker: {error}"),
        )
    })?;
    let (claim, idle) = match &marker {
        Marker::Active { claim } => (claim, false),
        Marker::Idle { claim, .. } => (claim, true),
    };
    if claim.format_version != 1
        || claim.lease_id.is_empty()
        || claim.lease_id.len() > 128
        || claim.server_instance_id.is_empty()
        || claim.server_instance_id.len() > 128
        || claim.execution_id.is_empty()
        || claim.execution_id.len() > 128
        || !claim.workspace.is_absolute()
        || (claim.inspection && (claim.generation != 0 || idle))
        || (!claim.inspection && claim.generation == 0)
    {
        return Err(ServiceError::new(
            ErrorCode::RecoveryRequired,
            "workspace marker identity or release proof is invalid",
        ));
    }
    Ok(Some(marker))
}

fn write_marker(path: &Path, marker: &Marker) -> Result<(), ServiceError> {
    reject_symlink(path)?;
    let parent = path.parent().ok_or("workspace marker has no parent")?;
    let bytes = serde_json::to_vec(marker).map_err(storage)?;
    if bytes.len() as u64 > MARKER_BYTES {
        return Err(storage("workspace marker exceeds its byte bound"));
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(storage)?;
    temporary.write_all(&bytes).map_err(storage)?;
    temporary.as_file().sync_all().map_err(storage)?;
    temporary.persist(path).map_err(storage)?;
    Ok(())
}

fn storage(error: impl ToString) -> ServiceError {
    ServiceError::new(ErrorCode::StorageUnavailable, error.to_string())
}

impl TaskService {
    /// Register blocking coordination I/O before shutdown closes and joins the
    /// tracker. A disconnected caller cannot detach an untracked lock acquisition.
    pub(super) async fn workspace_io<T, F>(
        &self,
        allow_closing: bool,
        job: F,
    ) -> Result<T, ServiceError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, ServiceError> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        {
            let state = self.lock_state();
            if state.closing && !allow_closing {
                return Err(ServiceError::new(
                    ErrorCode::ShuttingDown,
                    "runtime is shutting down",
                ));
            }
            let inner = self.inner.clone();
            self.inner.workers.spawn(async move {
                let result = tokio::task::spawn_blocking(job)
                    .await
                    .map_err(storage)
                    .and_then(|r| r);
                if sender.send(result).is_err() {
                    inner
                        .cleanup_unconfirmed
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            });
        }
        receiver.await.map_err(storage)?
    }

    pub(super) fn workspace_owner_error(
        &self,
        state: &State,
        workspace: &Path,
    ) -> Option<ServiceError> {
        if state
            .cold_executions
            .values()
            .any(|entry| entry.blocks(workspace))
        {
            return Some(ServiceError::new(
                ErrorCode::RecoveryRequired,
                "startup discovery found an unresolved cold execution in this workspace",
            ));
        }
        let id = state.active_workspaces.get(workspace)?;
        let known_active = state.tasks.get(id).is_some_and(|task| {
            !task.snapshot.status.terminal()
                && task.snapshot.status != TaskStatus::RecoveryRequired
                && task.storage_error.is_none()
        });
        let recovery = state.threads.values().any(|thread| {
            thread.snapshot.workspace == workspace
                && thread.snapshot.status == ThreadStatus::RecoveryRequired
        });
        Some(if known_active && !recovery {
            ServiceError::new(ErrorCode::Conflict, "another Turn owns this workspace")
        } else {
            ServiceError::new(
                ErrorCode::RecoveryRequired,
                "workspace has an unresolved local execution or inspection owner",
            )
        })
    }

    pub(super) async fn reserve_workspace(
        &self,
        workspace: &Path,
        execution_id: &str,
    ) -> Result<(), ServiceError> {
        let owner = self.initialize_execution().await?;
        // Initialization can install cold blockers after an entrypoint's first
        // capacity check. Recheck before acquiring a fresh execution lease.
        if let Some(error) = self.workspace_owner_error(&self.lock_state(), workspace) {
            return Err(error);
        }
        let path = workspace.to_path_buf();
        let id = execution_id.to_string();
        let fence = Arc::new(
            self.workspace_io(false, move || WorkspaceFence::acquire(&path, &owner, &id))
                .await?,
        );
        let conflict = {
            let mut state = self.lock_state();
            if state.active_workspaces.contains_key(workspace) {
                true
            } else {
                state
                    .active_workspaces
                    .insert(workspace.into(), execution_id.into());
                state
                    .workspace_fences
                    .insert(workspace.into(), fence.clone());
                false
            }
        };
        if conflict {
            // A concurrent cold reader may have installed unresolved evidence.
            // Dropping this guard keeps its active marker, never a false release.
            self.inner
                .cleanup_unconfirmed
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(ServiceError::new(
                ErrorCode::Conflict,
                "workspace acquired a local owner during admission",
            ));
        }
        Ok(())
    }

    pub(super) async fn prepare_workspace_finish(&self, task_id: &str) -> Result<(), ServiceError> {
        let (workspace, fence) = {
            let state = self.lock_state();
            let task = state.tasks.get(task_id).ok_or_else(unknown_task)?;
            let workspace = task.snapshot.workspace.clone();
            if state
                .active_workspaces
                .get(&workspace)
                .is_none_or(|id| id != task_id)
            {
                return Ok(());
            }
            (
                workspace.clone(),
                state
                    .workspace_fences
                    .get(&workspace)
                    .cloned()
                    .ok_or("workspace execution fence missing")?,
            )
        };
        let lease_id = {
            let claim = fence
                .claim
                .lock()
                .map_err(|_| storage("workspace claim lock poisoned"))?;
            claim.lease_id.clone()
        };
        self.commit_serialized(
            task_id,
            &[ExecutionRecord::WorkspaceReleasePrepared {
                workspace,
                execution_id: task_id.into(),
                lease_id,
            }],
        )
        .await?;
        self.workspace_io(true, move || fence.finish()).await?;
        Ok(())
    }

    pub(super) fn workspace_finish_failed(&self, task_id: &str, error: &ServiceError) {
        self.inner
            .cleanup_unconfirmed
            .store(true, std::sync::atomic::Ordering::Release);
        let mut state = self.lock_state();
        let thread_id = if let Some(task) = state.tasks.get_mut(task_id) {
            task.cancel.cancel();
            task.storage_error = Some(error.to_string());
            task.snapshot.status = TaskStatus::RecoveryRequired;
            task.snapshot.detail = Some(format!("workspace release failed: {error}"));
            let _ = task.publisher.send(Observation::Snapshot {
                snapshot: Box::new(task.snapshot.clone()),
                resynchronized: true,
                catchup: Vec::new(),
            });
            task.thread_id.clone()
        } else {
            None
        };
        if let Some(thread_id) = thread_id
            && let Some(thread) = state.threads.get_mut(&thread_id)
        {
            thread.storage_error = Some(error.to_string());
            thread.snapshot.status = ThreadStatus::RecoveryRequired;
            thread.snapshot.pause_reason = Some(error.to_string());
            thread.presentation.blocked(&error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<(tempfile::TempDir, PathBuf), Box<dyn std::error::Error>> {
        let root = tempfile::TempDir::new()?;
        let path = root.path().join("workspace");
        std::fs::create_dir(&path)?;
        Ok((root, path.canonicalize()?))
    }

    fn owner(id: &str) -> ExecutionOwner {
        ExecutionOwner {
            server_instance_id: id.into(),
            generation: 1,
            stopped_at_ms: None,
        }
    }

    fn code<T>(result: Result<T, ServiceError>) -> Result<ErrorCode, Box<dyn std::error::Error>> {
        Ok(result.err().ok_or("unexpected workspace acquisition")?.code)
    }

    #[test]
    fn only_confirmed_release_allows_an_independent_owner() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_root, path) = fixture()?;
        let first = WorkspaceFence::acquire(&path, &owner("one"), "turn-one")?;
        assert_eq!(
            code(WorkspaceFence::acquire(&path, &owner("two"), "turn-two"))?,
            ErrorCode::Conflict
        );
        first.finish()?;
        // Idle is written while the kernel lock is still held.
        assert_eq!(
            code(WorkspaceFence::acquire(&path, &owner("two"), "turn-two"))?,
            ErrorCode::Conflict
        );
        drop(first);
        let second = WorkspaceFence::acquire(&path, &owner("two"), "turn-two")?;
        second.validate()?;
        drop(second);
        assert_eq!(
            code(WorkspaceFence::acquire(
                &path,
                &owner("three"),
                "turn-three"
            ))?,
            ErrorCode::RecoveryRequired
        );
        Ok(())
    }

    #[test]
    fn inspection_preserves_unknown_ownership_and_cannot_release_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, path) = fixture()?;
        let fence = WorkspaceFence::acquire(&path, &owner("lost"), "old-turn")?;
        drop(fence);
        let (_, marker) = paths(&path)?;
        let before = std::fs::read(&marker)?;
        let inspection =
            WorkspaceFence::inspect(&path, "reader", "old-thread")?.ok_or("inspection missing")?;
        assert_eq!(code(inspection.finish())?, ErrorCode::RecoveryRequired);
        assert_eq!(std::fs::read(marker)?, before);
        drop(inspection);
        assert_eq!(
            code(WorkspaceFence::acquire(&path, &owner("new"), "new-turn"))?,
            ErrorCode::RecoveryRequired
        );
        let (_root, fresh) = fixture()?;
        let inspection = WorkspaceFence::inspect(&fresh, "reader", "legacy-thread")?
            .ok_or("inspection missing")?;
        drop(inspection);
        assert_eq!(
            code(WorkspaceFence::acquire(&fresh, &owner("new"), "new-turn"))?,
            ErrorCode::RecoveryRequired
        );
        Ok(())
    }

    #[test]
    fn invalid_missing_and_oversized_markers_never_authorize_execution()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_root, path) = fixture()?;
        let fence = WorkspaceFence::acquire(&path, &owner("old"), "old-turn")?;
        let (_, marker) = paths(&path)?;
        drop(fence);
        for bytes in [b"invalid".to_vec(), vec![b' '; MARKER_BYTES as usize + 1]] {
            std::fs::write(&marker, bytes)?;
            assert_eq!(
                code(WorkspaceFence::acquire(&path, &owner("new"), "new-turn"))?,
                ErrorCode::RecoveryRequired
            );
        }
        std::fs::remove_file(&marker)?;
        assert_eq!(
            code(WorkspaceFence::acquire(&path, &owner("new"), "new-turn"))?,
            ErrorCode::RecoveryRequired
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_coordination_paths_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let (root, path) = fixture()?;
        let (lock, _) = paths(&path)?;
        let other = root.path().join("other");
        std::fs::write(&other, "")?;
        std::os::unix::fs::symlink(other, lock)?;
        assert_eq!(
            code(WorkspaceFence::acquire(&path, &owner("new"), "new-turn"))?,
            ErrorCode::RecoveryRequired
        );
        Ok(())
    }

    // The parent launches this same test binary as a distinct OS process.
    #[test]
    fn workspace_process_fixture() -> Result<(), Box<dyn std::error::Error>> {
        let Some(path) = std::env::var_os("BRO_WORKSPACE_TEST_PATH") else {
            return Ok(());
        };
        let ready = std::env::var_os("BRO_WORKSPACE_TEST_READY").ok_or("ready path missing")?;
        let _fence = WorkspaceFence::acquire(&PathBuf::from(path), &owner("child"), "child-turn")?;
        std::fs::write(PathBuf::from(ready), "ready")?;
        std::io::stdin().read_to_end(&mut Vec::new())?;
        Ok(())
    }

    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn process_loss_releases_kernel_lock_but_never_confirms_effect_cleanup()
    -> Result<(), Box<dyn std::error::Error>> {
        let (root, path) = fixture()?;
        let ready = root.path().join("ready");
        let mut child = Child(
            std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "service::workspace::tests::workspace_process_fixture",
                    "--nocapture",
                ])
                .env("BRO_WORKSPACE_TEST_PATH", &path)
                .env("BRO_WORKSPACE_TEST_READY", &ready)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            if child.0.try_wait()?.is_some() {
                return Err("workspace fixture exited before locking".into());
            }
            if Instant::now() >= deadline {
                return Err("workspace fixture did not acquire its lock".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            code(WorkspaceFence::acquire(
                &path,
                &owner("parent"),
                "parent-turn"
            ))?,
            ErrorCode::Conflict
        );
        child.0.kill()?;
        child.0.wait()?;
        assert_eq!(
            code(WorkspaceFence::acquire(
                &path,
                &owner("parent"),
                "parent-turn"
            ))?,
            ErrorCode::RecoveryRequired
        );
        Ok(())
    }
}
