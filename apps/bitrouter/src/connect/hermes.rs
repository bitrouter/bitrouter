//! Hermes' shared CLI/TUI/Desktop/dashboard provider catalog and scoped recovery.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};

use super::files::{self, Change, Restore};
use super::{Prepared, Protocol};

const PROVIDER: &str = "bitrouter";

pub(super) fn instance(explicit: Option<&Path>) -> Result<PathBuf> {
    let path = if let Some(path) = explicit {
        path.to_owned()
    } else if let Some(path) = std::env::var_os("HERMES_HOME").filter(|path| !path.is_empty()) {
        PathBuf::from(path)
    } else {
        #[cfg(target_os = "windows")]
        let root = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or(user_home()?.join("AppData/Local"))
            .join("hermes");
        #[cfg(not(target_os = "windows"))]
        let root = user_home()?.join(".hermes");
        match files::read(&root.join("active_profile"))? {
            Some(name) if !name.trim().is_empty() && name.trim() != "default" => {
                root.join("profiles").join(profile_name(name.trim())?)
            }
            _ => root,
        }
    };
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .context("cannot locate the Hermes user home; supply --instance")
}

fn profile_name(name: &str) -> Result<String> {
    let name = name.trim().to_lowercase();
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'_' | b'-'))
            && !matches!(name.as_str(), "hermes" | "test" | "tmp" | "root" | "sudo"),
        "invalid or reserved Hermes profile name"
    );
    Ok(name)
}

fn root(home: &Path) -> &Path {
    if home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "profiles")
    {
        home.parent().and_then(Path::parent).unwrap_or(home)
    } else {
        home
    }
}

fn profile_home(home: &Path, name: &str) -> PathBuf {
    if name == "default" {
        root(home).to_owned()
    } else {
        root(home).join("profiles").join(name)
    }
}

pub(super) fn profiles(home: &Path, requested: &[String]) -> Result<Vec<String>> {
    let mut selected = BTreeSet::new();
    for requested in requested {
        let name = profile_name(requested)?;
        ensure!(
            profile_home(home, &name).join("config.yaml").is_file(),
            "Hermes profile {name} is not initialized; create it in Hermes first"
        );
        selected.insert(name);
    }
    Ok(selected.into_iter().collect())
}

pub(super) fn config_paths(home: &Path, requested: &[String]) -> Result<Vec<PathBuf>> {
    let selected = profiles(home, requested)?;
    Ok(if selected.is_empty() {
        vec![home.join("config.yaml")]
    } else {
        selected
            .iter()
            .map(|name| profile_home(home, name).join("config.yaml"))
            .collect()
    })
}

