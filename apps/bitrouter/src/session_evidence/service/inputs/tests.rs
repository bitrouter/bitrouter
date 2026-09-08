use super::*;
use crate::session_evidence::adapter_bridge::{AdapterIdentity, Event, Observation};
use crate::session_evidence::service::tests::{observation, write_rows};

mod executions;
mod rollouts;

async fn fixture(directory: &Path, harness: Harness) -> Result<EvidenceHandle> {
    fixture_at(directory, harness, None).await
}

async fn fixture_at(
    directory: &Path,
    harness: Harness,
    codex_home: Option<&Path>,
) -> Result<EvidenceHandle> {
    let key = if harness == Harness::Codex {
        "codex"
    } else {
        "claude"
    };
    let pins: Value = serde_json::from_str(include_str!("../../adapter_bridge/pins.json"))?;
    let pin = &pins[key];
    let identity = ControllerIdentity::new(
        if harness == Harness::Codex {
            "codex-acp"
        } else {
            "claude-acp"
        },
        pin["package"].as_str().context("package")?,
        pin["version"].as_str().context("version")?,
    );
    let mut env = HashMap::from([
        (
            "CODEX_HOME".into(),
            codex_home
                .map(Path::to_path_buf)
                .unwrap_or_else(|| directory.join("codex"))
                .to_string_lossy()
                .into_owned(),
        ),
        (
            "CLAUDE_CONFIG_DIR".into(),
            directory.join("claude").to_string_lossy().into_owned(),
        ),
    ]);
    let mut handle = EvidenceHandle::open(EvidenceLaunch {
        home: &directory.join("router"),
        database_url: "sqlite:evidence.db?mode=rwc",
        identity: &identity,
        env: &mut env,
        strip_inherited_env: &[],
    })
    .await?
    .context("evidence")?;
    handle.worker.abort();
    if let Err(error) = (&mut handle.worker).await {
        ensure!(error.is_cancelled(), "fixture worker failed");
    }
    Ok(handle)
}

