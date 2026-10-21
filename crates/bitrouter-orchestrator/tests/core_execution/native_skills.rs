use super::*;
use bitrouter_ai::types::Tool;
use bitrouter_orchestrator::core::context_router::FEATURE;
use bitrouter_orchestrator::core::skills::{
    SkillCatalog, SkillConstraints, SkillSelector, SkillSource,
};
use bitrouter_orchestrator::harness::skills::library::{LocalScope, LocalSkill};
use bitrouter_sdk::decision_model::types::{
    Answer, DecisionError, DecisionRequest, DecisionResponse, DecisionUsage,
};
use bitrouter_sdk::decision_model::{DecisionExecutor, DecisionRuntime};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

#[path = "native_skills/files.rs"]
mod files;

fn root_selection(
    snapshot: &SessionSnapshot,
) -> Option<&bitrouter_orchestrator::core::skills::SkillSelection> {
    snapshot.skill_selection(&snapshot.root_turn()?.agent_turn_id)
}

#[tokio::test]
async fn initial_context_budget_excludes_optional_roots_and_fails_required_roots_before_reads()
-> TestResult {
    let baseline =
        Arc::new(NativeSkills::new(Arc::new(Harness::new(None, None)), "CONTEXT_ROOT").await?);
    let (calibration, prompts, _) =
        setup(baseline, vec![output(vec![text("baseline")])], false).await?;
    calibration
        .start(
            "baseline-root",
            calibration.head().await.state_revision,
            input(),
        )
        .await?;
    calibration.drive().await?;
    let limit = serde_json::to_vec(&prompts.prompts.lock().await[0])?.len() as u64 + 128;
    for required in [false, true] {
        let host =
            Arc::new(NativeSkills::new(Arc::new(Harness::new(None, None)), "CONTEXT_ROOT").await?);
        let (session, executor, _) =
            setup(host.clone(), vec![output(vec![text("without root")])], true).await?;
        let mut task = input();
        task.context_limit_bytes = Some(limit);
        if required {
            task.routing.skills = Some(SkillConstraints {
                include: vec![SkillSelector::Local {
                    scope: LocalScope::Project,
                    directory_id: "installation_review".into(),
                }],
                exclude: vec![],
            });
        }
        session
            .start("bounded-root", session.head().await.state_revision, task)
            .await?;
        let snapshot = session.drive().await?;
        assert_eq!(host.reads.load(Ordering::SeqCst), 0);
        if required {
            assert_eq!(
                snapshot.run.as_ref().map(|run| run.status),
                Some(RunStatus::Failed)
            );
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(
                snapshot.run.as_ref().map(|run| run.status),
                Some(RunStatus::Completed)
            );
            let selection = root_selection(&snapshot).ok_or("selection")?;
            assert!(!selection.candidates[0].selected);
            assert_eq!(selection.candidates[0].reason, "not_explicitly_selected");
            assert!(
                !serde_json::to_string(&executor.prompts.lock().await[0])?.contains("CONTEXT_ROOT")
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn same_named_local_and_two_mcp_sources_keep_separate_consent_and_model_provenance()
-> TestResult {
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "REMOTE_SAME_NAME").await?,
    );
    let alpha = host.catalog.lock().await.mcp.skills[0].clone();
    let mut beta = alpha.clone();
    beta.origin.server = "second_source".into();
    beta.origin.configuration_digest = sha256(b"second source");
    beta.validate()?;
    let (local, body) = binding("LOCAL_SAME_NAME");
    host.bytes.lock().await.insert(local.identity()?, body);
    let remote = host
        .bytes
        .lock()
        .await
        .get(&SkillSource::Mcp(alpha.clone()).identity()?)
        .cloned()
        .ok_or("remote bytes")?;
    host.bytes
        .lock()
        .await
        .insert(SkillSource::Mcp(beta.clone()).identity()?, remote);
    if let SkillSource::Local(binding) = &local {
        host.catalog.lock().await.local.skills.push(binding.clone());
    }
    host.catalog.lock().await.mcp.skills.push(beta.clone());
    let (session, executor, _) =
        setup(host.clone(), vec![output(vec![text("done")])], false).await?;
    let mut task = required_mcp_task();
    task.routing
        .skills
        .as_mut()
        .ok_or("constraints")?
        .include
        .extend([local.selector(), SkillSource::Mcp(beta.clone()).selector()]);
    session
        .start("three-sources", session.head().await.state_revision, task)
        .await?;
    session.drive().await?;
    let first = host
        .consent_requests
        .lock()
        .await
        .last()
        .cloned()
        .ok_or("first source approval")?;
    session
        .answer_skill_consent("first-source-approval", first.answer(true))
        .await?;
    session.drive().await?;
    let second = host
        .consent_requests
        .lock()
        .await
        .last()
        .cloned()
        .ok_or("second source approval")?;
    assert_ne!(first.consent.skill_id, second.consent.skill_id);
    assert_ne!(first.source.origin.server, second.source.origin.server);
    assert_eq!(first.source.entry.uri, second.source.entry.uri);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    session
        .answer_skill_consent("second-source-approval", second.answer(true))
        .await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let selection = root_selection(&snapshot).ok_or("selection")?;
    let ids = selection
        .candidates
        .iter()
        .map(|candidate| candidate.source.identity())
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    assert_eq!(ids.len(), 3);
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 1);
    let system = prompts[0].system.as_deref().ok_or("system")?;
    assert!(system.contains("LOCAL_SAME_NAME") && !system.contains("REMOTE_SAME_NAME"));
    for source in [SkillSource::Mcp(alpha), SkillSource::Mcp(beta)] {
        let id = source.identity()?;
        assert!(
            snapshot
                .context_store
                .evidence
                .values()
                .any(|block| block.protected && block.source.skill_binding.as_ref() == Some(&id))
        );
    }
    assert_eq!(host.reads.load(Ordering::SeqCst), 3);
    Ok(())
}

struct NativeSkills {
    durable: Arc<Harness>,
    catalog: Mutex<SkillCatalog>,
    bytes: Mutex<BTreeMap<String, Vec<u8>>>,
    reads: AtomicUsize,
    consent_requests: Mutex<Vec<bitrouter_orchestrator::core::skills::SkillConsentRequest>>,
    read_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    read_seen: tokio::sync::Semaphore,
    read_token: Mutex<Option<CancellationToken>>,
    direct: Mutex<Vec<bitrouter_orchestrator::harness::skills::mcp::HeldMcpSkill>>,
    metadata_reads: AtomicUsize,
    file_reads: AtomicUsize,
    supporting_bytes: Mutex<Vec<u8>>,
    file_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    file_seen: tokio::sync::Semaphore,
    file_token: Mutex<Option<CancellationToken>>,
}

fn binding(marker: &str) -> (SkillSource, Vec<u8>) {
    let body =
        format!("---\nname: review\ndescription: review workflow\n---\n{marker}\n").into_bytes();
    let digest = sha256(&body);
    let source = SkillSource::Local(LocalSkill {
        scope: LocalScope::Project,
        directory: "/fixture/skills/review".into(),
        workspace: "/fixture".into(),
        api_key_id: "local".into(),
        user_id: "local".into(),
        local: true,
        directory_id: "installation_review".into(),
        name: "review".into(),
        description: "review workflow".into(),
        version: format!("bundle-{marker}"),
        receipt: json!({"fixture":marker}),
        instruction_sha256: digest,
        instruction_bytes: body.len() as u64,
    });
    (source, body)
}

impl NativeSkills {
    async fn new(durable: Arc<Harness>, marker: &str) -> Result<Self, CoreError> {
        let result = Self {
            durable,
            catalog: Default::default(),
            bytes: Default::default(),
            reads: AtomicUsize::new(0),
            consent_requests: Default::default(),
            read_gate: Default::default(),
            read_seen: tokio::sync::Semaphore::new(0),
            read_token: Default::default(),
            direct: Default::default(),
            metadata_reads: AtomicUsize::new(0),
            file_reads: AtomicUsize::new(0),
            supporting_bytes: Mutex::new(b"support bytes\n".to_vec()),
            file_gate: Default::default(),
            file_seen: tokio::sync::Semaphore::new(0),
            file_token: Default::default(),
        };
        result.publish(marker).await?;
        Ok(result)
    }

    async fn publish(&self, marker: &str) -> Result<SkillSource, CoreError> {
        let (source, bytes) = binding(marker);
        self.bytes.lock().await.insert(source.identity()?, bytes);
        let SkillSource::Local(skill) = &source else {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointConflict,
                "fixture source kind",
            ));
        };
        self.catalog.lock().await.local.skills = vec![skill.clone()];
        Ok(source)
    }

    async fn corrupt(&self, source: &SkillSource) -> Result<(), CoreError> {
        let mut files = self.bytes.lock().await;
        let bytes = files.get_mut(&source.identity()?).ok_or_else(|| {
            CoreError::rejected(ErrorCode::ArtifactUnavailable, "fixture bytes absent")
        })?;
        let last = bytes.last_mut().ok_or_else(|| {
            CoreError::rejected(ErrorCode::ArtifactUnavailable, "fixture bytes empty")
        })?;
        *last = b'!';
        Ok(())
    }

    async fn with_mcp(
        durable: Arc<Harness>,
        marker: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_mcp_file(durable, marker, b"support bytes\n").await
    }

    async fn with_mcp_file(
        durable: Arc<Harness>,
        marker: &str,
        supporting: &[u8],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_mcp_file_at(
            durable,
            marker,
            "skill://review/references/check.md",
            supporting,
        )
        .await
    }

    async fn with_mcp_file_at(
        durable: Arc<Harness>,
        marker: &str,
        reference: &str,
        supporting: &[u8],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        use bitrouter_orchestrator::harness::skills::mcp::{HeldMcpSkill, SkillOrigin, SkillScope};
        let host = Self::new(durable, marker).await?;
        let body = format!(
            "---\nname: review\ndescription: review workflow\nallowed-tools: shell\n---\n{marker}\n"
        )
        .into_bytes();
        let entry: bitrouter_sdk::mcp::skills::SkillEntry = serde_json::from_value(json!({
            "uri":"skill://review/SKILL.md", "frontmatter":{"name":"review","description":"review workflow","allowed-tools":"shell"},
            "resources":[
                {"uri":"skill://review/SKILL.md","digest":format!("sha256:{}",sha256(&body)),"size":body.len()},
                {"uri":reference,"digest":format!("sha256:{}",sha256(supporting)),"size":supporting.len()}
            ], "fixtureExtension":{"retained":true},
        }))?;
        fn canonical(value: serde_json::Value) -> serde_json::Value {
            match value {
                serde_json::Value::Object(values) => serde_json::Value::Object(
                    values
                        .into_iter()
                        .collect::<BTreeMap<_, _>>()
                        .into_iter()
                        .map(|(key, value)| (key, canonical(value)))
                        .collect(),
                ),
                serde_json::Value::Array(values) => {
                    serde_json::Value::Array(values.into_iter().map(canonical).collect())
                }
                value => value,
            }
        }
        let entry_digest = sha256(&serde_json::to_vec(&canonical(serde_json::to_value(
            &entry,
        )?))?);
        let held = HeldMcpSkill {
            origin: SkillOrigin {
                server: "configured_source".into(),
                configuration_digest: sha256(b"fixture configuration"),
                scope: SkillScope {
                    workspace: "/fixture".into(),
                    api_key_id: "local".into(),
                    user_id: "local".into(),
                    local: true,
                },
            },
            entry,
            entry_digest,
        };
        held.validate()?;
        let source = SkillSource::Mcp(held.clone());
        *host.supporting_bytes.lock().await = supporting.to_vec();
        host.bytes.lock().await.insert(source.identity()?, body);
        let mut catalog = host.catalog.lock().await;
        catalog.local.skills.clear();
        catalog.mcp.skills = vec![held];
        drop(catalog);
        Ok(host)
    }
}

