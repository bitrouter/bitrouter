use super::*;
use crate::session_evidence::claude_proxy::{NAMESPACE_ENV, ORIGIN_ENV, SPOOL_ENV};
use crate::session_evidence::service::tests::{claude_service, observation, write_rows};
use sea_orm::{ConnectionTrait, DbBackend, Statement};

async fn service(directory: &Path) -> Result<EvidenceHandle> {
    let mut handle = claude_service(directory).await?;
    handle.worker.abort();
    if let Err(error) = (&mut handle.worker).await {
        ensure!(error.is_cancelled(), "fixture worker failed");
    }
    Ok(handle)
}

fn params(directory: &Path, profile: &str) -> Value {
    json!({"cwd":directory,"mcpServers":[],"_meta":{"claudeCode":{"options":{
        "env":{"CLAUDE_CONFIG_DIR":directory.join(profile),"API_KEY":"fixture-secret"}}}}})
}

async fn prepare(
    service: &ControllerEvidence,
    operation: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    // Match the controller's observation contract: credentials and arbitrary
    // SDK options are not part of the durable original request envelope.
    let mut observed = params.clone();
    observed
        .as_object_mut()
        .context("request object")?
        .remove("_meta");
    service
        .observe(observation(operation, method, "request", observed))
        .await?;
    Ok(service
        .prepare_session_request(operation, method, params)
        .await?)
}

fn configuration(params: &Value) -> Result<RecordRef> {
    Ok(serde_json::from_str(
        params
            .pointer(&format!("/_meta/claudeCode/options/env/{ORIGIN_ENV}"))
            .and_then(Value::as_str)
            .context("configuration env")?,
    )?)
}

async fn capture(
    service: &ControllerEvidence,
    params: &Value,
    reference: Option<RecordRef>,
) -> Result<(ProcessBinding, PathBuf)> {
    let env = params
        .pointer("/_meta/claudeCode/options/env")
        .context("process env")?;
    let process_id = uuid::Uuid::new_v4().to_string();
    let path = PathBuf::from(env[SPOOL_ENV].as_str().context("process spool")?)
        .join(format!("cli-{process_id}.jsonl"));
    let status = if reference.is_some() {
        "present"
    } else {
        "missing"
    };
    let mut rows = vec![
        json!({"method":"runtime/started","phase":"metadata","configured_by":reference,"configuration_status":status}),
        json!({"method":"runtime/message","payload":{"type":"system","subtype":"init","session_id":"early-session","claude_code_version":"2.1.257"}}),
    ];
    for (sequence, row) in rows.iter_mut().enumerate() {
        row["process_id"] = json!(process_id);
        row["namespace"] = env[NAMESPACE_ENV].clone();
        row["scope_valid"] = json!(true);
        row["sequence"] = json!(sequence);
    }
    write_rows(&path, rows).await?;
    for _ in 0..256 {
        let snapshot = service.reconcile().await?;
        if let Some(binding) = snapshot
            .processes
            .into_iter()
            .find(|binding| binding.process_id.as_deref() == Some(process_id.as_str()))
        {
            return Ok((binding, path));
        }
    }
    anyhow::bail!("captured process did not reach a completed inventory")
}

async fn recover(service: &ControllerEvidence) -> Result<CollectionSnapshot> {
    for _ in 0..256 {
        let snapshot = service.reconcile().await?;
        if !snapshot.gaps.contains("native_recovery_backlog") {
            return Ok(snapshot);
        }
    }
    anyhow::bail!("fixture recovery did not converge")
}

