//! Persistent OpenCode provider configuration for a local server instance.
#[path = "opencode/jsonc.rs"]
mod jsonc;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};

use super::{Prepared, Protocol, files};

const PROVIDER: &str = "bitrouter";
const SELECTOR: &str = "bitrouter/automatos";

fn environment_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home() -> Result<PathBuf> {
    environment_path("HOME")
        .or_else(|| environment_path("USERPROFILE"))
        .context("cannot locate the OpenCode user home")
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    super::absolute(path)
}

fn directory_config(directory: &Path) -> PathBuf {
    let jsonc = directory.join("opencode.jsonc");
    if jsonc.is_file() {
        jsonc
    } else {
        directory.join("opencode.json")
    }
}

/// The actual file is also the receipt identity. Never changes the working project.
pub(super) fn instance(explicit: Option<&Path>) -> Result<PathBuf> {
    let path = match explicit {
        Some(path) => {
            let file_extension = path.extension().and_then(|extension| extension.to_str());
            if path.is_file() || matches!(file_extension, Some("json" | "jsonc")) {
                path.to_owned()
            } else {
                directory_config(path)
            }
        }
        None => {
            if let Some(directory) = environment_path("OPENCODE_CONFIG_DIR") {
                directory_config(&directory)
            } else if let Some(path) = environment_path("OPENCODE_CONFIG") {
                path
            } else {
                let directory = environment_path("XDG_CONFIG_HOME")
                    .unwrap_or(home()?.join(".config"))
                    .join("opencode");
                directory_config(&directory)
            }
        }
    };
    absolute(path)
}

fn permitted(value: &Value) -> Result<()> {
    if let Some(disabled) = value.get("disabled_providers") {
        let disabled = disabled
            .as_array()
            .context("OpenCode disabled_providers must be an array")?;
        ensure!(
            !disabled.iter().any(|item| item.as_str() == Some(PROVIDER)),
            "OpenCode explicitly disables BitRouter; update disabled_providers first"
        );
    }
    if let Some(enabled) = value.get("enabled_providers") {
        let enabled = enabled
            .as_array()
            .context("OpenCode enabled_providers must be an array")?;
        ensure!(
            enabled.iter().any(|item| item.as_str() == Some(PROVIDER)),
            "OpenCode enabled_providers excludes BitRouter; update that allowlist first"
        );
    }
    Ok(())
}

fn conflicting_layers(target: &Path, explicit: bool) -> Result<()> {
    // Refuse known provider collisions instead of silently overriding another
    // configuration source. Project configuration itself is never changed.
    let mut paths = Vec::new();
    let global = environment_path("XDG_CONFIG_HOME")
        .unwrap_or(home()?.join(".config"))
        .join("opencode");
    for directory in [Some(global), environment_path("OPENCODE_CONFIG_DIR")]
        .into_iter()
        .flatten()
    {
        for name in ["config.json", "opencode.json", "opencode.jsonc"] {
            paths.push(absolute(directory.join(name))?);
        }
    }
    if let Some(path) = environment_path("OPENCODE_CONFIG") {
        paths.push(absolute(path)?);
    }
    if !explicit {
        let cwd = std::env::current_dir()?;
        for directory in cwd.ancestors() {
            for name in [
                "opencode.json",
                "opencode.jsonc",
                ".opencode/opencode.json",
                ".opencode/opencode.jsonc",
            ] {
                paths.push(directory.join(name));
            }
        }
    }
    paths.sort();
    paths.dedup();
    for path in paths {
        if path == target {
            continue;
        }
        if let Some(text) = files::read(&path)? {
            let document = jsonc::parse(&text)
                .with_context(|| format!("invalid OpenCode configuration: {}", path.display()))?;
            permitted(&document.value).with_context(|| {
                format!("conflicting OpenCode configuration: {}", path.display())
            })?;
            ensure!(
                document.value.pointer("/provider/bitrouter").is_none(),
                "another OpenCode configuration already defines BitRouter: {}",
                path.display()
            );
        }
    }
    if let Ok(text) = std::env::var("OPENCODE_CONFIG_CONTENT")
        && !text.trim().is_empty()
    {
        let document = jsonc::parse(&text).context("invalid OPENCODE_CONFIG_CONTENT")?;
        permitted(&document.value)?;
        ensure!(
            document.value.pointer("/provider/bitrouter").is_none(),
            "OPENCODE_CONFIG_CONTENT already defines BitRouter; remove that runtime override first"
        );
    }
    Ok(())
}

fn literal_inputs(endpoint: &str, token: &str, model: &str, display_name: &str) -> Result<()> {
    for (name, value) in [
        ("endpoint", endpoint),
        ("credential", token),
        ("model", model),
        ("display name", display_name),
    ] {
        ensure!(
            !value.contains("{env:") && !value.contains("{file:"),
            "OpenCode {name} contains variable-substitution syntax; supply a literal value without OpenCode template markers"
        );
    }
    Ok(())
}