#[async_trait]
impl HarnessPort for NativeSkills {
    async fn skill_file(
        &self,
        source: &SkillSource,
        reference: &str,
        max_bytes: u64,
        cached_bytes: Option<&[u8]>,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, CoreError> {
        let file = source.file(reference)?;
        if source.root_file(&file.reference) {
            return self
                .skill_instructions(source, max_bytes, cached_bytes, cancel)
                .await;
        }
        if cancel.is_cancelled() {
            return Err(CoreError::rejected(
                ErrorCode::ArtifactUnavailable,
                "file read cancelled",
            ));
        }
        let bytes = match cached_bytes {
            Some(bytes) => bytes.to_vec(),
            None => {
                self.file_reads.fetch_add(1, Ordering::SeqCst);
                if let Some(gate) = self.file_gate.lock().await.clone() {
                    *self.file_token.lock().await = Some(cancel.clone());
                    self.file_seen.add_permits(1);
                    tokio::select! {
                        () = cancel.cancelled() => return Err(CoreError::rejected(ErrorCode::ArtifactUnavailable, "support read cancelled")),
                        permit = gate.acquire() => permit.map_err(|error| CoreError::rejected(ErrorCode::ArtifactUnavailable, error.to_string()))?.forget(),
                    }
                }
                self.supporting_bytes.lock().await.clone()
            }
        };
        if bytes.len() as u64 > max_bytes {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "file fixture budget",
            ));
        }
        Ok(bytes)
    }
    async fn skill_catalog(&self) -> Result<Option<SkillCatalog>, CoreError> {
        Ok(Some(self.catalog.lock().await.clone()))
    }
    async fn resolve_mcp_skill(
        &self,
        server: &str,
        uri: &str,
        cancel: &CancellationToken,
    ) -> Result<bitrouter_orchestrator::harness::skills::mcp::HeldMcpSkill, CoreError> {
        self.metadata_reads.fetch_add(1, Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(CoreError::rejected(
                ErrorCode::ArtifactUnavailable,
                "metadata cancelled",
            ));
        }
        self.direct
            .lock()
            .await
            .iter()
            .find(|held| held.origin.server == server && held.entry.uri == uri)
            .cloned()
            .ok_or_else(|| {
                CoreError::rejected(ErrorCode::ArtifactUnavailable, "explicit URI unavailable")
            })
    }
    async fn skill_instructions(
        &self,
        source: &SkillSource,
        max_bytes: u64,
        cached_bytes: Option<&[u8]>,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>, CoreError> {
        // The original acceptance, selection and complete source must already
        // be acknowledged before the first content fetch.
        let store = self.durable.store.lock().await;
        let payload = store
            .batches
            .last()
            .ok_or_else(|| CoreError::rejected(ErrorCode::CheckpointUnavailable, "no checkpoint"))?
            .decode(&store.limits)?;
        let state: SessionSnapshot =
            serde_json::from_value(payload.checkpoint.state).map_err(|error| {
                CoreError::rejected(ErrorCode::CheckpointConflict, error.to_string())
            })?;
        if !state
            .context_store
            .work
            .values()
            .filter_map(|work| work.skills.as_ref())
            .any(|selection| {
                selection.selected
                    && selection.candidates.iter().any(|candidate| {
                        candidate.selected
                            && candidate.source == *source
                            && (matches!(source, SkillSource::Local(_))
                                || candidate.consents.last().is_some_and(|consent| {
                                    consent.approved == Some(true) && !consent.revoked
                                }))
                    })
            })
        {
            return Err(CoreError::rejected(
                ErrorCode::CheckpointUnavailable,
                "content was fetched before durable selection",
            ));
        }
        drop(store);
        if cached_bytes.is_none() {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(gate) = self.read_gate.lock().await.clone() {
            *self.read_token.lock().await = Some(cancel.clone());
            self.read_seen.add_permits(1);
            tokio::select! {
                () = cancel.cancelled() => return Err(CoreError::rejected(ErrorCode::ArtifactUnavailable, "fixture read cancelled")),
                permit = gate.acquire() => permit.map_err(|error| CoreError::rejected(ErrorCode::ArtifactUnavailable, error.to_string()))?.forget(),
            }
        }
        let bytes = if let Some(bytes) = cached_bytes {
            bytes.to_vec()
        } else {
            self.bytes
                .lock()
                .await
                .get(&source.identity()?)
                .cloned()
                .ok_or_else(|| {
                    CoreError::rejected(
                        ErrorCode::ArtifactUnavailable,
                        "original bytes are unavailable",
                    )
                })?
        };
        if bytes.len() as u64 > max_bytes {
            return Err(CoreError::rejected(
                ErrorCode::LimitExceeded,
                "fixture read budget",
            ));
        }
        Ok(bytes)
    }
    async fn authorize_dispatch(&self) -> Result<(), CoreError> {
        self.durable.authorize_dispatch().await
    }
    async fn commit(&self, batch: CheckpointBatch) -> Result<CheckpointAck, CoreError> {
        self.durable.commit(batch).await
    }
    async fn send(&self, message: ServerMessage) -> Result<(), CoreError> {
        if let ServerMessage::SkillConsentRequest(request) = message {
            self.consent_requests.lock().await.push(*request);
            return Ok(());
        }
        self.durable.send(message).await
    }
    async fn read_artifact(
        &self,
        reference: &ArtifactRef,
        offset: u64,
        max_bytes: u64,
    ) -> Result<Vec<u8>, CoreError> {
        self.durable
            .read_artifact(reference, offset, max_bytes)
            .await
    }
}

struct Relevant {
    calls: AtomicUsize,
    requests: Mutex<Vec<DecisionRequest>>,
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    seen: tokio::sync::Semaphore,
}

#[async_trait]
impl DecisionExecutor for Relevant {
    async fn execute(
        &self,
        request: &DecisionRequest,
        _: &CancellationToken,
    ) -> Result<DecisionResponse, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().await.push(request.clone());
        self.seen.add_permits(1);
        let gate = self.gate.lock().await.clone();
        if let Some(gate) = gate {
            gate.acquire()
                .await
                .map_err(|error| DecisionError {
                    kind: bitrouter_sdk::decision_model::types::DecisionFailure::Cancelled,
                    message: error.to_string(),
                    usage: None,
                    may_have_run: true,
                })?
                .forget();
        }
        Ok(DecisionResponse {
            model: request.model.clone(),
            answers: request
                .questions
                .keys()
                .map(|id| {
                    (
                        id.clone(),
                        Answer::Noul {
                            noul: if request.state["task"].as_str() == Some("child-a") {
                                0.01
                            } else {
                                0.99
                            },
                        },
                    )
                })
                .collect(),
            usage: DecisionUsage {
                input_tokens: 17,
                output_tokens: 2,
            },
        })
    }
}