#[tokio::test]
async fn configuration_origin_is_durable_before_response_and_survives_spool_removal() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let origin = service(directory.path()).await?;
    let prepared = prepare(
        &origin.service,
        "create",
        "session/new",
        params(directory.path(), "profile"),
    )
    .await?;
    let reference = configuration(&prepared)?;
    let (first, first_path) = capture(&origin.service, &prepared, Some(reference.clone())).await?;
    let (second, second_path) =
        capture(&origin.service, &prepared, Some(reference.clone())).await?;
    assert!(first.gaps.is_empty(), "{:?}", first.gaps);
    assert!(second.gaps.is_empty(), "{:?}", second.gaps);
    assert_ne!(first.process_id, second.process_id);
    let configured = first
        .configured_by
        .as_ref()
        .context("configuration binding")?;
    assert_eq!(configured.controller_id, origin.service.controller_id);
    assert_eq!(configured.operation_id, "create");
    assert_eq!(configured.configuration, reference);
    assert_eq!(
        configured.request,
        second
            .configured_by
            .as_ref()
            .context("recreated configuration")?
            .request
    );
    let (_, request) = referenced_record(&origin.service.store, &configured.request).await?;
    let (_, config) = referenced_record(&origin.service.store, &reference).await?;
    assert!(!serde_json::to_string(&request)?.contains("fixture-secret"));
    assert!(!serde_json::to_string(&config)?.contains("fixture-secret"));
    assert!(origin.service.state.lock().await.loaded.is_empty());
    assert!(origin.service.snapshot().await.attempts.is_empty());
    // The process is known even though no lifecycle response or prompt has
    // arrived. Configuration origin is deliberately not task membership.
    std::fs::remove_file(first_path)?;
    std::fs::remove_file(second_path)?;
    drop(origin);
    let resumed = service(directory.path()).await?;
    let snapshot = recover(&resumed.service).await?;
    assert_eq!(snapshot.processes.len(), 2);
    for binding in snapshot.processes {
        assert!(binding.gaps.is_empty(), "{:?}", binding.gaps);
        assert_eq!(
            binding
                .configured_by
                .context("recovered origin")?
                .configuration,
            reference
        );
    }
    assert!(resumed.service.state.lock().await.loaded.is_empty());
    Ok(())
}

#[tokio::test]
async fn changed_profile_prepares_a_new_origin_without_rebinding_reused_processes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let first = prepare(
        &handle.service,
        "first",
        "session/new",
        params(directory.path(), "profile-a"),
    )
    .await?;
    handle
        .service
        .observe(observation(
            "first",
            "session/new",
            "response",
            json!({"sessionId":"same-session"}),
        ))
        .await?;
    let mut request = params(directory.path(), "profile-b");
    request["sessionId"] = json!("same-session");
    let second = prepare(&handle.service, "reload", "session/load", request).await?;
    let a = configuration(&first)?;
    let b = configuration(&second)?;
    assert_ne!(a.range.source_id, b.range.source_id);
    let (reused, _) = capture(&handle.service, &first, Some(a)).await?;
    let (replacement, _) = capture(&handle.service, &second, Some(b.clone())).await?;
    assert_eq!(
        reused
            .configured_by
            .context("original configuration")?
            .operation_id,
        "first"
    );
    assert_eq!(
        replacement
            .configured_by
            .context("replacement configuration")?
            .operation_id,
        "reload"
    );
    assert!(
        handle
            .service
            .state
            .lock()
            .await
            .uncertain_queries
            .contains("same-session")
    );
    // A valid reference from another profile cannot authorize the old process.
    let (wrong, _) = capture(&handle.service, &first, Some(b)).await?;
    assert!(wrong.configured_by.is_none());
    assert!(wrong.gaps.contains("native_process_configuration_invalid"));
    Ok(())
}