pub(super) fn prepare(
    endpoint: &str,
    token: &str,
    model: &str,
    _display_name: &str,
    explicit: Option<&Path>,
    requested: &[String],
    protocol: Protocol,
) -> Result<Prepared> {
    ensure!(
        matches!(protocol, Protocol::ChatCompletions),
        "Hermes connector currently verifies chat-completions only"
    );
    // Hermes expands ${ENV} throughout config.yaml before resolving inline keys.
    ensure!(
        !endpoint.contains("${") && !model.contains("${"),
        "Hermes cannot persist literal endpoint/model values containing an environment expression"
    );
    ensure!(
        !token.contains("${") && token.trim() == token,
        "Hermes cannot persist a literal API key containing an environment expression or surrounding whitespace"
    );
    let home = instance(explicit)?;
    let mut changes = Vec::new();
    for path in config_paths(&home, requested)? {
        let before = files::read(&path)?;
        let value = object(before.as_deref())?;
        let providers = value.get("providers");
        ensure!(
            providers.is_none_or(Value::is_object),
            "Hermes config providers must be a mapping"
        );
        ensure!(
            providers
                .and_then(|providers| providers.get(PROVIDER))
                .is_none(),
            "Hermes already has a bitrouter provider; preserve or rename it before connecting"
        );
        // A legacy entry named BitRouter resolves to the same custom:bitrouter
        // identity and must not be silently shadowed by the new provider.
        ensure!(
            !value
                .get("custom_providers")
                .and_then(Value::as_array)
                .is_some_and(|entries| entries.iter().any(|entry| entry
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.trim().eq_ignore_ascii_case(PROVIDER)))),
            "Hermes already has a legacy BitRouter provider; preserve or rename it before connecting"
        );
        let provider = json!({
            "name": "BitRouter",
            "api": format!("{endpoint}/v1"),
            "api_key": token,
            "transport": "chat_completions",
            "discover_models": false,
            "default_model": model,
            "models": {model: {}},
            "extra_headers": {"x-bitrouter-harness": "hermes"}
        });
        let after = set_child(
            before.as_deref().unwrap_or(""),
            "providers",
            PROVIDER,
            Some(provider),
        )?;
        changes.push(
            Change::snapshot(path, before, after).with_restore(Restore::HermesConfig {
                model: model.into(),
            }),
        );
    }
    Ok(Prepared {
        files: changes,
        instance: home.display().to_string(),
        surfaces: vec!["cli".into(), "tui".into(), "desktop".into(), "dashboard".into()],
        notes: vec![
            "Hermes: reopen the model picker or restart the selected profile/backend; select BitRouter and the connected exact model ID. Existing main and auxiliary defaults are retained.".into(),
            "Hermes shares config.yaml across CLI, TUI, Desktop and Web Dashboard for the same backend/profile. Desktop remote connections use the remote backend's Hermes home; a local connection does not configure that remote machine.".into(),
            "Hermes' native model catalog uses model ID strings and has no separate per-model display label. BitRouter is the provider name; bitrouter/automatos remains the exact gateway model ID. --display-name cannot rename that native row without changing the wire model.".into(),
            "Only Chat Completions is enabled. No context, pricing, vision, reasoning or other gateway capability is asserted; Hermes may apply its own metadata defaults.".into(),
            "API keys are stored privately in the selected config.yaml, independent of the launching shell. Removal restores the prior main route only while this BitRouter model is still selected and preserves later unrelated settings.".into(),
            "Hermes Desktop keeps manual composer pins and historical session models outside config.yaml. After removal, reopen its picker and choose another model if a stale Automatos pin remains; the removed BitRouter provider disappears from the catalog.".into(),
        ],
    })
}

fn object(text: Option<&str>) -> Result<Map<String, Value>> {
    match text.filter(|text| !text.trim().is_empty()) {
        Some(text) => serde_saphyr::from_str(text)
            .map_err(|_| anyhow::anyhow!("expected a Hermes YAML mapping")),
        None => Ok(Map::new()),
    }
}

// Locate mapping keys with the YAML parser, then change only the owned rows.
// This preserves comments, auxiliary settings and unrelated provider text.
struct Entries(Vec<(serde_saphyr::Spanned<String>, Value)>);
impl<'de> Deserialize<'de> for Entries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Mapping;
        impl<'de> Visitor<'de> for Mapping {
            type Value = Entries;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a YAML mapping")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Entries, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Entries(entries))
            }
        }
        deserializer.deserialize_map(Mapping)
    }
}

fn rows(text: &str) -> Result<Vec<(String, usize, usize, usize)>> {
    if text.trim().is_empty() || text.trim() == "{}" {
        return Ok(Vec::new());
    }
    let entries: Entries = serde_saphyr::from_str(text)
        .map_err(|_| anyhow::anyhow!("expected a Hermes YAML mapping"))?;
    let mut located = Vec::new();
    for (key, _) in entries.0 {
        let line = usize::try_from(key.referenced.line())?;
        let column = usize::try_from(key.referenced.column())?;
        ensure!(
            line > 0 && column > 0 && key.referenced == key.defined,
            "Hermes connector cannot edit aliased or merged mapping keys"
        );
        let start: usize = text
            .split_inclusive('\n')
            .take(line - 1)
            .map(str::len)
            .sum();
        let row = text[start..].lines().next().unwrap_or("");
        ensure!(
            row.chars().take(column - 1).all(char::is_whitespace)
                && !row.trim_start().starts_with(['{', '[']),
            "convert the Hermes mapping to block YAML before connecting"
        );
        located.push((key.value, start, text.len(), column - 1));
    }
    for index in 0..located.len().saturating_sub(1) {
        located[index].2 = located[index + 1].1;
    }
    Ok(located)
}