async fn setup(
    harness: Arc<NativeSkills>,
    responses: Vec<MockResponse>,
    semantic: bool,
) -> Result<(CoreSession, Arc<RecordingExecutor>, Arc<Relevant>), Box<dyn std::error::Error>> {
    setup_features(harness, responses, semantic, false).await
}

async fn setup_features(
    harness: Arc<NativeSkills>,
    responses: Vec<MockResponse>,
    semantic: bool,
    files: bool,
) -> Result<(CoreSession, Arc<RecordingExecutor>, Arc<Relevant>), Box<dyn std::error::Error>> {
    let executor = Arc::new(RecordingExecutor {
        mock: MockExecutor::new(responses),
        agent_once: Default::default(),
        prompts: Default::default(),
        calls: AtomicUsize::new(0),
    });
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("native-skill")]);
    let backend = Arc::new(Relevant {
        calls: AtomicUsize::new(0),
        requests: Default::default(),
        gate: Default::default(),
        seen: tokio::sync::Semaphore::new(0),
    });
    let mut app = App::builder().language_model(|builder| {
        builder
            .routing_table(Arc::new(table))
            .executor(executor.clone());
    });
    if semantic {
        app = app.decision_model(DecisionRuntime {
            model: "skill-relevance-fixture".into(),
            executor: backend.clone(),
            policy: Default::default(),
            pricing: None,
        });
    }
    let mut features = vec![
        FEATURE.into(),
        bitrouter_orchestrator::core::skills::FEATURE.into(),
    ];
    if files {
        features.push(bitrouter_orchestrator::core::skills::FILES_FEATURE.into());
    }
    let session =
        bind_app_with_features(Arc::new(app.build()?), harness, Limits::default(), features)
            .await?;
    Ok((session, executor, backend))
}

#[tokio::test]
async fn explicit_required_skills_load_without_a_semantic_backend() -> TestResult {
    let host =
        Arc::new(NativeSkills::new(Arc::new(Harness::new(None, None)), "EXPLICIT_ONLY").await?);
    let (session, executor, backend) =
        setup(host.clone(), vec![output(vec![text("done")])], false).await?;
    let mut task = input();
    task.routing.skills = Some(SkillConstraints {
        include: vec![SkillSelector::Local {
            scope: LocalScope::Project,
            directory_id: "installation_review".into(),
        }],
        exclude: vec![],
    });
    session
        .start("required-skill", session.head().await.state_revision, task)
        .await?;
    session.drive().await?;
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    assert!(
        executor.prompts.lock().await[0]
            .system
            .as_deref()
            .is_some_and(|system| system.contains("EXPLICIT_ONLY"))
    );
    Ok(())
}