async fn prompt(
    service: &ControllerEvidence,
    operation: &str,
    event: Event,
) -> Result<PromptEvidence> {
    service
        .observe(observation(
            operation,
            "session/prompt",
            "request",
            json!({"sessionId":"acp","prompt":[]}),
        ))
        .await?;
    let origin = service
        .store
        .prompt_origin(&service.controller_id, operation)
        .await?
        .context("prompt origin")?;
    let pins: Value = serde_json::from_str(include_str!("../../adapter_bridge/pins.json"))?;
    let pin = &pins[if origin.session.harness == Harness::Codex {
        "codex"
    } else {
        "claude"
    }];
    let adapter: AdapterIdentity = serde_json::from_value(
        json!({"package":pin["package"],"version":pin["version"],"moduleDigest":pin["moduleDigest"]}),
    )?;
    for (sequence, event) in [
        Event::Started,
        event,
        Event::Finished {
            outcome: "returned".into(),
            notification_failures: 0,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let value = serde_json::to_value(Observation {
            schema: 1,
            session_id: "acp".into(),
            origin: origin.clone(),
            adapter: adapter.clone(),
            sequence: sequence as u32,
            event,
        })?;
        service
            .observe(observation(
                &format!("{operation}-{sequence}"),
                crate::session_evidence::adapter_bridge::METHOD,
                "notification",
                value,
            ))
            .await?;
    }
    service
        .observe(observation(
            operation,
            "session/prompt",
            "response",
            json!({"stopReason":"end_turn"}),
        ))
        .await?;
    let attempt = service
        .store
        .active_attempt(&origin.session)
        .await?
        .context("attempt")?;
    service
        .store
        .prompt_bridge_evidence(&origin.session, &attempt.id)
        .await
}

async fn create(service: &ControllerEvidence, directory: &Path) -> Result<Value> {
    let params = json!({"cwd":directory,"mcpServers":[]});
    service
        .observe(observation(
            "create",
            "session/new",
            "request",
            params.clone(),
        ))
        .await?;
    let prepared = service
        .prepare_session_request("create", "session/new", params)
        .await?;
    service
        .observe(observation(
            "create",
            "session/new",
            "response",
            json!({"sessionId":"acp"}),
        ))
        .await?;
    Ok(prepared)
}

async fn codex_source(service: &ControllerEvidence, spool: &Path, thread: &str) -> Result<String> {
    let path = spool.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
    write_rows(&path, vec![
        json!({"method":"runtime/started","phase":"metadata","sequence":0,"version":"codex-cli 0.153.3"}),
        json!({"method":"turn/start","phase":"request","direction":"client","operation_id":"1","sequence":1,"payload":{"threadId":thread,"input":[]}}),
        json!({"method":"turn/start","phase":"response","direction":"server","operation_id":"1","sequence":2,"payload":{"turn":{"id":"turn","status":"inProgress"}}}),
    ]).await?;
    let descriptor = SourceDescriptor {
        namespace: service.collector.root().namespace.clone(),
        harness: Harness::Codex,
        format: SourceFormat::CodexAppServer,
        locator: format!("spool:{}", path.display()),
        node: None,
    };
    let id = service.store.register(descriptor.clone()).await?.id;
    import_spool(&service.store, spool, &path, descriptor).await?;
    Ok(id)
}

#[tokio::test]
async fn native_receipts_require_original_controller_thread_and_complete_raw_prefix() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: "turn".into(),
            role: "prompt".into(),
        },
    )
    .await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    codex_source(service, &service.spool.join("foreign"), "native").await?;
    let before = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert!(before["attempt"].bindings.is_empty());
    codex_source(service, &service.spool, "wrong-thread").await?;
    assert!(
        service.native_inputs(&selected, &BTreeSet::new()).await?["attempt"]
            .bindings
            .is_empty()
    );
    let id = codex_source(service, &service.spool, "native").await?;
    let after = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert_eq!(
        after["attempt"].bindings.len(),
        1,
        "{:?}",
        after["attempt"].gaps
    );
    let binding = &after["attempt"].bindings[0];
    assert_eq!(binding.input.range.source_id, id);
    assert_eq!(binding.input.range.start, 1);
    assert_eq!(binding.acknowledgements[0].record.range.start, 2);
    // A second connection accepting the same turn makes the earlier live
    // receipt ambiguous; it cannot become a frozen optimization sample.
    codex_source(service, &service.spool, "native").await?;
    let ambiguous = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert!(ambiguous["attempt"].bindings.is_empty());
    assert!(
        ambiguous["attempt"]
            .gaps
            .contains("native_input_turn_ambiguous")
    );
    Ok(())
}

#[tokio::test]
async fn competing_original_producer_claims_cannot_assign_one_input_to_two_prompts() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let event = Event::CodexAccepted {
        thread_id: "native".into(),
        turn_id: "turn".into(),
        role: "prompt".into(),
    };
    let old = prompt(service, "old", event.clone()).await?;
    codex_source(service, &service.spool, "native").await?;
    assert_eq!(
        service
            .native_inputs(
                &BTreeMap::from([("old".into(), old.clone())]),
                &BTreeSet::new()
            )
            .await?["old"]
            .bindings
            .len(),
        1
    );
    prompt(service, "new", event).await?;
    // Inspect only the old selection, as an archived-attempt reader would.
    // Raw claim scanning must still discover the newer competing origin.
    let next = service
        .native_inputs(&BTreeMap::from([("old".into(), old)]), &BTreeSet::new())
        .await?;
    assert!(next["old"].bindings.is_empty());
    assert!(next["old"].gaps.contains("native_input_producer_ambiguous"));
    Ok(())
}

