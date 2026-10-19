//! Controlled SQLite checkpoint images, actual SDK requests and local reads.
//! Source shutdown really joins before its stopped proof is transferred. This
//! is not abrupt-process retirement or private ACP context recovery.
use super::*;
use bitrouter_orchestrator::store as execution;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Semaphore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request};

#[derive(Clone, Copy)]
enum Boundary {
    SettledRead,
    PendingRead,
    BeforeRestore,
}

struct BarrierStore {
    inner: Arc<crate::agent_store::DatabaseExecutionStore>,
    boundary: Boundary,
    result_seen: AtomicBool,
    fired: AtomicBool,
    ready: Semaphore,
    release: Semaphore,
}

fn contains(
    record: &execution::ExecutionRecord,
    predicate: fn(&execution::ExecutionRecord) -> bool,
) -> bool {
    predicate(record)
        || matches!(record, execution::ExecutionRecord::TurnRecord { fact, .. } if contains(fact, predicate))
}

#[async_trait::async_trait]
impl ExecutionStore for BarrierStore {
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> std::result::Result<execution::ExecutionIndexPage, String> {
        self.inner.read_index(after, cutoff, limit, bytes).await
    }
    async fn claim_owner(&self, id: &str) -> std::result::Result<execution::OwnerClaim, String> {
        self.inner.claim_owner(id).await
    }
    async fn read_owner(
        &self,
        id: &str,
    ) -> std::result::Result<Option<execution::ExecutionOwner>, String> {
        self.inner.read_owner(id).await
    }
    async fn stop_owner(
        &self,
        owner: &execution::ExecutionOwner,
    ) -> std::result::Result<execution::ExecutionOwner, String> {
        self.inner.stop_owner(owner).await
    }
    async fn commit_owned(
        &self,
        owner: &execution::ExecutionOwner,
        id: &str,
        expected: u64,
        records: &[execution::ExecutionRecord],
    ) -> std::result::Result<u64, String> {
        let version = self
            .inner
            .commit_owned(owner, id, expected, records)
            .await?;
        if records.iter().any(|record| {
            contains(record, |fact| {
                matches!(fact, execution::ExecutionRecord::ToolResult { .. })
            })
        }) {
            self.result_seen.store(true, Ordering::SeqCst);
        }
        let matched = records.iter().any(|record| {
            contains(
                record,
                match self.boundary {
                    Boundary::SettledRead => |fact| {
                        matches!(
                            fact,
                            execution::ExecutionRecord::RunCheckpoint {
                                model_steps: 1,
                                tool_calls: 1,
                                ..
                            }
                        )
                    },
                    Boundary::PendingRead => {
                        |fact| matches!(fact, execution::ExecutionRecord::ToolIntent { .. })
                    }
                    Boundary::BeforeRestore => |_| false,
                },
            )
        });
        if matched
            && (matches!(self.boundary, Boundary::PendingRead)
                || self.result_seen.load(Ordering::SeqCst))
            && self
                .fired
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            // The complete batch is durable, but Core cannot advance before
            // this existing acknowledgement returns. No source facts are cut.
            self.ready.add_permits(1);
            self.release
                .acquire()
                .await
                .map_err(|error| error.to_string())?
                .forget();
        }
        Ok(version)
    }
    async fn commit(
        &self,
        id: &str,
        expected: u64,
        records: &[execution::ExecutionRecord],
    ) -> std::result::Result<u64, String> {
        self.inner.commit(id, expected, records).await
    }
    async fn load(
        &self,
        id: &str,
    ) -> std::result::Result<Option<execution::StoredExecution>, String> {
        self.inner.load(id).await
    }
    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        bytes: usize,
    ) -> std::result::Result<Option<execution::ExecutionPage>, String> {
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
    ) -> std::result::Result<execution::ThreadHistoryChunk, String> {
        self.inner
            .thread_history(id, after, cutoff, limit, bytes)
            .await
    }
    async fn find_key(
        &self,
        scope: &str,
        key: &str,
    ) -> std::result::Result<Option<execution::AcceptedKey>, String> {
        let entry = self.inner.find_key(scope, key).await?;
        if matches!(self.boundary, Boundary::BeforeRestore)
            && entry.is_some()
            && key == crate::goal_runtime::native::turn_key("original-work")
            && self
                .fired
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            self.ready.add_permits(1);
            self.release
                .acquire()
                .await
                .map_err(|error| error.to_string())?
                .forget();
        }
        Ok(entry)
    }
}

