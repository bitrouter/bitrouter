//! Claude Code's custom option and Claude Desktop's separate gateway profile.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::files::{self, Change};

const PROFILE: &str = "6c00b780-eab9-4f8f-a51e-4fc1c363b8cd";
const CONFLICT_ENV: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_CUSTOM_HEADERS",
];

pub(super) fn check_environment() -> Result<()> {
    for name in CONFLICT_ENV.iter().copied().chain([
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_CUSTOM_MODEL_OPTION",
    ]) {
        ensure!(
            std::env::var(name)
                .ok()
                .is_none_or(|value| value.is_empty() || value == "0"),
            "shell environment sets {name}; unset it for persistent connection setup (value redacted)"
        );
    }
    Ok(())
}

pub(super) fn cli(
    home: &Path,
    endpoint: &str,
    token: &str,
    model: &str,
    display_name: &str,
) -> Result<Change> {
    let path = home.join("settings.json");
    let (mut settings, before) = files::json_snapshot(&path)?;
    ensure!(
        settings.get("apiKeyHelper").is_none(),
        "Claude apiKeyHelper overrides gateway auth; resolve it before connecting"
    );
    let object = settings
        .as_object_mut()
        .context("Claude settings must be an object")?;
    let env = object
        .entry("env")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude env must be an object")?;
    for name in CONFLICT_ENV {
        ensure!(
            env.get(*name)
                .is_none_or(|v| v == "" || v == "0" || v == false),
            "Claude settings contain {name}; resolve this provider/auth override before connecting (value redacted)"
        );
    }
    ensure!(
        env.get("ANTHROPIC_CUSTOM_MODEL_OPTION")
            .is_none_or(|v| v == model),
        "Claude already has another custom model option; preserve it with modelPicker.options before connecting"
    );
    // A persisted upstream API key must not follow the gateway URL.
    env.insert("ANTHROPIC_API_KEY".into(), json!(""));
    env.insert("ANTHROPIC_BASE_URL".into(), json!(endpoint));
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(token));
    env.insert("ANTHROPIC_CUSTOM_MODEL_OPTION".into(), json!(model));
    env.insert(
        "ANTHROPIC_CUSTOM_MODEL_OPTION_NAME".into(),
        json!(display_name),
    );
    env.insert(
        "ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION".into(),
        json!("Gateway model connected through BitRouter"),
    );
    env.insert(
        "ANTHROPIC_CUSTOM_HEADERS".into(),
        json!("x-bitrouter-harness: claude-acp"),
    );
    Change::json(path, before, &settings)
}

