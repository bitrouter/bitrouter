//! Actual native execution process loss with a test-only durable journal image.
//! This verifies fail-closed recovery, not SQLite or forced owner retirement.

use super::tests::{app, final_turn, tool_call, turn};
use super::thread_tests::{input, target, thread_request};
use super::*;
use crate::store::{
    AcceptedKey, ExecutionOwner, ExecutionPage, StoredExecution, ThreadHistoryChunk,
};
use crate::thread::{ThreadRecoveryRequest, ThreadTarget, WorkspaceGrant};
use std::io::Write;
use tempfile::TempDir;

#[derive(Serialize, Deserialize)]
struct JournalImage {
    owner: ExecutionOwner,
    execution: StoredExecution,
}

struct CrashStore {
    memory: MemoryExecutionStore,
    image: PathBuf,
    ready: PathBuf,
    point: String,
}

fn fault_matches(record: &ExecutionRecord, point: &str) -> bool {
    match record {
        ExecutionRecord::TurnRecord { fact, .. } => fault_matches(fact, point),
        ExecutionRecord::TurnActivated { .. } => point == "activation",
        ExecutionRecord::ModelRequest { .. } => point == "request",
        ExecutionRecord::ToolIntent { call, .. } => {
            point == "intent" && call.origin == CallOrigin::Model
        }
        ExecutionRecord::ToolResult { .. } => point == "result",
        ExecutionRecord::RunCheckpoint {
            model_steps: 1,
            tool_calls: 1,
            ..
        } => point == "checkpoint",
        ExecutionRecord::Settled { .. } => point == "settled",
        ExecutionRecord::VerificationResult { .. } => point == "verification",
        _ => false,
    }
}

#[async_trait::async_trait]
impl ExecutionStore for CrashStore {
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> Result<crate::store::ExecutionIndexPage, String> {
        self.memory.read_index(after, cutoff, limit, bytes).await
    }
    async fn claim_owner(&self, id: &str) -> Result<crate::store::OwnerClaim, String> {
        self.memory.claim_owner(id).await
    }
    async fn read_owner(&self, id: &str) -> Result<Option<ExecutionOwner>, String> {
        self.memory.read_owner(id).await
    }
    async fn stop_owner(&self, owner: &ExecutionOwner) -> Result<ExecutionOwner, String> {
        self.memory.stop_owner(owner).await
    }
    async fn commit_owned(
        &self,
        owner: &ExecutionOwner,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        let version = self
            .memory
            .commit_owned(owner, id, version, records)
            .await?;
        let execution = self
            .memory
            .load(id)
            .await?
            .ok_or("native execution missing")?;
        let image = JournalImage {
            owner: owner.clone(),
            execution,
        };
        let parent = self.image.parent().ok_or("journal parent missing")?;
        let mut file =
            tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
        serde_json::to_writer(&mut file, &image).map_err(|error| error.to_string())?;
        file.flush().map_err(|error| error.to_string())?;
        file.as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        file.persist(&self.image)
            .map_err(|error| error.to_string())?;
        if records
            .iter()
            .any(|record| fault_matches(record, &self.point))
        {
            std::fs::write(&self.ready, "committed").map_err(|error| error.to_string())?;
            // Leave the actual service and workspace lock alive until the parent
            // kills the process; neither shutdown nor stopped proof can run.
            std::future::pending::<()>().await;
        }
        Ok(version)
    }
    async fn commit(
        &self,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.memory.commit(id, version, records).await
    }
    async fn load(&self, id: &str) -> Result<Option<StoredExecution>, String> {
        self.memory.load(id).await
    }
    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> Result<Option<ExecutionPage>, String> {
        self.memory
            .read_records(id, after, cutoff, limit, bytes)
            .await
    }
    async fn thread_history(
        &self,
        id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        bytes: usize,
    ) -> Result<ThreadHistoryChunk, String> {
        self.memory
            .thread_history(id, after, cutoff, limit, bytes)
            .await
    }
    async fn find_key(&self, scope: &str, key: &str) -> Result<Option<AcceptedKey>, String> {
        self.memory.find_key(scope, key).await
    }
}