#[tokio::test]
async fn configured_decision_backend_is_not_called_and_unselected_skills_stay_out() -> TestResult {
    let host =
        Arc::new(NativeSkills::new(Arc::new(Harness::new(None, None)), "DO_NOT_LOAD").await?);
    let (session, executor, backend) =
        setup(host.clone(), vec![output(vec![text("done")])], true).await?;
    session
        .start(
            "optional-skill",
            session.head().await.state_revision,
            input(),
        )
        .await?;
    session.drive().await?;
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert!(
        !executor.prompts.lock().await[0]
            .system
            .as_deref()
            .is_some_and(|system| system.contains("DO_NOT_LOAD"))
    );
    assert_eq!(
        root_selection(&session.snapshot().await)
            .map(|selection| selection.candidates[0].reason.as_str()),
        Some("not_explicitly_selected")
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_selection_ack_prevents_content_fetch_and_model_dispatch() -> TestResult {
    let durable = Arc::new(Harness::new(Some("skills.selected"), None));
    let host = Arc::new(NativeSkills::new(durable, "NO_ACK").await?);
    let (session, executor, _) =
        setup(host.clone(), vec![output(vec![text("must not run")])], true).await?;
    session
        .start(
            "no-ack",
            session.head().await.state_revision,
            selected_input(&host).await,
        )
        .await?;
    assert!(session.drive().await.is_err());
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn original_bytes_are_rechecked_before_a_later_model_step() -> TestResult {
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::new(durable.clone(), "ORIGINAL_V1").await?);
    let (session, executor, _) = setup(
        host.clone(),
        vec![
            output(vec![call("inspect")]),
            output(vec![text("must not run")]),
        ],
        true,
    )
    .await?;
    session
        .start(
            "tamper",
            session.head().await.state_revision,
            selected_input(&host).await,
        )
        .await?;
    session.drive().await?;
    let source = root_selection(&session.snapshot().await)
        .ok_or("selection")?
        .candidates[0]
        .source
        .clone();
    host.corrupt(&source).await?;
    let command = durable.sent.lock().await[0].clone();
    session
        .tool_result("tamper-result", result(&command))
        .await?;
    let failed = session.drive().await?;
    assert_eq!(
        failed.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    let selection = root_selection(&failed).ok_or("failed original selection")?;
    assert_eq!(
        selection.failure.as_ref().map(|error| error.code),
        Some(ErrorCode::ArtifactUnavailable)
    );
    assert_eq!(selection.candidates[0].source, source);
    assert!(
        selection.candidates[0]
            .content
            .as_ref()
            .is_some_and(|body| body.contains("ORIGINAL_V1"))
    );
    assert!(failed.operations.contains_key("tamper"));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn a_required_unreadable_root_fails_the_original_accepted_run_without_a_model() -> TestResult
{
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::new(durable, "UNREADABLE_REQUIRED").await?);
    let source = host
        .catalog
        .lock()
        .await
        .sources()
        .first()
        .cloned()
        .ok_or("fixture source")?;
    host.corrupt(&source).await?;
    let (session, executor, _) = setup(host, vec![], false).await?;
    let mut task = input();
    task.routing.skills = Some(SkillConstraints {
        include: vec![source.selector()],
        exclude: vec![],
    });
    session
        .start(
            "required-unreadable",
            session.head().await.state_revision,
            task,
        )
        .await?;
    let failed = session.drive().await?;
    assert_eq!(
        failed.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(failed.operations.contains_key("required-unreadable"));
    let selection = root_selection(&failed).ok_or("selection")?;
    assert_eq!(
        selection.failure.as_ref().map(|error| error.code),
        Some(ErrorCode::ArtifactUnavailable)
    );
    assert_eq!(selection.candidates[0].source, source);
    assert!(selection.candidates[0].content.is_none());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn an_unavailable_required_skill_never_silently_runs_without_it() -> TestResult {
    let host = Arc::new(NativeSkills::new(Arc::new(Harness::new(None, None)), "AVAILABLE").await?);
    let (session, executor, _) = setup(
        host.clone(),
        vec![output(vec![text("must not run")])],
        false,
    )
    .await?;
    let mut task = input();
    task.routing.skills = Some(SkillConstraints {
        include: vec![SkillSelector::Local {
            scope: LocalScope::Project,
            directory_id: "missing_installation".into(),
        }],
        exclude: vec![],
    });
    session
        .start("missing-skill", session.head().await.state_revision, task)
        .await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(snapshot.operations.contains_key("missing-skill"));
    let error = root_selection(&snapshot)
        .and_then(|selection| selection.failure.as_ref())
        .ok_or("missing admission rejection")?;
    assert_eq!(error.code, ErrorCode::ArtifactUnavailable);
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn recovery_settles_a_committed_rejection_without_new_selection_or_execution() -> TestResult {
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::new(durable.clone(), "ORIGINAL_CATALOG").await?);
    let (session, executor, _) = setup(host, vec![], false).await?;
    let mut task = input();
    task.routing.skills = Some(SkillConstraints {
        include: vec![SkillSelector::Local {
            scope: LocalScope::Project,
            directory_id: "missing_installation".into(),
        }],
        exclude: vec![],
    });
    session
        .start("rejected-input", session.head().await.state_revision, task)
        .await?;
    let failed = session.drive().await?;
    let original_turn = failed
        .root_turn()
        .ok_or("original Turn")?
        .agent_turn_id
        .clone();
    let prefix = recovery::prefix(&*durable.store.lock().await, "skills.admission.rejected")?;
    session.disconnect().await;
    let replacement = recovery::harness_at(prefix).await;
    let new_host = Arc::new(NativeSkills::new(replacement.clone(), "DO_NOT_RESELECT").await?);
    let request = recovery::request(&*replacement.store.lock().await, true)?;
    let mut caps = recovery::capabilities(&request.binding.grant.core_instance_id);
    caps.operations = vec![
        FEATURE.into(),
        bitrouter_orchestrator::core::skills::FEATURE.into(),
    ];
    let (app, resumed_executor) = recovery::application(vec![])?;
    let resumed = CoreSession::restore(
        request,
        &caps,
        app,
        CallerContext::local(),
        new_host.clone(),
    )
    .await?;
    let snapshot = resumed.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(
        snapshot.root_turn().map(|turn| &turn.agent_turn_id),
        Some(&original_turn)
    );
    assert!(snapshot.operations.contains_key("rejected-input"));
    assert!(root_selection(&snapshot).is_some_and(|selection| selection.failure.is_some()));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(resumed_executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(new_host.reads.load(Ordering::SeqCst), 0);
    Ok(())
}

fn required_mcp_task() -> TaskInput {
    let mut task = input();
    task.routing.skills = Some(SkillConstraints {
        include: vec![SkillSelector::Mcp {
            server: "configured_source".into(),
            uri: "skill://review/SKILL.md".into(),
        }],
        exclude: vec![],
    });
    task
}

#[tokio::test]
async fn explain_distinguishes_selected_loaded_and_exact_prepared_mcp_requests_without_reads()
-> TestResult {
    use bitrouter_orchestrator::core::skills::explain::capture;
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable, "EXPLAIN_BODY_IS_NOT_METADATA").await?);
    let (session, executor, _) =
        setup(host.clone(), vec![output(vec![call("inspect")])], false).await?;
    session
        .start(
            "explain-input",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    let pending = session.drive().await?;
    let run_id = pending.run.as_ref().ok_or("accepted run")?.run_id.clone();
    let selected = capture(&pending, &run_id)?;
    let candidate = &selected.work_units[0]
        .selection
        .as_ref()
        .ok_or("selected explanation")?
        .candidates[0];
    assert!(candidate.selected && candidate.instructions.is_none());
    assert!(candidate.prepared_model_requests.is_empty());
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    let consent = host.consent_requests.lock().await[0].clone();
    session
        .answer_skill_consent("explain-consent", consent.answer(true))
        .await?;
    let loaded = session.drive().await?;
    let explained = capture(&loaded, &run_id)?;
    let candidate = &explained.work_units[0]
        .selection
        .as_ref()
        .ok_or("loaded explanation")?
        .candidates[0];
    assert!(candidate.instructions.is_some());
    assert_eq!(candidate.prepared_model_requests.len(), 1);
    assert!(
        candidate.prepared_model_requests[0]
            .attempts
            .as_ref()
            .is_some_and(|attempts| attempts.len() == 1 && attempts[0].model_result_received)
    );
    assert!(!serde_json::to_string(&explained)?.contains("EXPLAIN_BODY_IS_NOT_METADATA"));
    assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);

    let mut implicit = loaded.clone();
    for work in implicit.context_store.work.values_mut() {
        if let Some(selection) = &mut work.skills {
            selection.constraints = SkillConstraints::default();
        }
    }
    for agent in implicit.agents.values_mut() {
        if let Some(turn) = &mut agent.turn {
            turn.input.routing.skills = None;
        }
    }
    assert!(capture(&implicit, &run_id).is_err());

    let mut wrong_role = loaded.clone();
    for execution in wrong_role.context_store.executions.values_mut() {
        if let Some(plan) = &mut execution.model {
            for message in &mut plan.prompt.messages {
                if serde_json::to_string(message)?.contains("EXPLAIN_BODY_IS_NOT_METADATA") {
                    message.role = bitrouter_ai::types::Role::System;
                }
            }
        }
    }
    let wrong_role = capture(&wrong_role, &run_id)?;
    assert!(
        wrong_role.work_units[0]
            .selection
            .as_ref()
            .ok_or("role explanation")?
            .candidates[0]
            .prepared_model_requests
            .is_empty(),
        "selection plus bytes cannot stand in for the actual source-tagged user frame"
    );
    let mut corrupt = loaded;
    let selection = corrupt
        .context_store
        .work
        .values_mut()
        .find_map(|work| work.skills.as_mut())
        .ok_or("mutable skill selection")?;
    selection.candidates[0]
        .content
        .as_mut()
        .ok_or("stored root")?
        .push('!');
    assert!(capture(&corrupt, &run_id).is_err());
    assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn an_explicit_unlisted_mcp_uri_resolves_metadata_before_consent_without_semantic_selection()
-> TestResult {
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable.clone(), "UNLISTED_APPROVED_BODY").await?);
    *host.direct.lock().await = std::mem::take(&mut host.catalog.lock().await.mcp.skills);
    let (session, executor, backend) =
        setup(host.clone(), vec![output(vec![text("done")])], false).await?;
    session
        .start(
            "unlisted-input",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    assert_eq!(host.metadata_reads.load(Ordering::SeqCst), 1);
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let request = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("unlisted consent")?;
    assert_eq!(request.source, host.direct.lock().await[0]);
    session
        .answer_skill_consent("unlisted-consent", request.answer(true))
        .await?;
    let completed = session.drive().await?;
    assert_eq!(
        completed.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(host.metadata_reads.load(Ordering::SeqCst), 1);
    assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    assert!(
        serde_json::to_string(&executor.prompts.lock().await[0].messages)?
            .contains("UNLISTED_APPROVED_BODY")
    );
    let selected = root_selection(&completed).ok_or("original selection")?;
    assert_eq!(
        selected.candidates[0].source,
        SkillSource::Mcp(request.source)
    );
    assert!(
        selected
            .constraints
            .include
            .contains(&selected.candidates[0].source.selector())
    );
    Ok(())
}

#[tokio::test]
async fn an_unresolvable_explicit_mcp_uri_fails_accepted_work_without_content_or_model()
-> TestResult {
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable, "UNSELECTED_BODY").await?);
    host.catalog.lock().await.mcp.skills.clear();
    let (session, executor, _) = setup(host.clone(), vec![], false).await?;
    session
        .start(
            "unresolvable-input",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    let failed = session.drive().await?;
    assert_eq!(
        failed.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(failed.operations.contains_key("unresolvable-input"));
    assert_eq!(host.metadata_reads.load(Ordering::SeqCst), 1);
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(host.consent_requests.lock().await.is_empty());
    let selected = root_selection(&failed).ok_or("failed selection")?;
    assert!(selected.failure.is_some());
    assert!(
        selected
            .catalog
            .mcp
            .excluded
            .iter()
            .any(
                |problem| problem.uri.as_deref() == Some("skill://review/SKILL.md")
                    && problem.reason.contains("explicit URI unavailable")
            )
    );
    Ok(())
}

#[tokio::test]
async fn a_later_model_step_uses_the_original_mcp_cache_when_source_bytes_disappear() -> TestResult
{
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable.clone(), "ORIGINAL_MCP_CACHE").await?);
    let (session, executor, _) = setup(
        host.clone(),
        vec![output(vec![call("inspect")]), output(vec![text("done")])],
        false,
    )
    .await?;
    session
        .start(
            "cache-turn",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    session.drive().await?;
    let request = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("consent")?;
    session
        .answer_skill_consent("cache-consent", request.answer(true))
        .await?;
    session.drive().await?;
    let original = root_selection(&session.snapshot().await)
        .cloned()
        .ok_or("original selection")?;
    assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    host.bytes.lock().await.clear();
    let command = durable
        .sent
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("tool command")?;
    session
        .tool_result("cache-tool-result", result(&command))
        .await?;
    let completed = session.drive().await?;
    assert_eq!(
        completed.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let current = root_selection(&completed).ok_or("retained selection")?;
    assert_eq!(current.candidates[0].source, original.candidates[0].source);
    assert_eq!(
        current.candidates[0].content,
        original.candidates[0].content
    );
    assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 2);
    for prompt in prompts.iter() {
        assert!(
            !prompt
                .system
                .as_deref()
                .unwrap_or_default()
                .contains("ORIGINAL_MCP_CACHE")
        );
        assert!(serde_json::to_string(&prompt.messages)?.contains("ORIGINAL_MCP_CACHE"));
    }
    Ok(())
}

#[tokio::test]
async fn restoration_revalidates_the_original_mcp_cache_without_a_catalog_replacement() -> TestResult
{
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable.clone(), "ACKNOWLEDGED_MCP_CACHE").await?);
    let (session, _, _) = setup(host.clone(), vec![output(vec![text("done")])], false).await?;
    session
        .start(
            "cached-original-input",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    session.drive().await?;
    let request = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("consent")?;
    session
        .answer_skill_consent("cached-original-consent", request.answer(true))
        .await?;
    session.drive().await?;
    let original = root_selection(&session.snapshot().await)
        .cloned()
        .ok_or("original selection")?;
    let prefix = recovery::prefix(&*durable.store.lock().await, "skills.instructions.verified")?;
    session.disconnect().await;
    let replacement = recovery::harness_at(prefix).await;
    let new_host = Arc::new(
        NativeSkills::with_mcp(replacement.clone(), "NEVER_SUBSTITUTE_THIS_VERSION").await?,
    );
    new_host.bytes.lock().await.clear();
    let restore = recovery::request(&*replacement.store.lock().await, true)?;
    let mut caps = recovery::capabilities(&restore.binding.grant.core_instance_id);
    caps.operations = vec![
        FEATURE.into(),
        bitrouter_orchestrator::core::skills::FEATURE.into(),
    ];
    let (app, executor) = recovery::application(vec![output(vec![text("restored original")])])?;
    let resumed = CoreSession::restore(
        restore,
        &caps,
        app,
        CallerContext::local(),
        new_host.clone(),
    )
    .await?;
    let completed = resumed.drive().await?;
    assert_eq!(
        completed.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let current = root_selection(&completed).ok_or("restored selection")?;
    assert_eq!(current.candidates[0].source, original.candidates[0].source);
    assert_eq!(
        current.candidates[0].content,
        original.candidates[0].content
    );
    assert_eq!(
        current.candidates[0].consents,
        original.candidates[0].consents
    );
    assert!(completed.operations.contains_key("cached-original-input"));
    assert!(completed.operations.contains_key("cached-original-consent"));
    assert_eq!(new_host.reads.load(Ordering::SeqCst), 0);
    assert!(new_host.consent_requests.lock().await.is_empty());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let prompt = executor.prompts.lock().await;
    let messages =
        serde_json::to_string(&prompt.first().ok_or("restored model request")?.messages)?;
    assert!(messages.contains("ACKNOWLEDGED_MCP_CACHE"));
    assert!(!messages.contains("NEVER_SUBSTITUTE_THIS_VERSION"));
    Ok(())
}

#[tokio::test]
async fn mcp_consent_is_metadata_only_then_binds_the_exact_entry_in_the_model_request() -> TestResult
{
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable, "APPROVED_MCP_BODY").await?);
    let (session, executor, _) =
        setup(host.clone(), vec![output(vec![text("done")])], true).await?;
    session
        .start(
            "mcp-start",
            session.head().await.state_revision,
            selected_input(&host).await,
        )
        .await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let request = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("missing consent request")?;
    assert!(session.skill_consent_pending(&request).await);
    let mut wrong = request.answer(true);
    wrong.entry_digest = "0".repeat(64);
    assert!(
        session
            .answer_skill_consent("wrong-manifest", wrong)
            .await
            .is_err()
    );
    let receipt = session
        .answer_skill_consent("approve-skill", request.answer(true))
        .await?;
    assert_eq!(
        receipt,
        session
            .answer_skill_consent("approve-skill", request.answer(true))
            .await?
    );
    assert!(
        session
            .answer_skill_consent("flip-answer", request.answer(false))
            .await
            .is_err()
    );
    assert_eq!(
        host.reads.load(Ordering::SeqCst),
        0,
        "answering must not prefetch content"
    );
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    let prompts = executor.prompts.lock().await;
    let prompt = prompts.first().ok_or("actual model request")?;
    let system = prompt.system.as_deref().ok_or("system")?;
    assert!(!system.contains("APPROVED_MCP_BODY"));
    let resource = prompt
        .messages
        .iter()
        .filter(|message| message.role == bitrouter_ai::types::Role::User)
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::Text { text, .. } if text.contains("APPROVED_MCP_BODY") => Some(text),
            _ => None,
        })
        .ok_or("MCP data frame")?;
    assert!(resource.contains(&request.source.entry.uri));
    assert!(resource.contains(&request.source.entry_digest));
    assert!(resource.contains("skill://review/references/check.md"));
    assert!(snapshot.context_store.evidence.values().any(
        |block| block.source.skill_binding.as_ref() == Some(&request.consent.skill_id)
            && block.protected
    ));
    assert!(
        !prompt
            .tools
            .iter()
            .any(|tool| matches!(tool, Tool::Function { name, .. } if name == "shell"))
    );
    assert_eq!(host.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        root_selection(&snapshot).ok_or("selection")?.candidates[0].consents[0].approved,
        Some(true)
    );
    Ok(())
}