async fn claude_source(
    service: &ControllerEvidence,
    prepared: &Value,
    native: Option<&str>,
) -> Result<String> {
    use crate::session_evidence::claude_proxy::{NAMESPACE_ENV, ORIGIN_ENV, SPOOL_ENV};
    let env = prepared
        .pointer("/_meta/claudeCode/options/env")
        .context("process environment")?;
    let configuration: RecordRef =
        serde_json::from_str(env[ORIGIN_ENV].as_str().context("configuration")?)?;
    let spool = PathBuf::from(env[SPOOL_ENV].as_str().context("process spool")?);
    let process_id = uuid::Uuid::new_v4().to_string();
    let path = spool.join(format!("cli-{process_id}.jsonl"));
    let mut rows = vec![
        json!({"method":"runtime/started","phase":"metadata","configured_by":configuration,"configuration_status":"present","version":"2.1.257"}),
        json!({"method":"runtime/input","phase":"request","direction":"client","payload":{"type":"user","uuid":"command","session_id":"acp"}}),
        json!({"method":"runtime/message","phase":"notification","direction":"server","payload":{"type":"command_lifecycle","command_uuid":"command","session_id":native,"state":"started"}}),
    ];
    if native.is_none() {
        rows.truncate(1);
        rows[0]["method"] = json!("runtime/failed");
    }
    for (sequence, raw) in rows.iter_mut().enumerate() {
        raw["sequence"] = json!(sequence);
        raw["process_id"] = json!(process_id);
        raw["namespace"] = env[NAMESPACE_ENV].clone();
        raw["scope_valid"] = json!(true);
    }
    write_rows(&path, rows).await?;
    import_spool(
        &service.store,
        &spool,
        &path,
        SourceDescriptor {
            namespace: env[NAMESPACE_ENV].as_str().context("namespace")?.into(),
            harness: Harness::ClaudeCode,
            format: SourceFormat::ClaudeCli,
            locator: format!("spool:{}", path.display()),
            node: None,
        },
    )
    .await?;
    Ok(process_id)
}

#[tokio::test]
async fn claude_recreation_keeps_each_input_process_and_native_reset_identity() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::ClaudeCode).await?;
    let service = &handle.service;
    let prepared = create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::ClaudeEnqueued {
            command_id: "command".into(),
        },
    )
    .await?;
    let first = claude_source(service, &prepared, Some("native-before")).await?;
    let second = claude_source(service, &prepared, Some("native-after")).await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    let output = service.native_inputs(&selected, &BTreeSet::new()).await?;
    let inputs = &output["attempt"];
    assert!(inputs.gaps.is_empty(), "{:?}", inputs.gaps);
    assert_eq!(inputs.bindings.len(), 2);
    assert_eq!(
        inputs
            .bindings
            .iter()
            .map(|binding| binding.process_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first, second])
    );
    assert_eq!(
        inputs
            .bindings
            .iter()
            .map(|binding| binding.node.native_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["native-before", "native-after"])
    );
    assert!(
        inputs
            .bindings
            .iter()
            .all(|binding| binding.origin.session.session_id == "acp"
                && binding.configuration.is_some()
                && binding.session_response.is_some())
    );
    Ok(())
}

#[tokio::test]
async fn claude_membership_preserves_one_input_across_reset_and_accepts_native_absolute_cwd()
-> Result<()> {
    use crate::session_evidence::claude_proxy::SPOOL_ENV;
    let directory = tempfile::tempdir()?;
    tokio::fs::create_dir(directory.path().join("child")).await?;
    let handle = fixture(directory.path(), Harness::ClaudeCode).await?;
    let service = &handle.service;
    let prepared = create(service, &directory.path().join("child/..")).await?;
    prompt(
        service,
        "prompt",
        Event::ClaudeEnqueued {
            command_id: "command".into(),
        },
    )
    .await?;
    let process = claude_source(service, &prepared, Some("native-before")).await?;
    let env = prepared
        .pointer("/_meta/claudeCode/options/env")
        .context("env")?;
    let spool = PathBuf::from(env[SPOOL_ENV].as_str().context("spool")?);
    let path = spool.join(format!("cli-{process}.jsonl"));
    let mut rows: Vec<Value> = tokio::fs::read_to_string(&path)
        .await?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let mut after = rows[2].clone();
    after["sequence"] = json!(3);
    after["payload"]["session_id"] = json!("native-after");
    rows.push(after);
    write_rows(&path, rows).await?;
    let observed = service.reconcile().await?;
    let attempt = observed.attempts.first().context("attempt")?;
    let membership = observed
        .attempt_executions
        .get(&attempt.id)
        .context(format!("membership missing: {:?}", observed.gaps))?;
    assert_eq!(membership.inputs.bindings.len(), 2);
    assert_eq!(membership.members().len(), 2);
    assert_eq!(
        membership.inputs.bindings[0].input,
        membership.inputs.bindings[1].input
    );
    let id = attempt
        .execution_snapshot
        .clone()
        .context("membership pointer")?;
    let strict = serde_json::to_value(service.store.attempt_executions(&id).await?)?;
    drop(handle);
    let reopened = fixture(directory.path(), Harness::ClaudeCode).await?;
    assert_eq!(
        serde_json::to_value(reopened.service.store.attempt_executions(&id).await?)?,
        strict
    );
    let recovered = reopened.service.reconcile().await?;
    assert_eq!(
        recovered
            .attempt_executions
            .get(&attempt.id)
            .context("recovered membership")?
            .members()
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn missing_original_tail_invalidates_a_previous_native_receipt() -> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: "turn".into(),
            role: "prompt".into(),
        },
    )
    .await?;
    codex_source(service, &service.spool, "native").await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    let before = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert_eq!(before["attempt"].bindings.len(), 1);
    let record = &before["attempt"].bindings[0].acknowledgements[0].record;
    let db = crate::db::connect(&format!(
        "sqlite:{}",
        directory.path().join("router/evidence.db").display()
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM native_evidence_records WHERE id = ?",
        [record.record_id.clone().into()],
    ))
    .await?;
    let after = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert!(after["attempt"].bindings.is_empty());
    assert!(
        after["attempt"]
            .gaps
            .contains("native_input_evidence_invalid")
    );
    Ok(())
}

