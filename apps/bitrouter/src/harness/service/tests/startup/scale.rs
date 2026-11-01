//! Opt-in native/SQLite history qualification. Counts are fixture checkpoints,
//! never product limits; no network, bulk history cloning or padding records.
use super::super::support::{
    app_with_execution_mode, final_turn, input, mock_stream, target, thread_request, tool_call,
    turn,
};
use crate::{
    agent_store::DatabaseExecutionStore,
    gateway::{
        caller::CallerContext,
        error::BitrouterError,
        pipeline::{
            context::PipelineContext,
            executor::{Executor, MockExecutor, MockResponse, StreamPartStream},
            types::{ExecutionResult, RoutingTarget},
        },
    },
    harness::{
        agent::AgentConfigExt,
        service::{ThreadService, default_runtime_limits},
        storage::{
            AcceptedKey, ExecutionIndexPage, ExecutionPage, ExecutionRecord, ExecutionStore,
            StoredExecution, ThreadHistoryChunk,
        },
        thread::ThreadTarget,
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    harness::{
        runtime::{EffectStatus, ErrorCode, ExecutionOwner, OwnerClaim, RuntimeLimits},
        thread::{PermissionProfile, ThreadRecoveryRequest, ThreadSnapshot},
        turn::TurnStatus,
    },
    types::{GenerateResult, Prompt},
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;

const ROOTS: usize = 1026;
const TURNS: usize = 32;
const PAGE: usize = 32;
const PAGE_BYTES: usize = 4 * 1024 * 1024;
const EVIDENCE: &str = "scale original native evidence";
const CHANGED: &str = "changed after native history was committed";

// Three tiny scripts, all serviced locally: two reads then final; final only;
// and one observable write then final. No vector grows with the workload.
struct Script {
    mode: AtomicUsize,
    step: AtomicUsize,
    calls: AtomicUsize,
    expect_context: AtomicBool,
    checked_context: AtomicUsize,
}
impl Script {
    fn new(mode: usize) -> Self {
        Self {
            mode: AtomicUsize::new(mode),
            step: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            expect_context: AtomicBool::new(false),
            checked_context: AtomicUsize::new(0),
        }
    }
    fn set(&self, mode: usize) {
        self.step.store(0, Ordering::SeqCst);
        self.mode.store(mode, Ordering::SeqCst);
    }
    fn response(&self, prompt: &Prompt) -> crate::gateway::error::Result<GenerateResult> {
        if self.expect_context.swap(false, Ordering::SeqCst) {
            let json = serde_json::to_string(prompt)
                .map_err(|error| BitrouterError::internal(error.to_string()))?;
            if !json.contains(EVIDENCE) || json.contains(CHANGED) {
                return Err(BitrouterError::internal(
                    "cold continuation lost original context or reread the file",
                ));
            }
            self.checked_context.fetch_add(1, Ordering::SeqCst);
        }
        let request = self.calls.fetch_add(1, Ordering::SeqCst);
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        Ok(match self.mode.load(Ordering::SeqCst) {
            0 if step % 3 != 2 => turn(vec![tool_call(
                &format!("read-{request}"),
                "read",
                serde_json::json!({"path":"evidence"}),
            )]),
            0 | 1 => final_turn(),
            2 if step.is_multiple_of(2) => turn(vec![tool_call(
                &format!("write-{request}"),
                "write",
                serde_json::json!({"path":"effect.txt", "content":"written once"}),
            )]),
            2 => final_turn(),
            _ => return Err(BitrouterError::internal("invalid scale script")),
        })
    }
}
#[async_trait::async_trait]
impl Executor for Script {
    fn native_protocol_validation(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_ai::core::evidence::NativeProtocolValidation {
        bitrouter_ai::core::evidence::NativeProtocolValidation::Compatible
    }
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        context: &PipelineContext,
    ) -> crate::gateway::error::Result<ExecutionResult> {
        MockExecutor::new(vec![MockResponse::Generate(self.response(prompt)?)])
            .execute(target, prompt, context)
            .await
    }
    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        context: &PipelineContext,
    ) -> crate::gateway::error::Result<StreamPartStream> {
        MockExecutor::new(vec![mock_stream(self.response(prompt)?)])
            .execute_stream(target, prompt, context)
            .await
    }
}

// Production SQLite remains the writer and reader. The only fault is a single
// durable Unknown ToolResult. In native history this also invalidates its Core
// projection digest; this case proves conservative refusal, not a valid Core
// Unknown outcome. It must never be silently repaired by startup or replay.
struct Disk {
    inner: DatabaseExecutionStore,
    armed: AtomicBool,
    injected: AtomicUsize,
    max_page: AtomicUsize,
}
impl Disk {
    fn new(db: DatabaseConnection) -> Self {
        Self {
            inner: DatabaseExecutionStore::new(db),
            armed: AtomicBool::new(false),
            injected: AtomicUsize::new(0),
            max_page: AtomicUsize::new(0),
        }
    }
    fn corrupt(record: &mut ExecutionRecord) -> bool {
        match record {
            ExecutionRecord::TurnRecord { fact, .. } => Self::corrupt(fact),
            ExecutionRecord::ToolResult { effect, .. } => {
                *effect = EffectStatus::Unknown;
                true
            }
            _ => false,
        }
    }
    fn bounded(&self, limit: usize, bytes: usize) -> std::result::Result<(), String> {
        if limit == 0 || limit > PAGE || bytes > PAGE_BYTES {
            return Err("scale read exceeded fixed page envelope".into());
        }
        self.max_page.fetch_max(limit, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait::async_trait]
impl ExecutionStore for Disk {
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> std::result::Result<ExecutionIndexPage, String> {
        self.bounded(limit, bytes)?;
        self.inner.read_index(after, cutoff, limit, bytes).await
    }
    async fn claim_owner(&self, id: &str) -> std::result::Result<OwnerClaim, String> {
        self.inner.claim_owner(id).await
    }
    async fn read_owner(&self, id: &str) -> std::result::Result<Option<ExecutionOwner>, String> {
        self.inner.read_owner(id).await
    }
    async fn stop_owner(
        &self,
        owner: &ExecutionOwner,
    ) -> std::result::Result<ExecutionOwner, String> {
        self.inner.stop_owner(owner).await
    }
    async fn commit_owned(
        &self,
        owner: &ExecutionOwner,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> std::result::Result<u64, String> {
        if self.armed.load(Ordering::SeqCst) {
            let mut changed = records.to_vec();
            if changed.iter_mut().any(Self::corrupt) && self.armed.swap(false, Ordering::SeqCst) {
                let next = self
                    .inner
                    .commit_owned(owner, id, version, &changed)
                    .await?;
                self.injected.fetch_add(1, Ordering::SeqCst);
                return Ok(next);
            }
        }
        self.inner.commit_owned(owner, id, version, records).await
    }
    async fn commit(
        &self,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> std::result::Result<u64, String> {
        self.inner.commit(id, version, records).await
    }
    async fn load(&self, _: &str) -> std::result::Result<Option<StoredExecution>, String> {
        Err("scale qualification forbids unpaged history load".into())
    }
    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> std::result::Result<Option<ExecutionPage>, String> {
        self.bounded(limit, bytes)?;
        self.inner
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
    ) -> std::result::Result<ThreadHistoryChunk, String> {
        self.bounded(limit, bytes)?;
        self.inner
            .thread_history(id, after, cutoff, limit, bytes)
            .await
    }
    async fn find_key(
        &self,
        scope: &str,
        key: &str,
    ) -> std::result::Result<Option<AcceptedKey>, String> {
        self.inner.find_key(scope, key).await
    }
}

fn limits() -> RuntimeLimits {
    RuntimeLimits {
        hot_threads: 1,
        active_turns: 1,
        retained_turns: 2,
        retained_bytes: 256 * 1024,
        hot_context_bytes: 16 * 1024 * 1024,
        startup_roots: 1,
        startup_metadata_bytes: 4096,
        startup_records: PAGE as u64,
        recovery_page_records: PAGE,
        recovery_page_bytes: PAGE_BYTES,
        recovery_readers: 1,
        ..default_runtime_limits()
    }
}
fn service(
    store: Arc<Disk>,
    script: Arc<Script>,
    workspaces: &[std::path::PathBuf],
) -> Result<ThreadService> {
    Ok(ThreadService::with_limits_and_store(
        app_with_execution_mode(script, true)?,
        workspaces,
        limits(),
        store,
    )?)
}
async fn open(url: &str) -> Result<(DatabaseConnection, Arc<Disk>)> {
    let db = crate::db::connect(url).await?;
    Ok((db.clone(), Arc::new(Disk::new(db))))
}
async fn create(
    service: &ThreadService,
    workspace: &TempDir,
    key: &str,
    effects: bool,
) -> Result<ThreadSnapshot> {
    let mut request = thread_request(workspace, key);
    request.permission_profile = if effects {
        PermissionProfile::AllowEffects
    } else {
        PermissionProfile::ReadOnly
    };
    if !effects {
        request.config = request.config.read_only();
    }
    Ok(service
        .create_thread(&service.inner.instance_id, request)
        .await?)
}

#[derive(Debug, Default)]
struct Peaks {
    threads: usize,
    active: usize,
    retained_turns: usize,
    context_bytes: usize,
}
impl Peaks {
    fn observe(&mut self, service: &ThreadService) -> Result<()> {
        let mut state = service.lock_state();
        service.prune(&mut state);
        self.threads = self.threads.max(state.threads.len());
        self.active = self.active.max(state.active_workspaces.len());
        self.retained_turns = self.retained_turns.max(
            state
                .turns
                .values()
                .filter(|turn| turn.terminal_at.is_some())
                .count(),
        );
        let context = state
            .threads
            .values()
            .map(|thread| thread.bytes())
            .sum::<usize>();
        self.context_bytes = self.context_bytes.max(context);
        ensure!(self.threads <= 1 && self.active <= 1 && self.retained_turns <= 2);
        ensure!(context <= service.inner.limits.hot_context_bytes);
        Ok(())
    }
}
async fn completed(
    service: &ThreadService,
    thread: &ThreadTarget,
    key: &str,
    peaks: &mut Peaks,
) -> Result<()> {
    let accepted = service
        .start_turn(
            thread,
            &CallerContext::local(),
            input("Use the evidence", key),
        )
        .await?;
    loop {
        peaks.observe(service)?;
        let (terminal, released) = {
            let state = service.lock_state();
            let saved = state
                .turns
                .get(&accepted.turn_id)
                .context("scale turn missing")?;
            let terminal = saved.snapshot.status.terminal();
            if terminal {
                ensure!(
                    saved.snapshot.status == TurnStatus::Completed,
                    "scale turn failed: {:?}",
                    saved.snapshot.detail
                );
            }
            (
                terminal,
                state.active_workspaces.is_empty() && state.running_turns.is_empty(),
            )
        };
        if terminal && released {
            // Join the particular Thread's last commit before sampling/unloading.
            let gate = service.thread_gate(&thread.thread_id)?;
            let guard = gate.lock().await;
            let released = Arc::strong_count(&gate) == 2;
            drop(guard);
            drop(gate);
            if released {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counts {
    roots: u64,
    records: u64,
}
async fn counts(db: &DatabaseConnection, store: &Disk) -> Result<Counts> {
    let row = db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT (SELECT COUNT(*) FROM bro_executions) AS roots,(SELECT COUNT(*) FROM bro_execution_records) AS records,(SELECT COUNT(*) FROM bro_execution_index) AS indexed,(SELECT COUNT(DISTINCT execution_id) FROM bro_execution_index) AS unique_roots"))
        .await?.context("scale SQL counts")?;
    let result = Counts {
        roots: u64::try_from(row.try_get::<i64>("", "roots")?)?,
        records: u64::try_from(row.try_get::<i64>("", "records")?)?,
    };
    ensure!(
        row.try_get::<i64>("", "indexed")? == i64::try_from(result.roots)?
            && row.try_get::<i64>("", "unique_roots")? == i64::try_from(result.roots)?
    );
    let (mut after, mut cutoff, mut roots, mut versions) = (0, None, 0, 0_u64);
    loop {
        let page = store
            .read_index(after, cutoff, PAGE, PAGE_BYTES)
            .await
            .map_err(anyhow::Error::msg)?;
        ensure!(cutoff.is_none_or(|cutoff| cutoff == page.cutoff) && page.entries.len() <= PAGE);
        cutoff = Some(page.cutoff);
        for head in page.entries {
            ensure!(head.position > after && head.position <= page.cutoff && head.version > 0);
            after = head.position;
            roots += 1;
            versions = versions
                .checked_add(head.version)
                .context("journal count overflow")?;
        }
        match page.next_after {
            Some(next) => ensure!(next == after, "index continuation mismatch"),
            None => break,
        }
    }
    ensure!(
        roots == result.roots && versions == result.records,
        "physical rows/index versions disagree"
    );
    Ok(result)
}
async fn discover(
    service: &ThreadService,
    script: &Script,
    expected: Counts,
    blocked: usize,
) -> Result<()> {
    let calls = script.calls.load(Ordering::SeqCst);
    service.initialize_execution().await?;
    let report = service
        .capabilities()
        .startup_discovery
        .context("startup report")?;
    ensure!(
        report.complete
            && report.writer_fenced
            && report.inspected_roots as u64 == expected.roots
            && report.scanned_records == expected.records
            && report.blocked_workspaces == blocked,
        "incomplete scale discovery: {report:?}"
    );
    let state = service.lock_state();
    ensure!(
        state.cold_executions.len() == blocked
            && state.threads.is_empty()
            && state.turns.is_empty()
    );
    ensure!(
        script.calls.load(Ordering::SeqCst) == calls,
        "startup replayed a model request"
    );
    Ok(())
}

struct Sample {
    id: String,
    messages: [u8; 32],
    version: u64,
}
fn sample(service: &ThreadService, id: &str) -> Result<Sample> {
    let state = service.lock_state();
    let thread = state.threads.get(id).context("sampled hot thread")?;
    ensure!(
        thread.native.is_some(),
        "fixture must execute the native engine"
    );
    Ok(Sample {
        id: id.into(),
        messages: Sha256::digest(serde_json::to_vec(&thread.messages)?).into(),
        version: thread.snapshot.context_version,
    })
}
async fn check_context(
    service: &ThreadService,
    script: &Script,
    saved: &Sample,
    peaks: &mut Peaks,
) -> Result<()> {
    let target = ThreadTarget {
        thread_id: saved.id.clone(),
        server_instance_id: service.inner.instance_id.clone(),
    };
    let loaded = service
        .load_thread(&target, &CallerContext::local())
        .await?;
    let report = loaded.recovery.context("cold recovery report")?;
    ensure!(
        report.context_valid && report.terminal_checkpoint,
        "invalid native recovery: {:?}",
        report.blockers
    );
    service
        .recover_thread(
            &target,
            &CallerContext::local(),
            ThreadRecoveryRequest {
                source_server_instance_id: report.source_server_instance_id,
                source_cursor: report.source_cursor,
                idempotency_key: "scale-recover".into(),
            },
        )
        .await?;
    let restored = sample(service, &saved.id)?;
    ensure!(
        restored.messages == saved.messages && restored.version == saved.version,
        "cold recovery changed native context"
    );
    service
        .resume_queue(&target, &CallerContext::local(), "scale-resume".into())
        .await?;
    script.expect_context.store(true, Ordering::SeqCst);
    completed(service, &target, "cold-continuation", peaks).await?;
    ensure!(!script.expect_context.load(Ordering::SeqCst));
    service
        .unload_thread(&target, &CallerContext::local())
        .await?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Witness {
    digest: [u8; 32],
    records: u64,
    unknown: usize,
}
fn unknown(record: &ExecutionRecord) -> bool {
    match record {
        ExecutionRecord::TurnRecord { fact, .. } => unknown(fact),
        ExecutionRecord::ToolResult { effect, .. } => *effect == EffectStatus::Unknown,
        _ => false,
    }
}
async fn witness(store: &Disk, id: &str) -> Result<Witness> {
    let (mut after, mut cutoff, mut unknowns) = (0, None, 0);
    let mut hash = Sha256::new();
    loop {
        let page = store
            .read_records(id, after, cutoff, PAGE, PAGE_BYTES)
            .await
            .map_err(anyhow::Error::msg)?
            .context("witness root")?;
        ensure!(cutoff.is_none_or(|cutoff| cutoff == page.cutoff));
        cutoff = Some(page.cutoff);
        for record in page.records {
            let bytes = serde_json::to_vec(&record)?;
            hash.update(u64::try_from(bytes.len())?.to_be_bytes());
            hash.update(bytes);
            unknowns += usize::from(unknown(&record));
            after += 1;
        }
        match page.next_after {
            Some(next) => ensure!(next == after),
            None => {
                ensure!(after == page.cutoff);
                break;
            }
        }
    }
    Ok(Witness {
        digest: hash.finalize().into(),
        records: after,
        unknown: unknowns,
    })
}
fn progress(
    phase: &str,
    started: Instant,
    path: &Path,
    counts: Counts,
    peaks: &Peaks,
) -> Result<()> {
    eprintln!(
        "native SQLite scale phase={phase} elapsed={:?} counts={counts:?} db_bytes={} peaks={peaks:?}",
        started.elapsed(),
        std::fs::metadata(path)?.len()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real SQLite/native history scale: over 1025 roots and one million records"]
async fn real_sqlite_native_history_crosses_old_startup_limits_without_replay() -> Result<()> {
    let started = Instant::now();
    let directory = TempDir::new()?;
    let workspace = TempDir::new()?;
    let unrelated = TempDir::new()?;
    std::fs::write(workspace.path().join("evidence"), EVIDENCE)?;
    let path = directory.path().join("native-scale.sqlite");
    let url = format!("sqlite://{}", path.display());
    let workspaces = [
        workspace.path().to_path_buf(),
        unrelated.path().to_path_buf(),
    ];
    let (db, store) = open(&url).await?;
    crate::db::run_migrations(&db).await?;
    let script = Arc::new(Script::new(0));
    let source = service(store.clone(), script.clone(), &workspaces)?;
    let mut peaks = Peaks::default();
    let mut samples = Vec::with_capacity(3);
    for root in 0..ROOTS {
        let thread = create(&source, &workspace, &format!("root-{root}"), false).await?;
        let target = target(&thread);
        for turn in 0..TURNS {
            completed(&source, &target, &format!("turn-{turn}"), &mut peaks).await?;
        }
        if [0, ROOTS / 2, ROOTS - 1].contains(&root) {
            samples.push(sample(&source, &thread.thread_id)?);
        }
        source
            .unload_thread(&target, &CallerContext::local())
            .await?;
        if root % 32 == 0 {
            eprintln!(
                "native SQLite scale generated_roots={} elapsed={:?} peaks={peaks:?}",
                root + 1,
                started.elapsed()
            );
        }
    }
    ensure!(
        script.calls.load(Ordering::SeqCst) == ROOTS * TURNS * 3,
        "fixed native workload did not finish every model/tool step"
    );
    source.shutdown().await;
    let clean = counts(&db, &store).await?;
    ensure!(
        clean.roots == ROOTS as u64 && clean.roots > 1025 && clean.records > 1_000_000,
        "scale checkpoint not reached: {clean:?}"
    );
    progress("clean committed", started, &path, clean, &peaks)?;
    drop(source);
    drop(store);
    drop(script);
    db.close().await?;

    std::fs::write(workspace.path().join("evidence"), CHANGED)?;
    let (db, store) = open(&url).await?;
    ensure!(counts(&db, &store).await? == clean);
    let script = Arc::new(Script::new(1));
    let reader = service(store.clone(), script.clone(), &workspaces)?;
    discover(&reader, &script, clean, 0).await?;
    for sample in &samples {
        check_context(&reader, &script, sample, &mut peaks).await?;
    }
    ensure!(script.checked_context.load(Ordering::SeqCst) == 3);
    // Late membership is strictly after the fully audited clean checkpoint.
    let late = create(&reader, &workspace, "late-unknown", true).await?;
    script.set(2);
    store.armed.store(true, Ordering::SeqCst);
    completed(&reader, &target(&late), "late-write", &mut peaks).await?;
    ensure!(store.injected.load(Ordering::SeqCst) == 1);
    ensure!(std::fs::read_to_string(workspace.path().join("effect.txt"))? == "written once");
    reader.shutdown().await;
    let before = witness(&store, &late.thread_id).await?;
    ensure!(before.unknown == 1);
    let all = counts(&db, &store).await?;
    ensure!(all.roots == clean.roots + 1 && all.records > clean.records);
    progress("late Unknown committed", started, &path, all, &peaks)?;
    drop(reader);
    drop(store);
    drop(script);
    db.close().await?;

    // Replaying the write would replace this sentinel, even without a new model call.
    std::fs::write(workspace.path().join("effect.txt"), "must survive restart")?;
    let (db, store) = open(&url).await?;
    ensure!(counts(&db, &store).await? == all);
    let script = Arc::new(Script::new(1));
    let reader = service(store.clone(), script.clone(), &workspaces)?;
    discover(&reader, &script, all, 1).await?;
    ensure!(
        reader
            .lock_state()
            .cold_executions
            .contains_key(&late.thread_id)
    );
    let denied = create(&reader, &workspace, "must-block", false).await?;
    let refusal = reader
        .start_turn(
            &target(&denied),
            &CallerContext::local(),
            input("must not run", "blocked"),
        )
        .await;
    ensure!(
        refusal
            .err()
            .is_some_and(|error| error.code == ErrorCode::RecoveryRequired)
    );
    reader
        .unload_thread(&target(&denied), &CallerContext::local())
        .await?;
    ensure!(witness(&store, &late.thread_id).await? == before);
    ensure!(
        std::fs::read_to_string(workspace.path().join("effect.txt"))? == "must survive restart"
    );
    ensure!(script.calls.load(Ordering::SeqCst) == 0);
    let fresh = create(&reader, &unrelated, "unrelated", false).await?;
    completed(&reader, &target(&fresh), "fresh", &mut peaks).await?;
    ensure!(script.calls.load(Ordering::SeqCst) == 1);
    reader.shutdown().await;
    ensure!(witness(&store, &late.thread_id).await? == before);
    ensure!(
        std::fs::read_to_string(workspace.path().join("effect.txt"))? == "must survive restart"
    );
    progress(
        "late Unknown blocked; unrelated completed",
        started,
        &path,
        counts(&db, &store).await?,
        &peaks,
    )?;
    ensure!(store.max_page.load(Ordering::SeqCst) == PAGE);
    drop(reader);
    drop(store);
    db.close().await?;
    Ok(())
}