#[tokio::test]
async fn denied_required_mcp_skill_preserves_acceptance_but_fails_execution_admission() -> TestResult
{
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "REQUIRED_DENIED").await?,
    );
    let (session, executor, _) = setup(host.clone(), vec![], false).await?;
    session
        .start(
            "required-mcp",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    session.drive().await?;
    let request = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("consent")?;
    session
        .answer_skill_consent("deny-required", request.answer(false))
        .await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert!(snapshot.operations.contains_key("required-mcp"));
    assert!(
        root_selection(&snapshot)
            .and_then(|selection| selection.failure.as_ref())
            .is_some_and(|error| error.message.contains("consent was denied"))
    );
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn cancellation_expires_the_exact_skill_request_and_refuses_a_late_answer() -> TestResult {
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "CANCELLED_BODY").await?,
    );
    let (session, executor, _) = setup(host.clone(), vec![], true).await?;
    session
        .start(
            "cancel-mcp",
            session.head().await.state_revision,
            selected_input(&host).await,
        )
        .await?;
    let waiting = session.drive().await?;
    let request = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("consent")?;
    let first = session.skill_consent_expired(&request);
    let second = session.skill_consent_expired(&request);
    tokio::pin!(first, second);
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    session
        .cancel_run(
            "cancel-skill",
            session.head().await.state_revision,
            &waiting.run.ok_or("run")?.run_id,
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(&mut first, &mut second);
    })
    .await?;
    assert!(
        session
            .answer_skill_consent("late-answer", request.answer(true))
            .await
            .is_err()
    );
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn an_unacknowledged_mcp_consent_answer_cannot_fetch_or_activate_content() -> TestResult {
    let host = Arc::new(
        NativeSkills::with_mcp(
            Arc::new(Harness::new(Some("skill.consent.answered"), None)),
            "UNACKNOWLEDGED_BODY",
        )
        .await?,
    );
    let (session, executor, _) = setup(host.clone(), vec![], false).await?;
    session
        .start(
            "unknown-mcp-approval",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    session.drive().await?;
    let request = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("consent")?;
    assert!(
        session
            .answer_skill_consent("unacknowledged-approval", request.answer(true))
            .await
            .is_err()
    );
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn a_pending_skill_consent_is_reissued_under_the_restored_owner_epoch() -> TestResult {
    let durable = Arc::new(Harness::new(None, None));
    let host = Arc::new(NativeSkills::with_mcp(durable.clone(), "ORIGINAL_HELD_ENTRY").await?);
    let (session, executor, _) = setup(host.clone(), vec![], false).await?;
    session
        .start(
            "epoch-mcp",
            session.head().await.state_revision,
            required_mcp_task(),
        )
        .await?;
    session.drive().await?;
    let old = host.consent_requests.lock().await[0].clone();
    let committed = durable.store.lock().await.clone();
    session.disconnect().await;
    let replacement = recovery::harness_at(committed).await;
    let new_host =
        Arc::new(NativeSkills::with_mcp(replacement.clone(), "ORIGINAL_HELD_ENTRY").await?);
    let request = recovery::request(&*replacement.store.lock().await, true)?;
    let mut caps = recovery::capabilities(&request.binding.grant.core_instance_id);
    caps.operations = vec![
        FEATURE.into(),
        bitrouter_orchestrator::core::skills::FEATURE.into(),
    ];
    let (app, resumed_executor) = recovery::application(vec![output(vec![text("done")])])?;
    let mut old_caps = caps.clone();
    old_caps
        .operations
        .retain(|feature| feature != bitrouter_orchestrator::core::skills::FEATURE);
    let rejected = CoreSession::restore(
        request.clone(),
        &old_caps,
        app.clone(),
        CallerContext::local(),
        new_host.clone(),
    )
    .await;
    assert!(matches!(
        rejected,
        Err(CoreError {
            code: ErrorCode::UnsupportedCapability,
            ..
        })
    ));
    assert_eq!(new_host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(resumed_executor.calls.load(Ordering::SeqCst), 0);
    let resumed = CoreSession::restore(
        request,
        &caps,
        app,
        CallerContext::local(),
        new_host.clone(),
    )
    .await?;
    resumed.drive().await?;
    let current = new_host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("restored consent")?;
    assert_ne!(current.consent.request_id, old.consent.request_id);
    assert!(current.consent.execution_epoch > old.consent.execution_epoch);
    assert_eq!(current.source, old.source);
    let error = resumed
        .answer_skill_consent("stale-owner-answer", old.answer(true))
        .await
        .err()
        .ok_or("old epoch accepted")?;
    assert_eq!(error.code, ErrorCode::StaleEpoch);
    assert_eq!(new_host.reads.load(Ordering::SeqCst), 0);
    resumed
        .answer_skill_consent("restored-owner-answer", current.answer(true))
        .await?;
    assert_eq!(
        resumed.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(new_host.reads.load(Ordering::SeqCst), 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(resumed_executor.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn permission_changes_revoke_the_old_skill_consent_before_any_body_fetch() -> TestResult {
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "PERMISSION_BOUND").await?,
    );
    let (session, executor, _) =
        setup(host.clone(), vec![output(vec![text("done")])], true).await?;
    session
        .start(
            "permission-mcp",
            session.head().await.state_revision,
            selected_input(&host).await,
        )
        .await?;
    session.drive().await?;
    let old = host.consent_requests.lock().await[0].clone();
    session
        .answer_skill_consent("first-permission", old.answer(true))
        .await?;
    let snapshot = session.snapshot().await;
    let mut manifest = snapshot.manifest.clone();
    manifest.permission_revision += 1;
    session
        .signals(
            "permission-changed",
            SignalUpdate {
                signal_revision: snapshot.signals.revision + 1,
                observed_at: "2".into(),
                scope: snapshot.session_id.clone(),
                source: grant().harness_id,
                workspace_revision: manifest.workspace_revision.clone(),
                manifest,
                materials: vec![],
                facts: BTreeMap::new(),
            },
        )
        .await?;
    session.drive().await?;
    let current = host
        .consent_requests
        .lock()
        .await
        .last()
        .cloned()
        .ok_or("renewed consent")?;
    assert_ne!(current.consent.request_id, old.consent.request_id);
    assert_eq!(current.source, old.source);
    assert!(!session.skill_consent_pending(&old).await);
    let snapshot = session.snapshot().await;
    assert!(root_selection(&snapshot).ok_or("selection")?.candidates[0].consents[0].revoked);
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    session
        .answer_skill_consent("deny-new-permission", current.answer(false))
        .await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(host.reads.load(Ordering::SeqCst), 0);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn a_permission_change_during_a_skill_read_cancels_old_bytes_before_reapproval() -> TestResult
{
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "CANCEL_OLD_READ").await?,
    );
    let (session, executor, _) =
        setup(host.clone(), vec![output(vec![text("done")])], true).await?;
    session
        .start(
            "read-permission",
            session.head().await.state_revision,
            selected_input(&host).await,
        )
        .await?;
    session.drive().await?;
    let old = host
        .consent_requests
        .lock()
        .await
        .first()
        .cloned()
        .ok_or("consent")?;
    session
        .answer_skill_consent("allow-old-read", old.answer(true))
        .await?;
    *host.read_gate.lock().await = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    let running = {
        let core = session.clone();
        tokio::spawn(async move { core.drive().await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), host.read_seen.acquire())
        .await??
        .forget();
    let before = session.snapshot().await;
    let mut manifest = before.manifest.clone();
    manifest.permission_revision += 1;
    session
        .signals(
            "revoke-during-read",
            SignalUpdate {
                signal_revision: before.signals.revision + 1,
                observed_at: "2".into(),
                scope: before.session_id.clone(),
                source: grant().harness_id,
                workspace_revision: manifest.workspace_revision.clone(),
                manifest,
                materials: vec![],
                facts: BTreeMap::new(),
            },
        )
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), running).await???;
    assert!(
        host.read_token
            .lock()
            .await
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    );
    let snapshot = session.snapshot().await;
    let candidate = root_selection(&snapshot)
        .ok_or("selection")?
        .candidates
        .first()
        .ok_or("candidate")?;
    assert!(candidate.content.is_none() && candidate.failure.is_none());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    let current = host
        .consent_requests
        .lock()
        .await
        .last()
        .cloned()
        .ok_or("renewed consent")?;
    assert_ne!(old.consent.request_id, current.consent.request_id);
    assert_eq!(old.source, current.source);
    session
        .answer_skill_consent("deny-new-read", current.answer(false))
        .await?;
    assert_eq!(
        session.drive().await?.run.as_ref().map(|run| run.status),
        Some(RunStatus::Failed)
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(!serde_json::to_string(&*executor.prompts.lock().await)?.contains("CANCEL_OLD_READ"));
    Ok(())
}

#[tokio::test]
async fn mcp_resource_data_is_not_shared_into_an_unconsenting_child_context() -> TestResult {
    let host = Arc::new(
        NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "PARENT_MCP_ONLY").await?,
    );
    let (session, executor, _) = setup(
        host.clone(),
        vec![
            output(vec![core_call(
                "spawn_agent",
                json!({"task":work("child-a")}),
                "spawn-mcp-child",
            )]),
            output(vec![text("done")]),
            output(vec![text("done")]),
            output(vec![text("parent consumed child")]),
        ],
        true,
    )
    .await?;
    session
        .start(
            "mcp-child",
            session.head().await.state_revision,
            selected_input(&host).await,
        )
        .await?;
    session.drive().await?;
    let request = host.consent_requests.lock().await[0].clone();
    session
        .answer_skill_consent("approve-parent-only", request.answer(true))
        .await?;
    let snapshot = session.drive().await?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Waiting)
    );
    let child = snapshot
        .agents
        .values()
        .find(|agent| agent.parent_id.is_some())
        .ok_or("child")?;
    let child_work = snapshot
        .context_store
        .work
        .get(&child.turn.as_ref().ok_or("child Turn")?.agent_turn_id)
        .ok_or("child work")?;
    assert!(child_work.shared_evidence.iter().all(|id| {
        snapshot
            .context_store
            .evidence
            .get(id)
            .is_some_and(|block| block.source.skill_binding.is_none())
    }));
    let prompts = executor.prompts.lock().await;
    assert!(
        !prompts
            .iter()
            .any(|prompt| prompt.messages.iter().any(|message| message.role
                == bitrouter_ai::types::Role::User
                && message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::Text{text,..} if text=="child-a"))))
    );
    assert_eq!(host.consent_requests.lock().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn any_support_manifest_change_requires_new_consent_even_when_root_bytes_are_identical()
-> TestResult {
    fn canonical(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(values) => serde_json::Value::Object(
                values
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .map(|(key, value)| (key, canonical(value)))
                    .collect(),
            ),
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(canonical).collect())
            }
            value => value,
        }
    }
    for change in ["add", "remove", "modify"] {
        let host = Arc::new(
            NativeSkills::with_mcp(Arc::new(Harness::new(None, None)), "IDENTICAL_ROOT").await?,
        );
        let (session, executor, _) = setup(
            host.clone(),
            vec![output(vec![text("first")]), output(vec![text("second")])],
            false,
        )
        .await?;
        session
            .start(
                "first-manifest",
                session.head().await.state_revision,
                required_mcp_task(),
            )
            .await?;
        session.drive().await?;
        let old = host.consent_requests.lock().await[0].clone();
        session
            .answer_skill_consent("first-manifest-consent", old.answer(true))
            .await?;
        let first = session.drive().await?;
        assert_eq!(
            first.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        let mut updated = old.source.clone();
        let mut entry = serde_json::to_value(&updated.entry)?;
        let files = entry["resources"].as_array_mut().ok_or("manifest files")?;
        match change {
            "add"=>files.push(json!({"uri":"skill://review/new.md","digest":format!("sha256:{}",sha256(b"new")),"size":3})),
            "remove"=>{files.remove(1);},
            _=>{files[1]["digest"]=format!("sha256:{}",sha256(b"updated")).into();files[1]["size"]=7.into();},
        }
        updated.entry = serde_json::from_value(entry)?;
        let mut sorted = updated.entry.clone();
        if let bitrouter_sdk::mcp::skills::SkillResources::Enumerated(files) = &mut sorted.resources
        {
            files.sort_by(|left, right| left.uri.cmp(&right.uri));
        }
        updated.entry_digest = sha256(&serde_json::to_vec(&canonical(serde_json::to_value(
            &sorted,
        )?))?);
        updated.validate()?;
        let source = SkillSource::Mcp(updated.clone());
        let original_root = host
            .bytes
            .lock()
            .await
            .get(&SkillSource::Mcp(old.source.clone()).identity()?)
            .cloned()
            .ok_or("root bytes")?;
        host.bytes
            .lock()
            .await
            .insert(source.identity()?, original_root);
        host.catalog.lock().await.mcp.skills = vec![updated.clone()];
        session
            .start(
                "second-manifest",
                session.head().await.state_revision,
                required_mcp_task(),
            )
            .await?;
        session.drive().await?;
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.reads.load(Ordering::SeqCst), 1);
        let new = host
            .consent_requests
            .lock()
            .await
            .last()
            .cloned()
            .ok_or("new approval")?;
        assert_eq!(old.source.entry.uri, new.source.entry.uri);
        assert_ne!(old.source.entry_digest, new.source.entry_digest);
        assert_ne!(old.consent.skill_id, new.consent.skill_id);
        assert_eq!(new.source, updated);
        assert!(
            session
                .answer_skill_consent("old-manifest-on-new-turn", old.answer(true))
                .await
                .is_err()
        );
        session
            .answer_skill_consent("second-manifest-consent", new.answer(true))
            .await?;
        let second = session.drive().await?;
        assert_eq!(
            second.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        assert_eq!(host.reads.load(Ordering::SeqCst), 2);
        let prompts = executor.prompts.lock().await;
        assert!(serde_json::to_string(&prompts[0])?.contains(&old.source.entry_digest));
        let latest = serde_json::to_string(&prompts[1])?;
        assert!(
            latest.contains(&new.source.entry_digest) && !latest.contains(&old.source.entry_digest)
        );
        assert_eq!(
            second
                .skill_selection(&first.root_turn().ok_or("first Turn")?.agent_turn_id)
                .ok_or("historical selection")?
                .candidates[0]
                .source,
            SkillSource::Mcp(old.source)
        );
    }
    Ok(())
}

