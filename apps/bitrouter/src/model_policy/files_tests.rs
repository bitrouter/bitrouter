use super::*;
use bitrouter_ai::policy::{Document, Policy, SpecVersion, Target};

fn document(model: &str) -> Document {
    Document {
        spec_version: SpecVersion::Beta1,
        policies: [(
            "auto".into(),
            Policy {
                tiers: [(
                    "strong".into(),
                    Target {
                        model: model.into(),
                        effort: None,
                    },
                )]
                .into(),
                default_tier: "strong".into(),
                classifier_digest: None,
                routes: BTreeMap::new(),
                tool_use_tier: None,
                tool_safe_tiers: Vec::new(),
            },
        )]
        .into(),
    }
}

#[test]
fn current_source_history_cas_and_exact_rollback() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join(DEFAULT_POLICY_FILENAME);
    let history = default_history_dir(&path);
    let first = document("fixture:first");
    let second = document("fixture:second");
    let first_digest = write_atomic(&path, None, &first)?;
    let first_bytes = std::fs::read(&path)?;
    let record = publish_candidate(&path, &first_digest, &second, &history)?;
    assert_eq!(record.parent_digest, first_digest);
    assert_eq!(record.child_digest, semantic_digest(&second)?);
    assert_eq!(load_history_snapshot(&history, &first_digest)?, first);
    assert_eq!(
        load_history_snapshot(&history, &record.child_digest)?,
        second
    );
    let current_bytes = std::fs::read(&path)?;
    assert!(publish_candidate(&path, &first_digest, &first, &history).is_err());
    assert_eq!(std::fs::read(&path)?, current_bytes);
    assert!(rollback_to_digest(&path, &first_digest, &first_digest, &history).is_err());
    rollback_to_digest(&path, &record.child_digest, &first_digest, &history)?;
    assert_eq!(std::fs::read(&path)?, first_bytes);
    assert!(write_atomic(&path, None, &second).is_err());
    assert_eq!(std::fs::read(&path)?, first_bytes);
    Ok(())
}

#[tokio::test]
async fn source_file_rejects_v4_unknown_fields_and_duplicate_keys_without_mutation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join(DEFAULT_POLICY_FILENAME);
    for raw in [
        "lockfileVersion: 4\npolicies: {}\n",
        "spec_version: policy/1.0.0-beta.1\npolicies: {}\nartifact: {}\n",
        "spec_version: policy/1.0.0-beta.1\npolicies:\n  auto: {tiers: {strong: {model: 'fixture:one'}}, default_tier: strong}\n  auto: {tiers: {strong: {model: 'fixture:two'}}, default_tier: strong}\n",
    ] {
        std::fs::write(&path, raw)?;
        assert!(load(&path).await.is_err());
        assert!(
            write_atomic(
                &path,
                Some(&semantic_digest(&document("fixture:one"))?),
                &document("fixture:two")
            )
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&path)?, raw);
    }
    Ok(())
}

#[tokio::test]
async fn default_discovery_never_reads_old_filename_and_explicit_errors_do_not_fallback()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("bitrouter.yaml");
    std::fs::write(
        dir.path().join("policy-lock.yaml"),
        "lockfileVersion: 4\npolicies: {}\n",
    )?;
    let mut config = Config {
        inherit_defaults: false,
        ..Default::default()
    };
    assert!(
        load_for_config(&config, Some(&config_path))
            .await?
            .is_none()
    );
    config.inherit_defaults = true;
    let loaded = load_for_config(&config, Some(&config_path))
        .await?
        .context("bundled source")?;
    assert!(loaded.path.is_none());
    assert_eq!(loaded.document.policies["auto"].default_tier, "strong");
    assert_eq!(loaded.document.policies["auto"].tiers.len(), 3);
    config.policy.path = Some(PathBuf::from("missing.yaml"));
    assert!(load_for_config(&config, Some(&config_path)).await.is_err());
    let invalid = dir.path().join("invalid.yaml");
    std::fs::write(&invalid, "lockfileVersion: 4\npolicies: {}\n")?;
    config.policy.path = Some(invalid);
    assert!(load_for_config(&config, None).await.is_err());
    config.policy.path = None;
    config.tenancy.enabled = true;
    assert!(
        load_for_config(&config, Some(&config_path))
            .await?
            .is_none()
    );
    Ok(())
}

#[test]
fn history_refuses_old_format_tampering_and_path_escape() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let current = document("fixture:first");
    let digest = semantic_digest(&current)?;
    let path = history_snapshot_path(dir.path(), &digest)?;
    std::fs::write(&path, "lockfileVersion: 4\npolicies: {}\n")?;
    assert!(load_history_snapshot(dir.path(), &digest).is_err());
    std::fs::write(&path, deterministic_yaml(&document("fixture:tampered"))?)?;
    assert!(load_history_snapshot(dir.path(), &digest).is_err());
    assert!(load_history_snapshot(dir.path(), "sha256:../../escape").is_err());
    Ok(())
}