#[tokio::test]
async fn configuration_binding_rejects_cross_controller_requests_and_mismatched_parameters()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let left = service(directory.path()).await?;
    let right = service(directory.path()).await?;
    let a = prepare(
        &left.service,
        "same-operation",
        "session/new",
        params(directory.path(), "profile"),
    )
    .await?;
    let b = prepare(
        &right.service,
        "same-operation",
        "session/new",
        params(directory.path(), "profile"),
    )
    .await?;
    let (_, original) = referenced_record(&left.service.store, &configuration(&a)?).await?;
    let (_, foreign) = referenced_record(&left.service.store, &configuration(&b)?).await?;
    let namespace = a
        .pointer(&format!("/_meta/claudeCode/options/env/{NAMESPACE_ENV}"))
        .and_then(Value::as_str)
        .context("namespace")?;
    let context = left
        .service
        .state
        .lock()
        .await
        .roots
        .get(namespace)
        .context("profile")?
        .clone();
    for fault in [
        "controller",
        "method",
        "operation",
        "cwd",
        "session",
        "digest",
    ] {
        let mut raw = original.input.raw.clone();
        match fault {
            "controller" => {
                raw["payload"]["request"] = foreign.input.raw["payload"]["request"].clone()
            }
            "method" => raw["payload"]["method"] = json!("session/resume"),
            "operation" => raw["operation_id"] = json!("different"),
            "cwd" => raw["payload"]["cwd"] = json!(directory.path().join("different")),
            "session" => raw["payload"]["sessionId"] = json!("different"),
            _ => {
                raw["payload"]["request"]["record_digest"] = json!(canonical_digest(&"different")?)
            }
        }
        let reference = RecordRef::from_record(&context.journal.append_record(raw).await?)?;
        let (binding, _) = capture(&left.service, &a, Some(reference)).await?;
        assert!(binding.process_id.is_some());
        assert!(binding.configured_by.is_none(), "{fault}");
        assert!(
            binding
                .gaps
                .contains("native_process_configuration_invalid"),
            "{fault}"
        );
    }
    let (wrong_spool, _) = capture(&left.service, &a, Some(configuration(&b)?)).await?;
    assert!(wrong_spool.configured_by.is_none());
    let (legacy, _) = capture(&left.service, &a, None).await?;
    assert!(legacy.gaps.contains("native_process_configuration_missing"));
    Ok(())
}