async fn other_journal(
    service: &ControllerEvidence,
    directory: &Path,
    foreign: bool,
) -> Result<Journal> {
    let native = directory.join(if foreign {
        "foreign-profile"
    } else {
        "other-profile"
    });
    tokio::fs::create_dir_all(&native).await?;
    let native = tokio::fs::canonicalize(native).await?;
    let namespace = canonical_digest(&(Harness::Codex, &native))?;
    let controller = if foreign {
        uuid::Uuid::new_v4().to_string()
    } else {
        service.controller_id.clone()
    };
    let spool = service
        .spool
        .parent()
        .context("controllers directory")?
        .join(&controller)
        .join(format!(
            "root-{}",
            namespace
                .strip_prefix("sha256:")
                .context("namespace digest")?
        ));
    tokio::fs::create_dir_all(&spool).await?;
    let journal = Journal::new(
        service.store.clone(),
        SourceDescriptor {
            namespace: namespace.clone(),
            harness: Harness::Codex,
            format: SourceFormat::Acp,
            locator: format!("controller:{controller}"),
            node: None,
        },
        "@agentclientprotocol/codex-acp@1.10.0".into(),
    )
    .await?;
    let header = journal.append_record(json!({"method":"controller/root_registered","phase":"metadata","payload":{"namespace":namespace,"harness":"codex","native_root":native,"spool":spool}})).await?;
    service
        .recovered_root(
            &service
                .store
                .source(&header.source_id)
                .await?
                .context("profile journal")?,
        )
        .await?;
    Ok(journal)
}