pub(super) fn prepare(
    endpoint: &str,
    token: &str,
    models: &[String],
    display_name: &str,
    explicit: Option<&Path>,
    protocol: Protocol,
) -> Result<Prepared> {
    let model = models.first().context("missing OpenCode model")?;
    let mut entries = Map::new();
    for (index, id) in models.iter().enumerate() {
        literal_inputs(endpoint, token, id, display_name)?;
        ensure!(
            index == 0 || id != "automatos",
            "additional OpenCode model automatos conflicts with the primary native selection key"
        );
        let key = if index == 0 { "automatos" } else { id };
        entries.insert(
            key.into(),
            json!({"id":id,"name":if index == 0 {display_name} else {id}}),
        );
    }
    let path = instance(explicit)?;
    conflicting_layers(&path, explicit.is_some())?;
    let before = files::read(&path)?;
    let text = before.as_deref().unwrap_or("{}\n");
    let document = jsonc::parse(text)
        .with_context(|| format!("invalid OpenCode configuration: {}", path.display()))?;
    permitted(&document.value)?;
    ensure!(
        document.value.pointer("/provider/bitrouter").is_none(),
        "OpenCode already has a BitRouter provider; choose another instance or remove that provider explicitly"
    );
    let npm = match protocol {
        Protocol::ChatCompletions => "@ai-sdk/openai-compatible",
        Protocol::Responses => "@ai-sdk/openai",
        Protocol::Messages => "@ai-sdk/anthropic",
    };
    let provider = json!({
        "npm": npm,
        "name": "BitRouter",
        "options": {
            "baseURL": format!("{}/v1", endpoint.trim_end_matches('/')),
            "apiKey": token
        },
        "models": entries
    });
    let after = if let Some(member) = document.member("provider") {
        jsonc::insert(text, &member.node, PROVIDER, &provider)?
    } else {
        jsonc::insert(text, &document, "provider", &json!({ PROVIDER: provider }))?
    };
    // Catch a patching error before the transaction can write any bytes.
    jsonc::parse(&after)?;
    let file = files::Change::snapshot(path.clone(), before, after)
        .with_restore(files::Restore::OpenCodeProvider);
    Ok(Prepared {
        files: vec![file],
        instance: path.to_string_lossy().into_owned(),
        surfaces: vec![
            "opencode_cli_tui".into(),
            "opencode_desktop_local_server".into(),
            "opencode_web_local_server".into(),
        ],
        notes: vec![
            format!("Select {SELECTOR} in /models; the gateway request model is {model}. Existing providers and defaults are preserved."),
            "Restart OpenCode CLI/TUI or its local Desktop/Web server to load this configuration. A Desktop/Web connection to a remote server must be configured on that server; local edits do not configure it.".into(),
            "The gateway credential is stored in the private provider configuration and connection receipt. No global OpenCode authentication entries are changed.".into(),
            "Custom configurations apply only to OpenCode processes that load that file/directory. Higher-priority project, inline, or managed configuration can override local settings.".into(),
        ],
    })
}

pub(super) fn stored_token(after: &str) -> Result<String> {
    let document = jsonc::parse(after)?;
    document
        .value
        .pointer("/provider/bitrouter/options/apiKey")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("connection receipt has no OpenCode provider credential")
}

pub(super) fn config_installed(after: &str, current: Option<&str>) -> Result<bool> {
    let Some(current) = current else {
        return Ok(false);
    };
    let expected = jsonc::parse(after)?;
    let actual = jsonc::parse(current)?;
    Ok(permitted(&actual.value).is_ok()
        && actual.value.pointer("/provider/bitrouter")
            == expected.value.pointer("/provider/bitrouter"))
}