#[tokio::test]
async fn unselected_skills_stay_unloaded_and_five_explicit_sources_exceed_the_bound() -> TestResult
{
    for required in [false, true] {
        let host = Arc::new(
            NativeSkills::new(Arc::new(Harness::new(None, None)), "COUNTED_SKILL_0").await?,
        );
        let mut bindings = Vec::new();
        for index in 0..5 {
            let (mut source, body) = binding(&format!("COUNTED_SKILL_{index}"));
            if let SkillSource::Local(binding) = &mut source {
                binding.directory_id = format!("installation_{index}");
            }
            host.bytes.lock().await.insert(source.identity()?, body);
            if let SkillSource::Local(binding) = source {
                bindings.push(binding);
            }
        }
        host.catalog.lock().await.local.skills = bindings.clone();
        let (session, executor, backend) =
            setup(host.clone(), vec![output(vec![text("done")])], true).await?;
        let mut task = input();
        if required {
            task.routing.skills = Some(SkillConstraints {
                include: bindings
                    .into_iter()
                    .map(|binding| SkillSource::Local(binding).selector())
                    .collect(),
                exclude: vec![],
            });
        }
        session
            .start("bounded-set", session.head().await.state_revision, task)
            .await?;
        let snapshot = session.drive().await?;
        let selection = root_selection(&snapshot).ok_or("selection")?;
        if required {
            assert_eq!(
                snapshot.run.as_ref().map(|run| run.status),
                Some(RunStatus::Failed)
            );
            assert_eq!(host.reads.load(Ordering::SeqCst), 0);
            assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            assert!(selection.failure.is_some());
        } else {
            assert_eq!(
                snapshot.run.as_ref().map(|run| run.status),
                Some(RunStatus::Completed)
            );
            assert_eq!(
                selection
                    .candidates
                    .iter()
                    .filter(|candidate| candidate.selected)
                    .count(),
                0
            );
            assert_eq!(host.reads.load(Ordering::SeqCst), 0);
            assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
            let model = serde_json::to_string(&executor.prompts.lock().await[0])?;
            assert_eq!(
                (0..5)
                    .filter(|index| model.contains(&format!("COUNTED_SKILL_{index}")))
                    .count(),
                0
            );
        }
    }
    Ok(())
}

async fn selected_input(host: &NativeSkills) -> TaskInput {
    let mut task = super::input();
    task.routing.skills = Some(SkillConstraints {
        include: host
            .catalog
            .lock()
            .await
            .sources()
            .iter()
            .map(SkillSource::selector)
            .collect(),
        exclude: vec![],
    });
    task
}