#[tokio::test]
async fn prepared_request_keeps_the_exact_source_generation_across_reload() -> Result<()> {
    use crate::gateway::{caller::CallerContext, pipeline::types::PipelineRequest};
    let config = Config {
        inherit_defaults: false,
        ..Default::default()
    };
    let runtime =
        PolicyRuntime::from_document(&config, Some(document("fixture:first")), Default::default())?;
    let input = crate::gateway::http::parse_input(
        &bitrouter_ai::types::ApiProtocol::ChatCompletions,
        serde_json::json!({"model":"bitrouter/auto","messages":[{"role":"user","content":"keep all context"}]}),
    )?;
    let prompt = input.generation_prompt().context("prompt")?.clone();
    let mut admitted = PipelineContext::new(PipelineRequest::new(
        "bitrouter/auto",
        CallerContext::local(),
        prompt.clone(),
    ));
    runtime.prepare_selection("auto", &mut admitted).await?;
    let next = document("fixture:second");
    runtime.commit(runtime.prepare_loaded(
        &config,
        Some(LoadedPolicy {
            path: None,
            digest: semantic_digest(&next)?,
            document: next,
        }),
    )?);
    runtime.select_policy("auto", &mut admitted).await?;
    assert_eq!(admitted.model(), "fixture:first");
    let mut fresh = PipelineContext::new(PipelineRequest::new(
        "bitrouter/auto",
        CallerContext::local(),
        prompt,
    ));
    runtime.prepare_selection("auto", &mut fresh).await?;
    runtime.select_policy("auto", &mut fresh).await?;
    assert_eq!(fresh.model(), "fixture:second");
    assert!(runtime.preserves_context());
    Ok(())
}

#[tokio::test]
async fn initialization_restores_source_when_configuration_publication_fails() -> Result<()> {
    for existing in [false, true] {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("bitrouter.yaml");
        let raw = "inherit_defaults: false\nregistry:\n  enabled: false\n";
        std::fs::write(&config_path, raw)?;
        let source_path = dir.path().join(DEFAULT_POLICY_FILENAME);
        let previous = if existing {
            write_atomic(&source_path, None, &document("fixture:first"))?;
            Some(std::fs::read(&source_path)?)
        } else {
            None
        };
        let error = initialize_with_writer(
            &config_path,
            PolicyInitRequest {
                policy_name: "coding",
                router_name: "coding",
                strong_model: Some("fixture:strong"),
                strong_effort: None,
                economy_model: "fixture:small",
                economy_effort: None,
            },
            |_, _, _| anyhow::bail!("injected config write failure"),
        )
        .await
        .err()
        .context("config write failure was ignored")?;
        assert!(error.to_string().contains("restored policy"));
        assert_eq!(std::fs::read_to_string(&config_path)?, raw);
        if let Some(previous) = previous {
            assert_eq!(std::fs::read(&source_path)?, previous);
        } else {
            assert!(!source_path.exists());
        }
    }
    Ok(())
}

#[test]
fn failed_promotion_record_restores_active_source() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join(DEFAULT_POLICY_FILENAME);
    let history = default_history_dir(&path);
    let digest = write_atomic(&path, None, &document("fixture:first"))?;
    let bytes = std::fs::read(&path)?;
    std::fs::create_dir_all(history.join("promotions.jsonl"))?;
    assert!(publish_candidate(&path, &digest, &document("fixture:second"), &history).is_err());
    assert_eq!(std::fs::read(&path)?, bytes);
    assert_eq!(
        load_history_snapshot(&history, &digest)?,
        document("fixture:first")
    );
    Ok(())
}

#[test]
fn source_identity_ignores_presentation_and_diff_includes_tool_and_default_changes() -> Result<()> {
    let first = document("fixture:first");
    let rendered = format!("# authored source\n{}", deterministic_yaml(&first)?);
    assert_eq!(
        semantic_digest(&crate::model_policy::decode(&rendered)?)?,
        semantic_digest(&first)?
    );
    let mut second = first.clone();
    let policy = second.policies.get_mut("auto").context("auto")?;
    policy.tiers.insert(
        "small".into(),
        Target {
            model: "fixture:small".into(),
            effort: None,
        },
    );
    policy.default_tier = "small".into();
    policy.tool_use_tier = Some("strong".into());
    policy.tool_safe_tiers.push("strong".into());
    assert!(!diff_explanations(&first, &second).is_empty());
    assert_ne!(semantic_digest(&first)?, semantic_digest(&second)?);
    Ok(())
}

#[test]
fn bundled_defaults_do_not_inject_a_tenant_router_or_override_explicit_routing() {
    let mut tenant = Config::default();
    tenant.tenancy.enabled = true;
    apply_defaults(&mut tenant, None);
    assert!(tenant.routers.is_empty());
    let mut local = Config::default();
    apply_defaults(&mut local, None);
    assert!(local.routers.contains_key("auto"));
    let mut explicit = Config::default();
    explicit.policy.path = Some(PathBuf::from("authored.yaml"));
    apply_defaults(&mut explicit, None);
    assert!(explicit.routers.is_empty());
}
