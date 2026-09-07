use super::*;
use crate::session_evidence::service::tests::observation;

#[tokio::test]
async fn prompt_origin_requires_loaded_capability_and_replaces_stale_claims() -> Result<()> {
    let pins: Value = serde_json::from_str(include_str!("../../adapter_bridge/pins.json"))?;
    for (harness_id, key) in [("codex-acp", "codex"), ("claude-acp", "claude")] {
        let directory = tempfile::tempdir()?;
        let pin = &pins[key];
        let identity = ControllerIdentity::new(
            harness_id,
            pin["package"].as_str().context("package")?,
            pin["version"].as_str().context("version")?,
        );
        let mut env = HashMap::from([
            (
                "CODEX_HOME".into(),
                directory
                    .path()
                    .join("codex")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".into(),
                directory
                    .path()
                    .join("claude")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]);
        let mut handle = EvidenceHandle::open(EvidenceLaunch {
            home: &directory.path().join("router"),
            database_url: "sqlite:evidence.db?mode=rwc",
            identity: &identity,
            env: &mut env,
            strip_inherited_env: &[],
        })
        .await?
        .context("evidence")?;
        let service = &handle.service;
        let mut stale = json!({"sessionId":"acp", "prompt":[], "_meta":{"user":"kept"}});
        stale["_meta"][adapter_bridge::META_KEY] = json!({"origin":"old-operation"});
        let clean = json!({"sessionId":"acp", "prompt":[], "_meta":{"user":"kept"}});
        assert_eq!(
            service
                .prepare_session_request("unknown", "session/prompt", stale.clone())
                .await?,
            clean
        );
        let capability = json!({"schema":1, "adapter":{"package":pin["package"], "version":pin["version"], "moduleDigest":pin["moduleDigest"]}});
        let mut metadata = agent_client_protocol::schema::v1::Meta::new();
        metadata.insert(adapter_bridge::META_KEY.into(), capability.clone());
        metadata.insert("private-config".into(), json!("excluded"));
        let selected = service
            .initialization_metadata(Some(&metadata))
            .context("selected capability")?;
        assert_eq!(selected.as_object().context("metadata")?.len(), 1);
        metadata
            .get_mut(adapter_bridge::META_KEY)
            .context("capability")?["adapter"]["moduleDigest"] = json!("different-module");
        assert!(service.initialization_metadata(Some(&metadata)).is_none());
        service
            .observe(observation(
                "init",
                "initialize",
                "response",
                json!({"_meta":selected}),
            ))
            .await?;
        // Capability alone cannot turn an unobserved operation into provenance.
        assert_eq!(
            service
                .prepare_session_request("unknown", "session/prompt", stale.clone())
                .await?,
            clean
        );
        service
            .observe(observation(
                "new",
                "session/new",
                "request",
                json!({"cwd":directory.path()}),
            ))
            .await?;
        service
            .observe(observation(
                "new",
                "session/new",
                "response",
                json!({"sessionId":"acp"}),
            ))
            .await?;
        service
            .observe(observation(
                "prompt",
                "session/prompt",
                "request",
                json!({"sessionId":"acp", "prompt":[]}),
            ))
            .await?;
        let original = service
            .store
            .prompt_origin(&service.controller_id, "prompt")
            .await?
            .context("origin")?;
        for (state, metadata) in [
            ("absent", None),
            ("null", Some(Value::Null)),
            ("object", Some(stale["_meta"].clone())),
        ] {
            let mut params = json!({"sessionId":"acp", "prompt":[]});
            if let Some(metadata) = metadata {
                params["_meta"] = metadata;
            }
            // A prompt has no cwd and must not enter Claude Query preparation.
            let prepared = service
                .prepare_session_request("prompt", "session/prompt", params)
                .await?;
            assert_eq!(
                prepared["_meta"][adapter_bridge::META_KEY]["origin"],
                serde_json::to_value(&original)?
            );
            assert_eq!(
                prepared["_meta"][adapter_bridge::META_KEY]["metaState"],
                state
            );
            if state == "object" {
                assert_eq!(prepared["_meta"]["user"], "kept");
            }
        }
        // A later unsupported initialization disables even an otherwise valid origin.
        service
            .observe(observation("init2", "initialize", "response", json!({})))
            .await?;
        assert_eq!(
            service
                .prepare_session_request("prompt", "session/prompt", stale)
                .await?,
            clean
        );
        handle.shutdown().await?;
    }
    Ok(())
}