#[tokio::test]
async fn native_crash_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let Some(root) = std::env::var_os("BRO_NATIVE_CRASH_ROOT") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    let point = std::env::var("BRO_NATIVE_CRASH_POINT")?;
    let workspace = root.join("workspace");
    let grants = [WorkspaceGrant {
        workspace: workspace.clone(),
        permission_profiles: vec![PermissionProfile::AllowEffects],
    }];
    let store = Arc::new(CrashStore {
        memory: MemoryExecutionStore::default(),
        image: root.join("journal.json"),
        ready: root.join("ready"),
        point: point.clone(),
    });
    let service = TaskService::with_workspace_grants(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"effect", "content":"confirmed once"}),
            )]),
            final_turn(),
        ])?,
        &grants,
        store,
    )?;
    let fixture = TempDir::new()?;
    let mut definition = thread_request(&fixture, "process-thread");
    definition.workspace = workspace;
    definition.permission_profile = PermissionProfile::AllowEffects;
    if point == "verification" {
        definition.verification_command = Some("printf verified >> verification-counter".into());
    }
    let thread = service
        .create_thread(&service.inner.instance_id, definition)
        .await?;
    service
        .start_turn(
            &target(&thread),
            &CallerContext::local(),
            input("write evidence", "process-turn"),
        )
        .await?;
    std::future::pending::<()>().await;
    Ok(())
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn native_process_loss_at_commit_windows_never_retires_owner_or_replays_work()
-> Result<(), Box<dyn std::error::Error>> {
    for point in [
        "activation",
        "request",
        "intent",
        "result",
        "checkpoint",
        "settled",
        "verification",
    ] {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let mut child = Child(
            std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "service::process_recovery_tests::native_crash_fixture",
                    "--nocapture",
                ])
                .env("BRO_NATIVE_CRASH_ROOT", root.path())
                .env("BRO_NATIVE_CRASH_POINT", point)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !root.path().join("ready").exists() {
            if let Some(status) = child.0.try_wait()? {
                return Err(format!("native fixture {point} exited before fault: {status}").into());
            }
            if Instant::now() >= deadline {
                return Err(
                    format!("native fixture {point} did not reach its commit window").into(),
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        child.0.kill()?;
        child.0.wait()?;
        let image: JournalImage =
            serde_json::from_slice(&std::fs::read(root.path().join("journal.json"))?)?;
        assert!(image.owner.stopped_at_ms.is_none());
        let effect_before = std::fs::read_to_string(workspace.join("effect")).ok();
        let verification_before =
            std::fs::read_to_string(workspace.join("verification-counter")).ok();
        assert_eq!(
            effect_before.as_deref(),
            matches!(point, "result" | "checkpoint" | "settled" | "verification")
                .then_some("confirmed once")
        );
        assert_eq!(
            verification_before.as_deref(),
            (point == "verification").then_some("verified")
        );
        // Reopen the test journal image with the original active owner. This
        // deliberately does not manufacture a stopped proof after waitpid.
        let memory = Arc::new(MemoryExecutionStore::default());
        let crate::store::OwnerClaim::Acquired { owner } =
            memory.claim_owner(&image.owner.server_instance_id).await?
        else {
            return Err("journal owner restore failed".into());
        };
        assert_eq!(owner, image.owner);
        memory
            .commit_owned(
                &owner,
                &image.execution.execution_id,
                0,
                &image.execution.records,
            )
            .await?;
        let grants = [WorkspaceGrant {
            workspace: workspace.clone(),
            permission_profiles: vec![PermissionProfile::AllowEffects],
        }];
        let destination =
            TaskService::with_workspace_grants(app(vec![])?, &grants, memory.clone())?;
        let target = ThreadTarget {
            thread_id: image.execution.execution_id.clone(),
            server_instance_id: destination.inner.instance_id.clone(),
        };
        let view = destination
            .load_thread(&target, &CallerContext::local())
            .await?;
        let report = view
            .recovery
            .as_ref()
            .ok_or("crashed execution report missing")?;
        assert_eq!(view.thread.status, ThreadStatus::RecoveryRequired);
        assert_eq!(
            destination
                .recover_thread(
                    &target,
                    &CallerContext::local(),
                    ThreadRecoveryRequest {
                        source_server_instance_id: report.source_server_instance_id.clone(),
                        source_cursor: report.source_cursor,
                        idempotency_key: "crash-recovery".into(),
                    }
                )
                .await
                .err()
                .ok_or("crashed owner resumed")?
                .code,
            ErrorCode::RecoveryRequired
        );
        assert_eq!(
            memory
                .load(&image.execution.execution_id)
                .await?
                .ok_or("journal disappeared")?
                .version,
            image.execution.version
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("effect")).ok(),
            effect_before
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("verification-counter")).ok(),
            verification_before
        );
        destination.shutdown().await;
        assert_eq!(
            memory.read_owner(&owner.server_instance_id).await?,
            Some(owner)
        );
    }
    Ok(())
}