fn model_app(routing: Arc<ConfigRoutingTable>) -> Result<Arc<App>> {
    let executor = Arc::new(bitrouter_sdk::language_model::HttpExecutor::with_defaults()?);
    Ok(Arc::new(
        App::builder()
            .skip_auth(true)
            .language_model(|model| {
                model.routing_table(routing).executor(executor);
            })
            .build()?,
    ))
}

async fn stopped_source(
    source: &Coordinator,
    barrier: &BarrierStore,
    workspace: &Path,
) -> Result<()> {
    let closing = {
        let threads = source.threads.clone();
        tokio::spawn(async move { threads.shutdown().await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match source.threads.register_local_workspace(workspace) {
                Err(error)
                    if error.code == bitrouter_orchestrator::service::ErrorCode::ShuttingDown =>
                {
                    break;
                }
                Err(error) => return Err(anyhow::Error::from(error)),
                Ok(_) => tokio::task::yield_now().await,
            }
        }
        Ok(())
    })
    .await??;
    barrier.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), closing).await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_goal_resumes_only_a_stopped_safe_original_checkpoint_without_rereading()
-> Result<()> {
    for (label, boundary, retired, paused, revoke_after_check) in [
        ("safe", Boundary::SettledRead, true, false, false),
        ("live-owner", Boundary::SettledRead, false, false, false),
        ("pending-read", Boundary::PendingRead, true, false, false),
        ("paused", Boundary::SettledRead, true, true, false),
        (
            "revoked-after-check",
            Boundary::SettledRead,
            true,
            false,
            true,
        ),
    ] {
        let mut home = tempfile::tempdir()?;
        let retained = std::env::var_os("BRO_GOAL_FIXTURE_KEEP_FAILURES").is_some();
        home.disable_cleanup(retained);
        if retained {
            eprintln!("Native recovery fixture {label}: {}", home.path().display());
        }
        let backend = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with({
            let calls=calls.clone();let recorded=recorded.clone();move |request:&Request| {
                calls.fetch_add(1,Ordering::SeqCst);
                let Ok(body)=serde_json::from_slice::<serde_json::Value>(&request.body) else {return wiremock::ResponseTemplate::new(400)};
                if let Ok(mut values)=recorded.lock() {values.push(body.clone());}
                let seen=body["messages"].as_array().is_some_and(|messages|messages.iter().any(|message|message["role"]=="tool"));
                crate::goal_runtime::tests::native::completion(&body,if seen {
                    serde_json::json!({"role":"assistant","content":"The original seed read is retained; no second read was needed."})
                } else {serde_json::json!({"role":"assistant","tool_calls":[{"id":"cold-read","type":"function","function":{"name":"read","arguments":"{\"path\":\"seed.txt\"}"}}]})},if seen {"stop"} else {"tool_calls"})
            }
        }).mount(&backend).await;
        let extra = format!(
            "server: {{skip_auth: true}}\nproviders:\n  fixture:\n    api_base: {}/v1\n    api_key: native-cold-fixture-key\n    api_protocol: [{{'*': chat_completions}}]\n    models: [{{id: test-model}}]\nmodels:\n  cold-model:\n    endpoints: [{{provider: fixture, service_id: test-model}}]\n",
            backend.uri()
        );
        let url = format!(
            "sqlite://{}?mode=rwc",
            home.path().join("source.db").display()
        );
        let (mut source, db) = coordinator_with_config(home.path(), &url, &extra).await?;
        source.threads.shutdown().await;
        source.app = model_app(source.routing.clone())?;
        let barrier = Arc::new(BarrierStore {
            inner: Arc::new(crate::agent_store::DatabaseExecutionStore::new(db.clone())),
            boundary,
            result_seen: AtomicBool::new(false),
            fired: AtomicBool::new(false),
            ready: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        source.threads = ThreadService::with_store(source.app.clone(), &[], barrier.clone())?;
        let mut destination = None;
        let mut receipt_barrier = None;
        let result: Result<()> = async {
            let id = prepared_worker_attempt(
                &source,
                home.path(),
                None,
                Selection {
                    model_route: Some("cold-model".into()),
                    ..Default::default()
                },
            )
            .await?;
            let goal = read(&source, &id).await?;
            let task = &goal.workflow.tasks[0];
            let execution = &task.executions[0];
            tokio::fs::write(
                execution.workspace.join("seed.txt"),
                "ORIGINAL_COLD_EVIDENCE\n",
            )
            .await?;
            source.drive_task(&goal, task).await?;
            tokio::time::timeout(Duration::from_secs(10), barrier.ready.acquire())
                .await??
                .forget();
            let saved = read(&source, &id).await?;
            let original = saved.workflow.tasks[0].executions[0].clone();
            assert_eq!(saved.accounting.provider_attempts, 1);
            let binding = original
                .thread
                .clone()
                .context("original native binding missing")?;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "source advanced beyond the captured boundary: {label}"
            );
            let image = home.path().join("checkpoint.db");
            crate::upgrade_preflight::snapshot(&db, &image).await?;
            let image_url = format!("sqlite://{}?mode=rwc", image.display());
            let (mut resumed, image_db) =
                coordinator_with_config(home.path(), &image_url, &extra).await?;
            resumed.threads.shutdown().await;
            resumed.app = model_app(resumed.routing.clone())?;
            let destination_store = Arc::new(crate::agent_store::DatabaseExecutionStore::new(
                image_db.clone(),
            ));
            if revoke_after_check {
                receipt_barrier = Some(Arc::new(BarrierStore {
                    inner: destination_store.clone(),
                    boundary: Boundary::BeforeRestore,
                    result_seen: AtomicBool::new(false),
                    fired: AtomicBool::new(false),
                    ready: Semaphore::new(0),
                    release: Semaphore::new(0),
                }));
            }
            let store: Arc<dyn ExecutionStore> = match &receipt_barrier {
                Some(gate) => gate.clone(),
                None => destination_store,
            };
            resumed.threads = ThreadService::with_store(resumed.app.clone(), &[], store)?;
            destination = Some((resumed, image_db));
            let (resumed, image_db) = destination.as_ref().context("destination missing")?;
            let source_epoch = source.threads.capabilities().server_instance_id;
            if retired {
                stopped_source(&source, &barrier, &original.workspace).await?;
                let stopped = barrier
                    .read_owner(&source_epoch)
                    .await
                    .map_err(anyhow::Error::msg)?
                    .context("source owner proof missing")?;
                assert!(
                    stopped.stopped_at_ms.is_some(),
                    "source did not join cleanly: {label}"
                );
                assert_eq!(
                    read(&source, &id).await?.accounting.provider_attempts,
                    saved.accounting.provider_attempts,
                    "source admitted another provider attempt after the checkpoint"
                );
                let store = crate::agent_store::DatabaseExecutionStore::new(image_db.clone());
                let mut snapshot_owner = store
                    .read_owner(&source_epoch)
                    .await
                    .map_err(anyhow::Error::msg)?
                    .context("copied owner missing")?;
                assert!(snapshot_owner.stopped_at_ms.is_none());
                snapshot_owner.stopped_at_ms = stopped.stopped_at_ms;
                assert_eq!(
                    snapshot_owner, stopped,
                    "copied owner identity differs from the joined source"
                );
                snapshot_owner.stopped_at_ms = None;
                // Only after actual source join, affirm that same epoch's
                // stopped proof in its checkpoint image. Never retire a live owner.
                store
                    .stop_owner(&snapshot_owner)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            if paused {
                let current = read(resumed, &id).await?;
                resumed
                    .goals
                    .execute(
                        "local",
                        GoalCommand::Control {
                            operation_id: "pause-cold-source".into(),
                            goal_id: id.clone(),
                            expected_revision: current.revision,
                            intent: RunIntent::Pause,
                        },
                        now()?,
                    )
                    .await?;
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            tokio::fs::write(
                original.workspace.join("seed.txt"),
                "CHANGED_AFTER_SOURCE_JOIN\n",
            )
            .await?;
            let before = read(resumed, &id).await?;
            resumed.claim_owner(&before).await?;
            let target = ThreadTarget {
                thread_id: binding.thread_id.clone(),
                server_instance_id: resumed.threads.capabilities().server_instance_id,
            };
            if matches!(boundary, Boundary::SettledRead)
                && retired
                && !paused
                && !revoke_after_check
            {
                let task = &before.workflow.tasks[0];
                resumed.native_workspace(&before, &task.executions[0])?;
                let source =
                    crate::goal_runtime::native::source(&before, task, &task.executions[0])?;
                let control = crate::goal_runtime::model_control::ModelControl::for_worker(
                    resumed,
                    &before,
                    source.clone(),
                );
                let error = resumed
                    .threads
                    .start_goal_worker_turn(
                        &target,
                        &CallerContext::local(),
                        source,
                        bitrouter_orchestrator::turn::TurnRequest {
                            prompt: "changed retry body after restart".into(),
                            idempotency_key: crate::goal_runtime::native::turn_key(&original.id),
                        },
                        control,
                    )
                    .await
                    .err()
                    .context("changed retry body was accepted")?;
                assert_eq!(
                    error.code,
                    bitrouter_orchestrator::service::ErrorCode::Conflict,
                    "changed-body refusal: {}",
                    error.message
                );
                assert!(
                    resumed
                        .threads
                        .read_stored_thread_view(&target, &CallerContext::local())
                        .await?
                        .recovery
                        .is_some(),
                    "changed retry body resumed execution before it was rejected"
                );
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
            if let Some(gate) = &receipt_barrier {
                let revoke = async {
                    let result: Result<()> = async {
                        tokio::time::timeout(Duration::from_secs(5), gate.ready.acquire())
                            .await??
                            .forget();
                        let current = read(resumed, &id).await?;
                        resumed
                            .goals
                            .execute(
                                "local",
                                GoalCommand::Control {
                                    operation_id: "revoke-waiting-restore".into(),
                                    goal_id: id.clone(),
                                    expected_revision: current.revision,
                                    intent: RunIntent::Pause,
                                },
                                now()?,
                            )
                            .await?;
                        Ok(())
                    }
                    .await;
                    gate.release.add_permits(1);
                    result
                };
                let (driven, revoked) = tokio::join!(
                    resumed.drive_task(&before, &before.workflow.tasks[0]),
                    revoke
                );
                driven?;
                revoked?;
                assert!(
                    resumed
                        .threads
                        .read_stored_thread_view(&target, &CallerContext::local())
                        .await?
                        .recovery
                        .is_some(),
                    "source revocation was checked after continuation had already been committed"
                );
            } else {
                resumed
                    .drive_task(&before, &before.workflow.tasks[0])
                    .await?;
            }
            let after = read(resumed, &id).await?;
            assert_eq!(after.workflow.tasks[0].executions.len(), 1);
            assert_eq!(
                after.workflow.tasks[0].executions[0].thread,
                Some(binding.clone())
            );
            assert_eq!(
                after.workflow.owner_thread_id,
                saved.workflow.owner_thread_id
            );
            assert!(!after.workflow.completion.completed_for(1));
            if matches!(boundary, Boundary::SettledRead)
                && retired
                && !paused
                && !revoke_after_check
            {
                assert!(
                    !matches!(
                        after.workflow.tasks[0].executions[0].observation,
                        Observation::Unknown { .. }
                    ),
                    "safe accepted native checkpoint was only projected as unknown"
                );
                let done = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let turn = resumed
                            .threads
                            .read_stored_turn(&target, &CallerContext::local(), &binding.turn_id)
                            .await?;
                        if turn.status == TurnStatus::Completed {
                            return Ok::<_, anyhow::Error>(turn);
                        }
                        anyhow::ensure!(
                            !turn.status.terminal(),
                            "continuation failed: {:?}",
                            turn.detail
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await??;
                assert!(!done.unknown_effect);
                assert_eq!(calls.load(Ordering::SeqCst), 2);
                let resumed_body = {
                    let bodies = recorded
                        .lock()
                        .map_err(|_| anyhow::anyhow!("request capture lock poisoned"))?;
                    bodies
                        .get(1)
                        .context("resumed provider request missing")?
                        .to_string()
                };
                assert!(resumed_body.contains("ORIGINAL_COLD_EVIDENCE"));
                assert!(!resumed_body.contains("CHANGED_AFTER_SOURCE_JOIN"));
                let store = crate::agent_store::DatabaseExecutionStore::new(image_db.clone());
                let records = store
                    .load(&binding.thread_id)
                    .await
                    .map_err(anyhow::Error::msg)?
                    .context("resumed native history missing")?;
                assert_eq!(
                    records
                        .records
                        .iter()
                        .filter(|record| contains(record, |fact| matches!(
                            fact,
                            execution::ExecutionRecord::ToolIntent { .. }
                        )))
                        .count(),
                    1
                );
                let current = read(resumed, &id).await?;
                assert_eq!(
                    current.accounting.provider_attempts, 2,
                    "continuation reset or repeated the Goal provider accounting"
                );
                let task = &current.workflow.tasks[0];
                let source =
                    crate::goal_runtime::native::source(&current, task, &task.executions[0])?;
                let repeated = resumed
                    .threads
                    .resume_goal_worker_turn(
                        &target,
                        &CallerContext::local(),
                        &source,
                        &binding.turn_id,
                        &crate::goal_runtime::native::turn_key(&original.id),
                        crate::goal_runtime::model_control::ModelControl::for_worker(
                            resumed,
                            &current,
                            source.clone(),
                        ),
                    )
                    .await?;
                assert_eq!(repeated.turn_id, binding.turn_id);
                resumed.drive_task(&after, &after.workflow.tasks[0]).await?;
                assert_eq!(calls.load(Ordering::SeqCst), 2);
            } else {
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "unsafe checkpoint dispatched a model: {label}"
                );
                assert!(
                    paused
                        || revoke_after_check
                        || matches!(
                            after.workflow.tasks[0].executions[0].observation,
                            Observation::Unknown { .. }
                        )
                );
                assert_eq!(
                    after.run_intent,
                    if paused || revoke_after_check {
                        RunIntent::Pause
                    } else {
                        RunIntent::Run
                    }
                );
            }
            Ok(())
        }
        .await;
        if let Some(gate) = &receipt_barrier {
            gate.release.add_permits(1);
        }
        stopped_source(&source, &barrier, home.path()).await?;
        if let Some((resumed, image_db)) = destination {
            resumed.threads.shutdown().await;
            resumed.supervisor.shutdown().await;
            image_db.close().await?;
        }
        source.threads.shutdown().await;
        source.supervisor.shutdown().await;
        db.close().await?;
        if result.is_ok() {
            home.disable_cleanup(false);
        }
        result.with_context(|| format!("native checkpoint case {label}"))?;
    }
    Ok(())
}