#[tokio::test]
async fn native_claims_follow_original_prompts_across_profile_journals() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let template = prompt(
        service,
        "template",
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: "template".into(),
            role: "prompt".into(),
        },
    )
    .await?;
    let adapter = template
        .observations
        .first()
        .context("adapter")?
        .observation
        .adapter
        .clone();
    let journal = other_journal(service, directory.path(), false).await?;
    service
        .observe(observation(
            "moved",
            "session/prompt",
            "request",
            json!({"sessionId":"acp","prompt":[]}),
        ))
        .await?;
    let origin = service
        .store
        .prompt_origin(&service.controller_id, "moved")
        .await?
        .context("moved origin")?;
    let event = Event::CodexAccepted {
        thread_id: "native".into(),
        turn_id: "turn".into(),
        role: "prompt".into(),
    };
    let mut selected = PromptEvidence::default();
    for (sequence, event) in [
        Event::Started,
        event.clone(),
        Event::Finished {
            outcome: "returned".into(),
            notification_failures: 0,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let observation = Observation {
            schema: 1,
            session_id: "acp".into(),
            origin: origin.clone(),
            adapter: adapter.clone(),
            sequence: sequence as u32,
            event,
        };
        let record = journal.append_record(json!({"method":crate::session_evidence::adapter_bridge::METHOD,"phase":"notification","native_scope":"unresolved","payload":observation})).await?;
        selected.observations.push(ProvenObservation {
            record: RecordRef::from_record(&record)?,
            observation,
        });
    }
    service
        .observe(observation(
            "moved",
            "session/prompt",
            "response",
            json!({"stopReason":"end_turn"}),
        ))
        .await?;
    codex_source(service, &service.spool, "native").await?;
    let selected = BTreeMap::from([("attempt".into(), selected)]);
    let before = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert_eq!(
        before["attempt"].bindings.len(),
        1,
        "{:?}",
        before["attempt"].gaps
    );
    assert!(before["attempt"].gaps.is_empty());
    assert_ne!(
        before["attempt"].bindings[0].producer.range.source_id,
        origin.request.range.source_id
    );
    let mut competing = prompt(service, "competing", event).await?;
    competing
        .observations
        .retain(|proven| proven.observation.origin.operation_id == "competing");
    let after = service
        .native_inputs(
            &BTreeMap::from([("attempt".into(), competing)]),
            &BTreeSet::new(),
        )
        .await?;
    assert!(after["attempt"].bindings.is_empty());
    assert!(
        after["attempt"]
            .gaps
            .contains("native_input_producer_ambiguous")
    );
    Ok(())
}

#[tokio::test]
async fn unknown_old_producer_claims_remain_gaps_but_foreign_controllers_are_excluded() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: "turn".into(),
            role: "prompt".into(),
        },
    )
    .await?;
    codex_source(service, &service.spool, "native").await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    let invalid = json!({"method":crate::session_evidence::adapter_bridge::METHOD,"phase":"notification","native_scope":"unresolved","payload":{"invalid":true}});
    other_journal(service, directory.path(), true)
        .await?
        .append(invalid.clone())
        .await?;
    assert!(
        service.native_inputs(&selected, &BTreeSet::new()).await?["attempt"]
            .gaps
            .is_empty()
    );
    other_journal(service, directory.path(), false)
        .await?
        .append(invalid)
        .await?;
    let after = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert!(after["attempt"].gaps.contains("native_input_claim_unknown"));
    Ok(())
}

#[tokio::test]
async fn uncollected_spool_extent_survives_tail_removal_and_database_reopen() -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: "turn".into(),
            role: "prompt".into(),
        },
    )
    .await?;
    let id = codex_source(service, &service.spool, "native").await?;
    let source = service.store.source(&id).await?.context("source")?;
    let path = PathBuf::from(
        source
            .descriptor
            .locator
            .strip_prefix("spool:")
            .context("spool")?,
    );
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await?;
    for sequence in 3..132 {
        let mut bytes = serde_json::to_vec(
            &json!({"method":"thread/status/changed","phase":"notification","direction":"server","sequence":sequence,"payload":{"threadId":"native","status":{"type":"idle"}}}),
        )?;
        bytes.push(b'\n');
        file.write_all(&bytes).await?;
    }
    file.flush().await?;
    drop(file);
    let imported = import_spool(&service.store, &service.spool, &path, source.descriptor).await?;
    assert!(imported.gaps.contains("native_spool_backlog"));
    tokio::fs::remove_file(&path).await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    let after = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert!(
        after["attempt"]
            .gaps
            .contains("native_input_tail_uncollected")
    );
    drop(handle);
    let reopened = fixture(directory.path(), Harness::Codex).await?;
    let again = reopened
        .service
        .native_inputs(&selected, &BTreeSet::new())
        .await?;
    assert!(
        again["attempt"]
            .gaps
            .contains("native_input_tail_uncollected")
    );
    Ok(())
}

