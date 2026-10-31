//! Codex CLI and desktop share the user config and local ModelsResponse catalog.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Table, value};

use super::files::{self, Change};

const PROVIDER: &str = "bitrouter-connect";

pub(super) fn binary() -> Option<PathBuf> {
    crate::providers::cli_install::resolve_binary("codex").or_else(|| {
        [
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex",
            "/Applications/Codex.app/Contents/Resources/codex",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
    })
}

pub(super) async fn bundled_catalog() -> Option<Value> {
    // This documented debug command bypasses configuration/authentication and
    // returns the schema-matched catalog shipped with the installed binary.
    let binary = binary()?;
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(binary)
            .args(["debug", "models", "--bundled"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

pub(super) fn prepare(
    home: &Path,
    endpoint: &str,
    token: &str,
    model: &str,
    display_name: Option<&str>,
    catalog_override: Option<&Path>,
    bundled: Option<Value>,
) -> Result<Vec<Change>> {
    let config_path = home.join("config.toml");
    let before = files::read(&config_path)?;
    let mut config: DocumentMut = before
        .as_deref()
        .unwrap_or_default()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid Codex config.toml (contents redacted)"))?;
    // An active profile would silently win over the provider written below.
    if let Some(profile) = config.get("profile").and_then(Item::as_str) {
        let selected = config.get("profiles").and_then(|p| p.get(profile));
        ensure!(
            !selected.is_some_and(
                |p| p.get("model_provider").is_some() || p.get("model_catalog_json").is_some()
            ),
            "active Codex profile overrides its provider/catalog; remove those overrides before connecting"
        );
    }
    let existing_catalog = config
        .get("model_catalog_json")
        .and_then(Item::as_str)
        .map(|path| {
            let path = Path::new(path);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                home.join(path)
            }
        });
    let explicit_catalog = catalog_override.is_some() || existing_catalog.is_some();
    let source = catalog_override
        .map(Path::to_path_buf)
        .or(existing_catalog)
        .unwrap_or_else(|| home.join("models_cache.json"));
    let mut catalog = match files::read(&source)? {
        Some(text) => serde_json::from_str(&text).context("invalid Codex model catalog JSON")?,
        None if !explicit_catalog => bundled
            .context("Codex model catalog is missing; open Codex once or pass --codex-catalog")?,
        None => anyhow::bail!(
            "configured Codex catalog does not exist: {}",
            source.display()
        ),
    };
    let models = catalog.get_mut("models").and_then(Value::as_array_mut)
        .context("Codex model catalog is missing; open Codex once, or pass --codex-catalog with a complete ModelsResponse JSON file")?;
    ensure!(
        !models.is_empty(),
        "Codex catalog is empty; open Codex once to populate its model cache"
    );
    let mut slugs = std::collections::HashSet::new();
    for model in models.iter() {
        let slug = model
            .get("slug")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .context("Codex catalog requires ModelInfo entries, not an OpenAI /v1/models list")?;
        ensure!(
            slugs.insert(slug.to_string()),
            "Codex catalog contains duplicate slugs"
        );
        ensure!(
            model.get("shell_type").is_some() && model.get("supported_reasoning_levels").is_some(),
            "Codex catalog lacks required model metadata"
        );
    }
    if let Some(existing) = models.iter_mut().find(|entry| entry["slug"] == model) {
        // Reuse the model's supplied capabilities, including context and tools.
        existing["visibility"] = json!("list");
    } else {
        models.push(model_entry(model));
    }
    if let Some(label) = display_name
        && let Some(entry) = models.iter_mut().find(|entry| entry["slug"] == model)
    {
        entry["display_name"] = json!(label);
    }
    // API-authenticated custom providers filter models on this flag. Existing
    // slugs keep their metadata and route through the harness's gateway mapping.
    for model in models.iter_mut() {
        model["supported_in_api"] = json!(true);
    }
    let catalog_path = home.join("bitrouter-models.json");
    ensure!(
        !catalog_path.exists(),
        "unmanaged BitRouter catalog already exists; reconcile it before connecting"
    );
    let catalog = json!({ "models": models });
    config["model_provider"] = value(PROVIDER);
    config["model_catalog_json"] = value(catalog_path.to_string_lossy().as_ref());
    if config.get("model_providers").is_none() {
        config["model_providers"] = Item::Table(Table::new());
    }
    let providers = config["model_providers"]
        .as_table_like_mut()
        .context("Codex model_providers must be a table")?;
    ensure!(
        providers.get(PROVIDER).is_none(),
        "reserved BitRouter provider already exists without a receipt"
    );
    let mut provider = Table::new();
    provider["name"] = value("BitRouter");
    provider["base_url"] = value(format!("{endpoint}/v1"));
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(false);
    provider["supports_websockets"] = value(false);
    // Desktop launchers do not inherit shell exports. This private file holds
    // only the BitRouter credential, never a borrowed upstream credential.
    provider["experimental_bearer_token"] = value(token);
    let mut headers = Table::new();
    headers["x-bitrouter-harness"] = value("codex-acp");
    provider["http_headers"] = Item::Table(headers);
    providers.insert(PROVIDER, Item::Table(provider));
    Ok(vec![
        Change::json(catalog_path, None, &catalog)?,
        Change::snapshot(config_path, before, config.to_string()).with_restore(
            files::Restore::CodexConfig {
                model: model.into(),
            },
        ),
    ])
}

fn config_value(text: Option<&str>) -> Result<toml::Value> {
    toml::from_str(text.unwrap_or_default())
        .map_err(|_| anyhow::anyhow!("invalid Codex config.toml (contents redacted)"))
}

fn owned_provider(value: &toml::Value) -> Option<&toml::Value> {
    value.get("model_providers").and_then(|v| v.get(PROVIDER))
}

pub(super) fn config_installed(after: &str, current: Option<&str>) -> Result<bool> {
    let after = config_value(Some(after))?;
    let current = config_value(current)?;
    Ok(["model_provider", "model_catalog_json"]
        .iter()
        .all(|key| current.get(key) == after.get(key))
        && owned_provider(&current) == owned_provider(&after))
}

pub(super) fn restore_config(
    before_text: Option<&str>,
    after_text: &str,
    current_text: Option<&str>,
    model: &str,
) -> Result<Option<String>> {
    let before = config_value(before_text)?;
    let after = config_value(Some(after_text))?;
    let current = config_value(current_text)?;
    // Each owned field may already have been restored before an interruption.
    ensure!(
        ["model_provider", "model_catalog_json"]
            .iter()
            .all(|key| current.get(key) == after.get(key) || current.get(key) == before.get(key))
            && (owned_provider(&current) == owned_provider(&after)
                || owned_provider(&current) == owned_provider(&before)),
        "BitRouter provider or catalog settings were edited"
    );
    let parse = |text: &str| {
        text.parse::<DocumentMut>()
            .map_err(|_| anyhow::anyhow!("invalid Codex config.toml (contents redacted)"))
    };
    let original = parse(before_text.unwrap_or_default())?;
    let mut document = parse(current_text.context("Codex config was removed")?)?;
    for key in ["model_provider", "model_catalog_json"] {
        restore_item(&mut document, &original, key);
    }
    if let Some(providers) = document.get_mut("model_providers") {
        let providers = providers
            .as_table_like_mut()
            .context("invalid Codex providers")?;
        providers.remove(PROVIDER);
        if providers.is_empty() && before.get("model_providers").is_none() {
            document.remove("model_providers");
        }
    }
    // The picker writes model and effort after setup. Avoid leaving a default
    // that points at the catalog being removed; preserve later model choices.
    if current.get("model").and_then(toml::Value::as_str) == Some(model) {
        restore_item(&mut document, &original, "model");
        restore_item(&mut document, &original, "model_reasoning_effort");
    }
    if config_value(Some(&document.to_string()))? == before {
        Ok(before_text.map(str::to_owned))
    } else {
        Ok(Some(document.to_string()))
    }
}

fn restore_item(document: &mut DocumentMut, original: &DocumentMut, key: &str) {
    if let Some(value) = original.get(key) {
        document[key] = value.clone();
    } else {
        document.remove(key);
    }
}

fn model_entry(model: &str) -> Value {
    // Conservative Responses/tool contract. No built-in web/image/audio tools,
    // code-mode, multi-agent backend or unsupported service tier is advertised.
    json!({
        "slug": model,
        "display_name": model,
        "description": "Gateway model connected through BitRouter",
        "default_reasoning_level": null,
        "supported_reasoning_levels": [],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 100,
        "base_instructions": "You are a coding assistant. Follow the user's instructions, inspect relevant files, use the provided tools to complete the task, and verify changes. Explain results and any limitations accurately.",
        "support_verbosity": false,
        "supports_reasoning_summaries": false,
        "supports_reasoning_summary_parameter": false,
        "supports_parallel_tool_calls": false,
        "supports_search_tool": false,
        "prefer_websockets": false,
        "use_responses_lite": false,
        "tool_mode": "direct",
        "input_modalities": ["text"],
        "context_window": 64000,
        "max_context_window": 64000,
        "auto_compact_token_limit": 48000,
        "truncation_policy": {"mode": "tokens", "limit": 10000},
        "experimental_supported_tools": [],
        "additional_speed_tiers": [],
        "service_tiers": []
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const MODEL: &str = "team/coding-v2";

    #[test]
    fn picker_and_desktop_preferences_survive_reconnect_and_restore() -> Result<()> {
        for selected in [MODEL, "another-native"] {
            let temp = tempfile::tempdir()?;
            let root = temp.path().canonicalize()?;
            let config = root.join("config.toml");
            std::fs::write(
                &config,
                "# original\nmodel = 'native'\nmodel_reasoning_effort = 'high'\n",
            )?;
            let changes = prepare(
                &root,
                "http://localhost",
                "fixture",
                MODEL,
                None,
                None,
                Some(json!({"models": [model_entry("native")]})),
            )?;
            let receipt = files::Receipt::new("http://localhost".into(), MODEL.into(), changes);
            let journal = root.join("receipt.json");
            receipt.apply(&journal)?;
            let applied = std::fs::read_to_string(&config)?;
            let mut edited: DocumentMut = applied.parse()?;
            edited["model"] = value(selected);
            edited["model_reasoning_effort"] = value("none");
            edited["desktop"] = Item::Table(Table::new());
            edited["desktop"]["conversationDetailMode"] = value("STEPS_COMMANDS");
            std::fs::write(&config, edited.to_string())?;
            assert!(receipt.installed()?);
            if selected == MODEL {
                // Crash after restoring config, before deleting catalog/receipt.
                let restored = restore_config(
                    Some("# original\nmodel = 'native'\nmodel_reasoning_effort = 'high'\n"),
                    &applied,
                    Some(&edited.to_string()),
                    MODEL,
                )?
                .context("missing restored config")?;
                std::fs::write(&config, restored)?;
                assert!(!receipt.installed()?);
            }
            receipt.restore(&journal)?;
            let restored = std::fs::read_to_string(&config)?;
            let cfg = config_value(Some(&restored))?;
            assert!(restored.contains("# original"));
            assert_eq!(
                cfg["desktop"]["conversationDetailMode"].as_str(),
                Some("STEPS_COMMANDS")
            );
            assert_eq!(
                cfg["model"].as_str(),
                Some(if selected == MODEL {
                    "native"
                } else {
                    selected
                })
            );
            assert_eq!(
                cfg["model_reasoning_effort"].as_str(),
                Some(if selected == MODEL { "high" } else { "none" })
            );
            assert!(cfg.get("model_provider").is_none());
            assert!(cfg.get("model_catalog_json").is_none());
            assert!(cfg.get("model_providers").is_none());
            assert!(!root.join("bitrouter-models.json").exists());
        }
        Ok(())
    }

    #[test]
    fn owned_provider_edits_abort_all_restore_writes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let config = root.join("config.toml");
        let changes = prepare(
            &root,
            "http://localhost",
            "fixture",
            MODEL,
            None,
            None,
            Some(json!({"models": [model_entry("native")]})),
        )?;
        let receipt = files::Receipt::new("http://localhost".into(), MODEL.into(), changes);
        let journal = root.join("receipt.json");
        receipt.apply(&journal)?;
        let mut edited: DocumentMut = std::fs::read_to_string(&config)?.parse()?;
        edited["model_providers"][PROVIDER]["base_url"] = value("https://changed.example/v1");
        let text = edited.to_string();
        std::fs::write(&config, &text)?;
        assert!(!receipt.installed()?);
        assert!(receipt.restore(&journal).is_err());
        assert_eq!(std::fs::read_to_string(&config)?, text);
        assert!(root.join("bitrouter-models.json").exists());
        assert!(journal.exists());
        Ok(())
    }

    #[test]
    fn preserves_settings_native_metadata_and_restores_exactly() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let config = root.join("config.toml");
        let original =
            "# user comment\nmodel = \"native\"\n[projects.\"/repo\"]\ntrust_level = \"trusted\"\n";
        std::fs::write(&config, original)?;
        let mut native = model_entry("native");
        native["slug"] = json!("native");
        native["display_name"] = json!("Native");
        native["priority"] = json!(1);
        native["future_field"] = json!({"keep": true});
        std::fs::write(
            root.join("models_cache.json"),
            json!({"models": [native.clone()]}).to_string(),
        )?;
        let changes = prepare(
            &root,
            "http://127.0.0.1:4356",
            "gateway-key",
            MODEL,
            None,
            None,
            None,
        )?;
        let journal = root.join("receipt.json");
        let receipt = super::super::files::Receipt::new(
            "http://127.0.0.1:4356".into(),
            MODEL.into(),
            changes,
        );
        receipt.apply(&journal)?;
        let text = std::fs::read_to_string(&config)?;
        assert!(text.contains("# user comment"));
        let cfg: toml::Value = toml::from_str(&text)?;
        assert_eq!(cfg["model"].as_str(), Some("native"));
        assert_eq!(
            cfg["projects"]["/repo"]["trust_level"].as_str(),
            Some("trusted")
        );
        let catalog = files::json(&root.join("bitrouter-models.json"))?;
        assert_eq!(catalog["models"][0], native);
        assert_eq!(catalog["models"][1]["slug"], MODEL);
        receipt.restore(&journal)?;
        assert_eq!(std::fs::read_to_string(config)?, original);
        Ok(())
    }

    #[test]
    fn missing_catalog_and_active_profile_fail_without_writes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        assert!(prepare(&root, "http://localhost", "local", MODEL, None, None, None).is_err());
        assert!(!root.join("config.toml").exists());
        let mut native = model_entry("native");
        native["slug"] = json!("bundled-native");
        let seeded = prepare(
            &root,
            "http://localhost",
            "local",
            MODEL,
            None,
            None,
            Some(json!({"models": [native]})),
        )?;
        assert_eq!(seeded.len(), 2);
        assert!(!root.join("config.toml").exists());
        std::fs::write(
            root.join("config.toml"),
            "profile = 'work'\n[profiles.work]\nmodel_provider = 'company'\n",
        )?;
        assert!(prepare(&root, "http://localhost", "local", MODEL, None, None, None).is_err());
        Ok(())
    }

    #[test]
    fn an_existing_model_keeps_capabilities_without_duplicate_entries() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let mut existing = model_entry(MODEL);
        existing["context_window"] = json!(8192);
        existing["auto_compact_token_limit"] = json!(6000);
        existing["visibility"] = json!("hide");
        existing["future_metadata"] = json!({"keep": true});
        let changes = prepare(
            &root,
            "http://localhost/proxy",
            "local",
            MODEL,
            None,
            None,
            Some(json!({"models": [existing.clone()]})),
        )?;
        let receipt = super::super::files::Receipt::new(
            "http://localhost/proxy".into(),
            MODEL.into(),
            changes,
        );
        receipt.apply(&root.join("receipt.json"))?;
        let catalog = files::json(&root.join("bitrouter-models.json"))?;
        let models = catalog["models"].as_array().context("missing models")?;
        assert_eq!(models.len(), 1);
        existing["visibility"] = json!("list");
        assert_eq!(models[0], existing);
        Ok(())
    }
}