/// Remove only the owned provider and a default still pointing to its selector.
/// All other JSONC bytes, including comments and later user choices, survive.
pub(super) fn restore_config(
    before: Option<&str>,
    after: &str,
    current: Option<&str>,
) -> Result<Option<String>> {
    let Some(current) = current else {
        return Ok(None);
    };
    let original = jsonc::parse(before.unwrap_or("{}"))?;
    let expected = jsonc::parse(after)?;
    let document = jsonc::parse(current)?;
    let mut restored = current.to_owned();
    if let Some(provider) = document.member("provider") {
        ensure!(
            provider.node.value.is_object(),
            "OpenCode provider settings were replaced"
        );
        if let Some(owned) = provider.node.member(PROVIDER) {
            ensure!(
                Some(&owned.node.value) == expected.value.pointer("/provider/bitrouter"),
                "BitRouter provider was edited; receipt retained"
            );
            restored = if original.member("provider").is_none() && provider.node.members.len() == 1
            {
                jsonc::remove(&restored, &document, "provider")?
            } else {
                jsonc::remove(&restored, &provider.node, PROVIDER)?
            };
        }
    }
    for key in ["model", "small_model"] {
        let document = jsonc::parse(&restored)?;
        if let Some(member) = document.member(key)
            && member
                .node
                .value
                .as_str()
                .and_then(|selected| selected.strip_prefix("bitrouter/"))
                .is_some_and(|key| {
                    expected
                        .value
                        .pointer("/provider/bitrouter/models")
                        .and_then(Value::as_object)
                        .is_some_and(|models| models.contains_key(key))
                })
        {
            if let Some(value) = original.value.get(key) {
                restored.replace_range(member.node.span.clone(), &serde_json::to_string(value)?);
            } else {
                restored = jsonc::remove(&restored, &document, key)?;
            }
        }
    }
    jsonc::parse(&restored)?;
    Ok(Some(restored))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn added(before: &str) -> Result<String> {
        let document = jsonc::parse(before)?;
        let provider = json!({"models":{"automatos":{"id":"wire/model", "name":"Display"}}});
        if let Some(member) = document.member("provider") {
            jsonc::insert(before, &member.node, PROVIDER, &provider)
        } else {
            jsonc::insert(before, &document, "provider", &json!({PROVIDER:provider}))
        }
    }

    #[test]
    fn inserted_values_cannot_trigger_native_variable_substitution() -> Result<()> {
        let ordinary = [
            "http://127.0.0.1:4356",
            "fixture-key",
            "team/coder",
            "BitRouter model",
        ];
        literal_inputs(ordinary[0], ordinary[1], ordinary[2], ordinary[3])?;
        for index in 0..ordinary.len() {
            for marker in ["{env:SECRET}", "{file:/private/key}"] {
                let mut values = ordinary;
                values[index] = marker;
                assert!(literal_inputs(values[0], values[1], values[2], values[3]).is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn preserves_jsonc_and_later_edits_and_cleans_owned_default() -> Result<()> {
        let before = "{\n // keep root\n \"model\": \"native/model\",\n \"provider\": { /* keep provider */ \"native\": {\"models\":{}}, },\n}\n";
        let after = added(before)?;
        let current = after
            .replace("native/model", SELECTOR)
            .replace("// keep root", "// later comment");
        let document = jsonc::parse(&current)?;
        let current = jsonc::insert(&current, &document, "theme", &json!("system"))?;
        assert!(config_installed(&after, Some(&current))?);
        let restored =
            restore_config(Some(before), &after, Some(&current))?.context("missing config")?;
        let value = jsonc::parse(&restored)?.value;
        assert_eq!(value["model"], "native/model");
        assert_eq!(value["theme"], "system");
        assert!(value.pointer("/provider/native").is_some());
        assert!(value.pointer("/provider/bitrouter").is_none());
        assert!(restored.contains("// later comment"));
        assert!(restored.contains("/* keep provider */"));
        Ok(())
    }

    #[test]
    fn preserves_later_default_and_new_provider() -> Result<()> {
        let before = "{\"model\":\"original/model\"}";
        let after = added(before)?;
        let current = after.replace("original/model", "later/model");
        let document = jsonc::parse(&current)?;
        let provider = document.member("provider").context("missing provider")?;
        let current = jsonc::insert(&current, &provider.node, "new", &json!({"models":{}}))?;
        let restored =
            restore_config(Some(before), &after, Some(&current))?.context("missing config")?;
        let value = jsonc::parse(&restored)?.value;
        assert_eq!(value["model"], "later/model");
        assert!(value.pointer("/provider/new").is_some());
        assert!(value.pointer("/provider/bitrouter").is_none());
        Ok(())
    }

    #[test]
    fn rejects_changed_owned_provider_and_ambiguous_jsonc() -> Result<()> {
        let after = added("{}")?;
        let edited = after.replace("wire/model", "different/model");
        assert!(restore_config(None, &after, Some(&edited)).is_err());
        assert!(!config_installed(&after, Some(&edited))?);
        assert!(jsonc::parse("{\"provider\":{},\"provider\":{}}").is_err());
        assert!(jsonc::parse("{\"a\":1 /* truncated").is_err());
        assert!(jsonc::parse("{\"a\": [true, false,], /* valid */}").is_ok());
        Ok(())
    }

    #[test]
    fn removes_owned_default_without_replacing_new_comments() -> Result<()> {
        let after = added("{}")?;
        let document = jsonc::parse(&after)?;
        let current = jsonc::insert(&after, &document, "small_model", &json!(SELECTOR))?
            .replace('{', "{ /* user comment */");
        let restored = restore_config(None, &after, Some(&current))?.context("missing config")?;
        assert_eq!(jsonc::parse(&restored)?.value, json!({}));
        assert!(restored.contains("/* user comment */"));
        Ok(())
    }
}