#[tokio::test]
async fn claude_failed_spawn_does_not_hide_a_later_healthy_input() -> Result<()> {
    use crate::session_evidence::claude_proxy::SPOOL_ENV;
    use tokio::io::AsyncWriteExt;
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::ClaudeCode).await?;
    let service = &handle.service;
    let prepared = create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::ClaudeEnqueued {
            command_id: "command".into(),
        },
    )
    .await?;
    let failed = claude_source(service, &prepared, None).await?;
    let healthy = claude_source(service, &prepared, Some("native")).await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    let before = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert!(
        before["attempt"].gaps.is_empty(),
        "{:?}",
        before["attempt"].gaps
    );
    assert_eq!(before["attempt"].bindings.len(), 1);
    assert_eq!(before["attempt"].bindings[0].process_id, healthy);
    let spool = PathBuf::from(
        prepared
            .pointer(&format!("/_meta/claudeCode/options/env/{SPOOL_ENV}"))
            .and_then(Value::as_str)
            .context("process spool")?,
    );
    let path = spool.join(format!("cli-{failed}.jsonl"));
    let failed_source = service
        .store
        .sources(None, 16)
        .await?
        .into_iter()
        .find(|source| source.descriptor.locator == format!("spool:{}", path.display()))
        .context("failed source")?;
    let mut raw = service
        .store
        .records(&SourceRange {
            source_id: failed_source.id.clone(),
            generation: "spool/1".into(),
            start: 0,
            end: 1,
        })
        .await?
        .into_iter()
        .next()
        .context("failed header")?
        .input
        .raw;
    drop(handle);
    let reopened = fixture(directory.path(), Harness::ClaudeCode).await?;
    let after = reopened
        .service
        .native_inputs(&selected, &BTreeSet::new())
        .await?;
    assert_eq!(after["attempt"].bindings.len(), 1);
    assert!(after["attempt"].gaps.is_empty());
    // A failed header cannot be used to excuse subsequent input as another
    // harmless failed-only process, even when the stored bytes are intact.
    raw["sequence"] = json!(1);
    raw["method"] = json!("runtime/input");
    raw["phase"] = json!("request");
    raw["direction"] = json!("client");
    raw["payload"] = json!({"type":"user","uuid":"command","session_id":"acp"});
    let mut bytes = serde_json::to_vec(&raw)?;
    bytes.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await?;
    file.write_all(&bytes).await?;
    file.flush().await?;
    drop(file);
    import_spool(
        &reopened.service.store,
        &spool,
        &path,
        failed_source.descriptor,
    )
    .await?;
    let invalid = reopened
        .service
        .native_inputs(&selected, &BTreeSet::new())
        .await?;
    assert!(invalid["attempt"].bindings.is_empty());
    assert!(
        invalid["attempt"]
            .gaps
            .contains("native_input_evidence_invalid")
    );
    Ok(())
}

#[tokio::test]
async fn missing_and_corrupt_extent_metadata_stays_visible_in_input_evidence() -> Result<()> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let directory = tempfile::tempdir()?;
    let handle = fixture(directory.path(), Harness::Codex).await?;
    let service = &handle.service;
    create(service, directory.path()).await?;
    let evidence = prompt(
        service,
        "prompt",
        Event::CodexAccepted {
            thread_id: "native".into(),
            turn_id: "turn".into(),
            role: "prompt".into(),
        },
    )
    .await?;
    let source_id = codex_source(service, &service.spool, "native").await?;
    let selected = BTreeMap::from([("attempt".into(), evidence)]);
    let db = crate::db::connect(&format!(
        "sqlite:{}",
        directory.path().join("router/evidence.db").display()
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "DELETE FROM native_evidence_objects WHERE kind = 'spool_extent' AND object_key = ?",
        [source_id.clone().into()],
    ))
    .await?;
    let missing = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert!(
        missing["attempt"]
            .gaps
            .contains("native_input_extent_unknown")
    );
    let source = service.store.source(&source_id).await?.context("source")?;
    service
        .store
        .observe_spool_extent(&source, source.cursor.offset)
        .await?;
    db.execute(Statement::from_sql_and_values(DbBackend::Sqlite, "UPDATE native_evidence_objects SET object_json = '{}' WHERE kind = 'spool_extent' AND object_key = ?", [source_id.into()])).await?;
    let corrupt = service.native_inputs(&selected, &BTreeSet::new()).await?;
    assert!(
        corrupt["attempt"]
            .gaps
            .contains("native_input_extent_invalid")
    );
    Ok(())
}