fn content_end(text: &str, start: usize, end: usize) -> usize {
    let mut end = end;
    while end > start {
        let fragment = &text[start..end];
        let without_newline = fragment.strip_suffix('\n').unwrap_or(fragment);
        let row = without_newline.rsplit('\n').next().unwrap_or("");
        if !row.trim().is_empty() && !row.trim_start().starts_with('#') {
            break;
        }
        end -= row.len() + usize::from(fragment.ends_with('\n'));
    }
    end
}

fn render(key: &str, value: Value, indent: usize) -> Result<String> {
    let text = serde_saphyr::to_string(&json!({key: value}))?;
    Ok(text
        .lines()
        .map(|line| format!("{}{line}\n", " ".repeat(indent)))
        .collect())
}

fn set_entry(text: &str, key: &str, value: Option<Value>, default_indent: usize) -> Result<String> {
    let entries = rows(text)?;
    let mut result = text.to_owned();
    if let Some((_, start, end, indent)) = entries.iter().find(|(name, ..)| name == key) {
        let end = content_end(text, *start, *end);
        result.replace_range(
            *start..end,
            &match value {
                Some(value) => render(key, value, *indent)?,
                None => String::new(),
            },
        );
    } else if let Some(value) = value {
        ensure!(
            !text.lines().any(|line| line.trim() == "..."),
            "remove the explicit Hermes YAML document terminator before connecting"
        );
        if text.trim() == "{}" {
            result.clear();
        }
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&render(
            key,
            value,
            entries.first().map_or(default_indent, |entry| entry.3),
        )?);
    }
    object(Some(&result))?;
    Ok(result)
}

fn set_child(text: &str, parent: &str, key: &str, value: Option<Value>) -> Result<String> {
    let parsed = object(Some(text))?;
    let parent_value = parsed.get(parent);
    ensure!(
        parent_value.is_none_or(Value::is_object),
        "Hermes {parent} must be a mapping"
    );
    if parent_value
        .and_then(Value::as_object)
        .is_none_or(Map::is_empty)
    {
        let mut mapping = Map::new();
        if let Some(value) = value {
            mapping.insert(key.into(), value);
        }
        return set_entry(text, parent, Some(Value::Object(mapping)), 0);
    }
    let entries = rows(text)?;
    let (_, start, end, _) = entries
        .iter()
        .find(|(name, ..)| name == parent)
        .context("cannot locate Hermes mapping")?;
    let end = content_end(text, *start, *end);
    let fragment = &text[*start..end];
    let colon = fragment
        .find(':')
        .context("cannot locate Hermes mapping separator")?;
    let child_text = &fragment[colon + 1..];
    let child = set_entry(child_text, key, value, 2)?;
    if object(Some(&child))?.is_empty() {
        return set_entry(text, parent, Some(json!({})), 0);
    }
    let mut result = text.to_owned();
    result.replace_range(*start + colon + 1..end, &child);
    object(Some(&result))?;
    Ok(result)
}

pub(super) fn restore_config(
    before: Option<&str>,
    after: &str,
    current: Option<&str>,
    model: &str,
) -> Result<Option<String>> {
    let expected = object(Some(after))?;
    let prior = object(before)?;
    let value = object(current)?;
    let owned = expected
        .get("providers")
        .and_then(|providers| providers.get(PROVIDER))
        .context("missing saved Hermes provider")?;
    ensure!(
        value
            .get("providers")
            .and_then(|providers| providers.get(PROVIDER))
            .is_none_or(|provider| provider == owned),
        "BitRouter Hermes provider was edited"
    );
    let mut text = current.unwrap_or("").to_owned();
    if value
        .get("providers")
        .and_then(|providers| providers.get(PROVIDER))
        .is_some()
    {
        text = set_child(&text, "providers", PROVIDER, None)?;
        if object(Some(&text))?
            .get("providers")
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
            && !prior.contains_key("providers")
        {
            text = set_entry(&text, "providers", None, 0)?;
        }
    }
    let selected = value.get("model");
    let selected_provider = selected
        .and_then(|value| value.get("provider"))
        .and_then(Value::as_str);
    if matches!(selected_provider, Some("bitrouter" | "custom:bitrouter"))
        && selected
            .and_then(|value| value.get("default"))
            .and_then(Value::as_str)
            == Some(model)
    {
        let previous = prior.get("model");
        for key in [
            "provider", "default", "base_url", "api_mode", "api_key", "key_env",
        ] {
            text = set_child(
                &text,
                "model",
                key,
                previous.and_then(|value| value.get(key)).cloned(),
            )?;
        }
        let remaining = object(Some(&text))?.get("model").cloned();
        if remaining
            .as_ref()
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty)
        {
            text = set_entry(&text, "model", previous.cloned(), 0)?;
        }
    }
    if object(Some(&text))? == prior {
        Ok(before.map(str::to_owned))
    } else {
        Ok(Some(text))
    }
}