pub(super) fn desktop(
    home: &Path,
    endpoint: &str,
    token: &str,
    model: &str,
    display_name: &str,
) -> Result<Vec<Change>> {
    // configLibrary schema and key sealing checked against Desktop 4.4212.0. Profiles
    // contain flat documented gateway keys; _meta selects the applied UUID.
    let library = home.join("configLibrary");
    let meta_path = library.join("_meta.json");
    let (mut meta, meta_before) = files::json_snapshot(&meta_path)?;
    ensure!(
        meta.get("hybridPointer").is_none(),
        "Claude Desktop uses an organization bootstrap; change it in the app before connecting"
    );
    let mut models = vec![];
    let mut profile = json!({});
    if let Some(id) = meta.get("appliedId").and_then(Value::as_str) {
        let id =
            uuid::Uuid::parse_str(id).context("invalid applied Claude Desktop configuration id")?;
        let previous = files::json(&library.join(format!("{id}.json")))?;
        ensure!(
            previous.get("bootstrapUrl").is_none(),
            "Claude Desktop uses a bootstrap configuration; change it in the app before connecting"
        );
        if let Some(existing) = previous.get("inferenceModels") {
            let parsed = match existing.as_str() {
                Some(text) => serde_json::from_str::<Value>(text)?,
                None => existing.clone(),
            };
            models = parsed
                .as_array()
                .context("Claude inferenceModels must be an array")?
                .clone();
        }
        // Keep application/tool policies. Provider credentials and helpers
        // belong to the previous inference receiver.
        profile = Value::Object(
            previous
                .as_object()
                .context("invalid desktop profile")?
                .iter()
                .filter(|(key, _)| !key.starts_with("inference"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
    }
    if let Some(existing) = models
        .iter_mut()
        .find(|m| m.as_str() == Some(model) || m.get("name").and_then(Value::as_str) == Some(model))
    {
        if existing.is_string() {
            *existing = json!({"name": model, "labelOverride": display_name});
        } else {
            existing["labelOverride"] = json!(display_name);
        }
    } else {
        models.push(json!({"name": model, "labelOverride": display_name}));
    }
    let profile_path = library.join(format!("{PROFILE}.json"));
    ensure!(
        !profile_path.exists(),
        "unmanaged BitRouter Claude Desktop profile already exists"
    );
    let gateway = json!({
        "inferenceProvider": "gateway",
        "inferenceGatewayBaseUrl": endpoint,
        "inferenceGatewayApiKey": token,
        "inferenceGatewayAuthScheme": "bearer",
        "inferenceCustomHeaders": {"x-bitrouter-harness": "claude-acp"},
        "inferenceModels": models
    });
    profile
        .as_object_mut()
        .context("invalid new desktop profile")?
        .extend(
            gateway
                .as_object()
                .context("invalid gateway profile")?
                .clone(),
        );
    let entries = meta
        .as_object_mut()
        .context("invalid Claude Desktop metadata")?
        .entry("entries")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("Claude Desktop metadata entries must be an array")?;
    ensure!(
        !entries
            .iter()
            .any(|e| e.get("id").and_then(Value::as_str) == Some(PROFILE)),
        "unmanaged BitRouter Claude Desktop entry already exists"
    );
    entries.push(json!({"id": PROFILE, "name": "BitRouter"}));
    meta["appliedId"] = json!(PROFILE);
    let mode_path = home.join("claude_desktop_config.json");
    let (mut mode, mode_before) = files::json_snapshot(&mode_path)?;
    mode["deploymentMode"] = json!("3p");
    Ok(vec![
        Change::json(profile_path, None, &profile)?.with_restore(files::Restore::ClaudeProfile),
        Change::json(meta_path, meta_before, &meta)?.with_restore(files::Restore::ClaudeMeta {
            profile: PROFILE.into(),
        }),
        Change::json(mode_path, mode_before, &mode)?.with_restore(files::Restore::ClaudeMode),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    const MODEL: &str = "team/coding-v2";

    #[test]
    fn cli_preserves_default_permissions_and_replaces_only_gateway_credentials() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let path = root.join("settings.json");
        let original = json!({"model":"sonnet","permissions":{"deny":["Bash(rm *)"]},
            "env":{"ANTHROPIC_API_KEY":"upstream-secret","USER_FLAG":"keep"}});
        std::fs::write(&path, original.to_string())?;
        let change = cli(
            &root,
            "http://127.0.0.1:4356",
            "gateway-secret",
            MODEL,
            "BitRouter test",
        )?;
        let journal = root.join("receipt.json");
        let receipt = super::super::files::Receipt::new(
            "http://127.0.0.1:4356".into(),
            MODEL.into(),
            vec![change],
        );
        receipt.apply(&journal)?;
        let settings = files::json(&path)?;
        assert_eq!(settings["model"], "sonnet");
        assert_eq!(settings["permissions"], original["permissions"]);
        assert_eq!(settings["env"]["ANTHROPIC_CUSTOM_MODEL_OPTION"], MODEL);
        assert_eq!(
            settings["env"]["ANTHROPIC_CUSTOM_MODEL_OPTION_NAME"],
            "BitRouter test"
        );
        assert_eq!(settings["env"]["ANTHROPIC_API_KEY"], "");
        assert_eq!(settings["env"]["USER_FLAG"], "keep");
        receipt.restore(&journal)?;
        assert_eq!(files::json(&path)?, original);
        Ok(())
    }

    #[test]
    fn desktop_keeps_previous_profile_and_restores_selection() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let library = root.join("configLibrary");
        std::fs::create_dir(&library)?;
        let previous = "0c67873c-3236-44a0-b62f-9e512738a22b";
        let meta = json!({"appliedId":previous,"entries":[{"id":previous,"name":"Work"}]});
        std::fs::write(library.join("_meta.json"), meta.to_string())?;
        let old_profile = json!({"inferenceModels":["claude-sonnet-test"],
            "inferenceGatewayApiKey":"old-key","inferenceCredentialHelper":"old-helper",
            "autoModeEnabled":false});
        let old_path = library.join(format!("{previous}.json"));
        std::fs::write(&old_path, old_profile.to_string())?;
        let receipt = super::super::files::Receipt::new(
            "http://localhost".into(),
            MODEL.into(),
            desktop(
                &root,
                "http://localhost",
                "new-key",
                MODEL,
                "BitRouter test",
            )?,
        );
        let journal = root.join("receipt.json");
        receipt.apply(&journal)?;
        assert_eq!(files::json(&old_path)?, old_profile);
        let profile = files::json(&library.join(format!("{PROFILE}.json")))?;
        assert_eq!(profile["inferenceModels"][0], "claude-sonnet-test");
        assert_eq!(profile["inferenceModels"][1]["name"], MODEL);
        assert_eq!(
            profile["inferenceModels"][1]["labelOverride"],
            "BitRouter test"
        );
        assert_eq!(profile["autoModeEnabled"], false);
        assert!(profile.get("inferenceCredentialHelper").is_none());
        assert_eq!(
            files::json(&root.join("claude_desktop_config.json"))?["deploymentMode"],
            "3p"
        );
        receipt.restore(&journal)?;
        assert_eq!(files::json(&library.join("_meta.json"))?, meta);
        assert!(!root.join("claude_desktop_config.json").exists());
        Ok(())
    }

    #[test]
    fn auth_overrides_fail_before_mutation() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let path = root.join("settings.json");
        let original = json!({"apiKeyHelper":"secret-helper"});
        std::fs::write(&path, original.to_string())?;
        assert!(cli(&root, "http://localhost", "token", MODEL, "BitRouter test").is_err());
        assert_eq!(files::json(&path)?, original);
        Ok(())
    }

    #[test]
    fn desktop_sealing_and_preferences_survive_restart_and_remove() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let receipt = files::Receipt::new(
            "http://localhost".into(),
            MODEL.into(),
            desktop(&root, "http://localhost", "key", MODEL, "BitRouter")?,
        );
        let journal = root.join("receipt.json");
        receipt.apply(&journal)?;
        let profile_path = root.join(format!("configLibrary/{PROFILE}.json"));
        let mut profile = files::json(&profile_path)?;
        profile["inferenceGatewayApiKey"] = json!("__sealed__:fixture");
        std::fs::write(&profile_path, profile.to_string())?;
        let mode_path = root.join("claude_desktop_config.json");
        let mut mode = files::json(&mode_path)?;
        mode["preferences"] = json!({"theme":"dark"});
        mode["_cfprefsMigrated"] = json!(true);
        std::fs::write(&mode_path, mode.to_string())?;
        let meta_path = root.join("configLibrary/_meta.json");
        let mut meta = files::json(&meta_path)?;
        meta["entries"]
            .as_array_mut()
            .context("entries")?
            .push(json!({"id":"another-profile","name":"Later"}));
        std::fs::write(&meta_path, meta.to_string())?;
        assert!(receipt.installed()?);
        receipt.restore(&journal)?;
        assert!(!profile_path.exists());
        let restored = files::json(&mode_path)?;
        assert_eq!(restored["preferences"], mode["preferences"]);
        assert_eq!(restored["_cfprefsMigrated"], true);
        assert!(restored.get("deploymentMode").is_none());
        let meta = files::json(&meta_path)?;
        assert!(meta.get("appliedId").is_none());
        assert_eq!(
            meta["entries"],
            json!([{"id":"another-profile","name":"Later"}])
        );
        Ok(())
    }

    #[test]
    fn desktop_user_profile_choice_is_not_reverted() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let receipt = files::Receipt::new(
            "http://localhost".into(),
            MODEL.into(),
            desktop(&root, "http://localhost", "key", MODEL, "BitRouter")?,
        );
        let journal = root.join("receipt.json");
        receipt.apply(&journal)?;
        let meta_path = root.join("configLibrary/_meta.json");
        let mut meta = files::json(&meta_path)?;
        meta["entries"]
            .as_array_mut()
            .context("entries")?
            .push(json!({"id":"another-profile","name":"Later"}));
        meta["appliedId"] = json!("another-profile");
        std::fs::write(&meta_path, meta.to_string())?;
        assert!(!receipt.installed()?);
        receipt.restore(&journal)?;
        assert_eq!(files::json(&meta_path)?["appliedId"], "another-profile");
        assert_eq!(
            files::json(&root.join("claude_desktop_config.json"))?["deploymentMode"],
            "3p"
        );
        Ok(())
    }

    #[test]
    fn desktop_owned_edits_conflict_before_any_restore_write() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let receipt = files::Receipt::new(
            "http://localhost".into(),
            MODEL.into(),
            desktop(&root, "http://localhost", "key", MODEL, "BitRouter")?,
        );
        let journal = root.join("receipt.json");
        receipt.apply(&journal)?;
        let profile_path = root.join(format!("configLibrary/{PROFILE}.json"));
        let mut profile = files::json(&profile_path)?;
        profile["inferenceGatewayBaseUrl"] = json!("https://user.example");
        std::fs::write(&profile_path, profile.to_string())?;
        assert!(!receipt.installed()?);
        assert!(receipt.restore(&journal).is_err());
        assert!(journal.exists());
        assert_eq!(
            files::json(&root.join("configLibrary/_meta.json"))?["appliedId"],
            PROFILE
        );
        assert_eq!(
            files::json(&root.join("claude_desktop_config.json"))?["deploymentMode"],
            "3p"
        );
        Ok(())
    }
}