#[tokio::test]
async fn each_binding_read_rechecks_original_records_and_owner() -> Result<()> {
    for fault in ["request", "configuration", "header", "registration"] {
        let directory = tempfile::tempdir()?;
        let handle = service(directory.path()).await?;
        let prepared = prepare(
            &handle.service,
            "create",
            "session/new",
            params(directory.path(), "profile"),
        )
        .await?;
        let reference = configuration(&prepared)?;
        let (valid, _) = capture(&handle.service, &prepared, Some(reference.clone())).await?;
        let configured = valid.configured_by.as_ref().context("valid origin")?;
        let db = crate::db::connect(&crate::db::anchor_url(
            "sqlite:evidence.db?mode=rwc",
            &directory.path().join("router"),
        ))
        .await?;
        let foreign = EvidenceStore::new(db.clone(), "another-owner")?;
        assert!(referenced_record(&foreign, &reference).await.is_err());
        let id = match fault {
            "request" => configured.request.record_id.clone(),
            "configuration" => configured.configuration.record_id.clone(),
            "header" => valid.header.as_ref().context("header")?.record_id.clone(),
            _ => handle
                .service
                .store
                .records(&SourceRange {
                    source_id: reference.range.source_id.clone(),
                    generation: "controller/1".into(),
                    start: 0,
                    end: 1,
                })
                .await?
                .first()
                .context("registration")?
                .id
                .clone(),
        };
        db.execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "UPDATE native_evidence_records SET digest = ? WHERE id = ?",
            [canonical_digest(&"corrupt")?.into(), id.into()],
        ))
        .await?;
        for _ in 0..2 {
            let invalid = handle
                .service
                .process_binding(valid.source_id.clone())
                .await;
            assert!(invalid.configured_by.is_none(), "{fault}");
            assert!(!invalid.gaps.is_empty(), "{fault}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn original_lifecycle_response_binds_acp_identity_without_claiming_current_conversation()
-> Result<()> {
    for (method, response, expected, error) in [
        (
            "session/new",
            json!({"sessionId":"created"}),
            Some("created"),
            None,
        ),
        ("session/load", json!({}), Some("requested"), None),
        ("session/resume", json!({}), Some("requested"), None),
        (
            "session/fork",
            json!({"sessionId":"forked"}),
            Some("forked"),
            None,
        ),
        (
            "session/load",
            json!({"error_code":-32603}),
            None,
            Some(-32603),
        ),
    ] {
        let directory = tempfile::tempdir()?;
        let handle = service(directory.path()).await?;
        let mut request = params(directory.path(), "profile");
        if method != "session/new" {
            request["sessionId"] = json!("requested");
        }
        let prepared = prepare(&handle.service, "create", method, request).await?;
        let reference = configuration(&prepared)?;
        let (before, path) = capture(&handle.service, &prepared, Some(reference.clone())).await?;
        assert!(before.session_response.is_none());
        handle
            .service
            .observe(observation("create", method, "response", response))
            .await?;
        let after = handle
            .service
            .process_binding(before.source_id.clone())
            .await;
        assert!(after.gaps.is_empty(), "{method}: {:?}", after.gaps);
        let outcome = after
            .session_response
            .context("original lifecycle response")?;
        assert_eq!(outcome.acp_session_id.as_deref(), expected);
        assert_eq!(outcome.error_code, error);
        // The transport fixture emits early-session as the native id. The
        // recorded ACP id is an attachment to that configuration, not a claim
        // that subsequent native conversation ids must equal the ACP alias.
        let (recreated, another_path) =
            capture(&handle.service, &prepared, Some(reference)).await?;
        assert_eq!(
            recreated
                .session_response
                .context("saved creation outcome")?
                .record,
            outcome.record
        );
        assert_ne!(before.process_id, recreated.process_id);
        std::fs::remove_file(path)?;
        std::fs::remove_file(another_path)?;
        drop(handle);
        let resumed = service(directory.path()).await?;
        let snapshot = recover(&resumed.service).await?;
        let restored = snapshot
            .processes
            .iter()
            .find(|process| process.source_id == before.source_id)
            .context("restored process")?;
        assert_eq!(
            restored
                .session_response
                .as_ref()
                .context("restored response")?
                .record,
            outcome.record
        );
    }
    Ok(())
}

#[tokio::test]
async fn fork_without_returned_child_id_and_corrupt_response_never_bind_a_session() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let handle = service(directory.path()).await?;
    let mut request = params(directory.path(), "profile");
    request["sessionId"] = json!("parent");
    let prepared = prepare(&handle.service, "fork", "session/fork", request).await?;
    let (process, _) = capture(&handle.service, &prepared, Some(configuration(&prepared)?)).await?;
    handle
        .service
        .observe(observation("fork", "session/fork", "response", json!({})))
        .await?;
    let invalid = handle.service.process_binding(process.source_id).await;
    assert!(invalid.session_response.is_none());
    assert!(invalid.gaps.contains("native_process_lifecycle_invalid"));

    let prepared = prepare(
        &handle.service,
        "new",
        "session/new",
        params(directory.path(), "profile"),
    )
    .await?;
    let (process, _) = capture(&handle.service, &prepared, Some(configuration(&prepared)?)).await?;
    handle
        .service
        .observe(observation(
            "new",
            "session/new",
            "response",
            json!({"sessionId":"created"}),
        ))
        .await?;
    let bound = handle
        .service
        .process_binding(process.source_id.clone())
        .await;
    let response = bound.session_response.context("valid response")?;
    let db = crate::db::connect(&crate::db::anchor_url(
        "sqlite:evidence.db?mode=rwc",
        &directory.path().join("router"),
    ))
    .await?;
    db.execute(Statement::from_sql_and_values(
        DbBackend::Sqlite,
        "UPDATE native_evidence_records SET digest = ? WHERE id = ?",
        [
            canonical_digest(&"corrupt")?.into(),
            response.record.record_id.into(),
        ],
    ))
    .await?;
    let invalid = handle.service.process_binding(process.source_id).await;
    assert!(
        invalid.configured_by.is_some(),
        "valid configuration remains visible"
    );
    assert!(invalid.session_response.is_none());
    assert!(invalid.gaps.contains("native_process_lifecycle_invalid"));
    Ok(())
}