pub(super) fn config_installed(after: &str, current: Option<&str>) -> Result<bool> {
    let expected = object(Some(after))?;
    let value = object(current)?;
    Ok(value
        .get("providers")
        .and_then(|providers| providers.get(PROVIDER))
        .is_some_and(|provider| {
            expected
                .get("providers")
                .and_then(|providers| providers.get(PROVIDER))
                == Some(provider)
        }))
}

pub(super) fn stored_token(after: &str) -> Result<String> {
    let value = object(Some(after))?;
    let token = value
        .get("providers")
        .and_then(|providers| providers.get(PROVIDER))
        .and_then(|provider| provider.get("api_key"))
        .and_then(Value::as_str)
        .context("missing saved Hermes API key")?;
    ensure!(
        !token.is_empty() && !token.contains("${"),
        "Hermes credential expressions cannot be checked automatically"
    );
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_provider_preserves_yaml_and_exact_model() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let original = "# keep header\nmodel:\n  default: old\n  provider: native\nproviders:\n  native: # keep provider comment\n    api: https://native.invalid/v1\nauxiliary:\n  compression:\n    provider: native\n    model: small\n# keep footer\n";
        std::fs::write(directory.path().join("config.yaml"), original)?;
        let prepared = prepare(
            "http://127.0.0.1:4356",
            "fixture-key",
            "bitrouter/automatos",
            "automatos",
            Some(directory.path()),
            &[],
            Protocol::ChatCompletions,
        )?;
        let file = prepared.files.first().context("missing provider change")?;
        // The private receipt owns the complete written text. Inspect through
        // serde rather than exposing file internals just for this test.
        let provider = json!({"api":"http://127.0.0.1:4356/v1", "api_key":"fixture-key", "models":{"bitrouter/automatos":{}}});
        let after = set_child(original, "providers", PROVIDER, Some(provider))?;
        assert!(
            after.contains("native: # keep provider comment\n    api: https://native.invalid/v1\n")
        );
        assert!(after.contains(
            "auxiliary:\n  compression:\n    provider: native\n    model: small\n# keep footer\n"
        ));
        assert_eq!(file.path, directory.path().join("config.yaml"));
        assert!(config_installed(&after, Some(&after))?);
        assert_eq!(stored_token(&after)?, "fixture-key");
        assert_eq!(
            restore_config(Some(original), &after, Some(&after), "bitrouter/automatos")?,
            Some(original.into())
        );
        Ok(())
    }

    #[test]
    fn fresh_profile_removal_recovers_missing_mapping() -> Result<()> {
        let before = "model: \"\"\n";
        let after = set_child(
            before,
            "providers",
            PROVIDER,
            Some(json!({"api_key":"fixture-key"})),
        )?;
        assert_eq!(
            restore_config(Some(before), &after, Some(&after), "bitrouter/automatos")?,
            Some(before.into())
        );
        assert_eq!(
            restore_config(
                None,
                &set_child(
                    "",
                    "providers",
                    PROVIDER,
                    Some(json!({"api_key":"fixture-key"}))
                )?,
                Some(&set_child(
                    "",
                    "providers",
                    PROVIDER,
                    Some(json!({"api_key":"fixture-key"}))
                )?),
                "bitrouter/automatos"
            )?,
            None
        );
        Ok(())
    }

    #[test]
    fn removal_recovers_owned_selection_and_keeps_later_native_edits() -> Result<()> {
        let before = "model:\n  provider: native\n  default: first\n  base_url: https://old.invalid/v1\n  aliases: {fast: native/fast}\nauxiliary: {vision: {model: keep}}\n";
        let after = set_child(
            before,
            "providers",
            PROVIDER,
            Some(json!({"api_key":"fixture-key"})),
        )?;
        let selected = set_child(&after, "model", "provider", Some(json!("bitrouter")))?;
        let selected = set_child(
            &selected,
            "model",
            "default",
            Some(json!("bitrouter/automatos")),
        )?;
        let selected = set_child(
            &selected,
            "model",
            "base_url",
            Some(json!("http://127.0.0.1:4356/v1")),
        )?;
        let selected = set_child(&selected, "model", "reasoning_effort", Some(json!("high")))?;
        let selected = set_child(
            &selected,
            "providers",
            "later",
            Some(json!({"api":"https://later.invalid/v1"})),
        )?;
        let restored =
            restore_config(Some(before), &after, Some(&selected), "bitrouter/automatos")?
                .context("missing restored config")?;
        let value = object(Some(&restored))?;
        assert_eq!(value["model"]["provider"], "native");
        assert_eq!(value["model"]["default"], "first");
        assert_eq!(value["model"]["reasoning_effort"], "high");
        assert_eq!(value["model"]["aliases"]["fast"], "native/fast");
        assert!(value["providers"].get(PROVIDER).is_none());
        assert!(value["providers"].get("later").is_some());
        assert_eq!(value["auxiliary"]["vision"]["model"], "keep");
        Ok(())
    }

    #[test]
    fn removal_fails_closed_on_provider_edits_and_retains_later_selection() -> Result<()> {
        let before = "model: {provider: native, default: first}\n";
        let after = set_child(
            before,
            "providers",
            PROVIDER,
            Some(json!({"api_key":"fixture-key"})),
        )?;
        let modified = set_child(
            &after,
            "providers",
            PROVIDER,
            Some(json!({"api_key":"edited"})),
        )?;
        assert!(
            restore_config(Some(before), &after, Some(&modified), "bitrouter/automatos").is_err()
        );
        assert!(!config_installed(&after, Some(&modified))?);
        let later = set_entry(
            &after,
            "model",
            Some(json!({"provider":"native","default":"later"})),
            0,
        )?;
        let restored = restore_config(Some(before), &after, Some(&later), "bitrouter/automatos")?
            .context("missing restored config")?;
        assert_eq!(object(Some(&restored))?["model"]["default"], "later");
        Ok(())
    }

    #[test]
    fn explicit_profiles_are_bounded_and_protocols_are_not_guessed() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let named = directory.path().join("profiles/work");
        std::fs::create_dir_all(&named)?;
        std::fs::write(named.join("config.yaml"), "model: ''\n")?;
        assert!(profiles(directory.path(), &[])?.is_empty());
        assert_eq!(
            profiles(directory.path(), &[" Work ".into(), "work".into()])?,
            vec!["work"]
        );
        assert_eq!(profile_home(&named, "default"), directory.path());
        assert!(profiles(directory.path(), &["../escape".into()]).is_err());
        assert!(
            prepare(
                "http://127.0.0.1:4356",
                "key",
                "m",
                "m",
                Some(directory.path()),
                &[],
                Protocol::Responses
            )
            .is_err()
        );
        for (endpoint, model) in [
            ("http://127.0.0.1:4356/${BASE}", "model"),
            ("http://127.0.0.1:4356", "${MODEL}"),
        ] {
            assert!(
                prepare(
                    endpoint,
                    "key",
                    model,
                    "automatos",
                    Some(directory.path()),
                    &[],
                    Protocol::ChatCompletions
                )
                .is_err()
            );
        }
        assert!(
            prepare(
                "http://127.0.0.1:4356",
                "${TOKEN}",
                "m",
                "m",
                Some(directory.path()),
                &[],
                Protocol::ChatCompletions
            )
            .is_err()
        );
        Ok(())
    }
}
