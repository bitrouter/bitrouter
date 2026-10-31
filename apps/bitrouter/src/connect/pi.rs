//! Pi CLI/TUI's persistent compatible provider, credentials, and scoped recovery.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};

use super::files::{self, Change, Restore};
use super::{Prepared, Protocol};

const PROVIDER: &str = "bitrouter";

pub(super) fn prepare(
    endpoint: &str,
    token: &str,
    model: &str,
    display_name: &str,
    instance: Option<&Path>,
    protocol: Protocol,
) -> Result<Prepared> {
    let home = self::instance(instance)?;
    let (mut models, models_before) = files::json_snapshot(&home.join("models.json"))?;
    let providers = models
        .as_object_mut()
        .context("Pi models.json must be an object")?
        .entry("providers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Pi models.json providers must be an object")?;
    ensure!(
        !providers.contains_key(PROVIDER),
        "Pi already has a bitrouter provider; preserve or rename it before connecting"
    );
    let (api, base_url) = match protocol {
        Protocol::ChatCompletions => ("openai-completions", format!("{endpoint}/v1")),
        Protocol::Responses => ("openai-responses", format!("{endpoint}/v1")),
        Protocol::Messages => ("anthropic-messages", endpoint.to_owned()),
    };
    providers.insert(
        PROVIDER.into(),
        json!({
            "name": "BitRouter",
            "baseUrl": base_url,
            "api": api,
            "authHeader": true,
            "headers": {"x-bitrouter-harness": "pi-acp"},
            "models": [{"id": model, "name": display_name}]
        }),
    );

    let (mut auth, auth_before) = files::json_snapshot(&home.join("auth.json"))?;
    let credentials = auth
        .as_object_mut()
        .context("Pi auth.json must be an object")?;
    ensure!(
        !credentials.contains_key(PROVIDER),
        "Pi already has bitrouter credentials; preserve or remove them before connecting"
    );
    // Pi resolves $ templates and leading ! commands even in auth.json. Escaping
    // prevents a gateway token from becoming an environment lookup or command.
    let key = token.replace('$', "$$");
    let key = if key.starts_with('!') {
        format!("${key}")
    } else {
        key
    };
    credentials.insert(PROVIDER.into(), json!({"type": "api_key", "key": key}));

    let (settings, settings_before) = files::json_snapshot(&home.join("settings.json"))?;
    // Track the startup selection without changing it. An empty file is created
    // for a fresh profile so later /model Ctrl+S selections can also be unwound.
    let settings_after = match &settings_before {
        Some(text) => text.clone(),
        None => format!("{}\n", serde_json::to_string_pretty(&settings)?),
    };
    Ok(Prepared {
        files: vec![
            Change::json(home.join("models.json"), models_before, &models)?
                .with_restore(Restore::PiModels),
            Change::json(home.join("auth.json"), auth_before, &auth)?
                .with_restore(Restore::PiAuth),
            Change::snapshot(home.join("settings.json"), settings_before, settings_after)
                .with_restore(Restore::PiSettings { model: model.into() }),
        ],
        instance: home.display().to_string(),
        surfaces: vec!["cli".into(), "tui".into()],
        notes: vec![
            "Pi CLI/TUI: reopen /model (or restart Pi) to reload models.json; select the BitRouter provider and the connected model. Existing providers and startup defaults are retained.".into(),
            "Pi authentication is persisted in auth.json, so it does not depend on the launching shell. A runtime --api-key or provider extension can override it.".into(),
            "No context limit, price, image or reasoning capability is asserted. Pi supplies its own defaults for omitted metadata; these are not verified gateway model limits.".into(),
            "settings.json snapshots the startup selection. Removal restores its previous default only while the connected BitRouter model is still selected; later native selections and other preferences are preserved.".into(),
        ],
    })
}

pub(super) fn instance(instance: Option<&Path>) -> Result<PathBuf> {
    let path = match instance {
        Some(path) => path.to_owned(),
        None => match std::env::var_os("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => user_home()?.join(".pi/agent"),
        },
    };
    // This matches Pi's getAgentDir(), including ~/ in PI_CODING_AGENT_DIR.
    let path = match path.to_str() {
        Some("~") => user_home()?,
        Some(text) if text.starts_with("~/") || text.starts_with("~\\") => {
            user_home()?.join(&text[2..])
        }
        _ => path,
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
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("cannot locate the Pi user home; supply --instance")
}

fn object(text: Option<&str>) -> Result<Map<String, Value>> {
    match text {
        Some(text) => serde_json::from_str(text).context("expected a Pi JSON object"),
        None => Ok(Map::new()),
    }
}

fn encoded(value: Map<String, Value>, before: Option<&str>) -> Result<Option<String>> {
    if value == object(before)? {
        Ok(before.map(str::to_owned))
    } else {
        Ok(Some(format!("{}\n", serde_json::to_string_pretty(&value)?)))
    }
}

pub(super) fn restore_models(
    before: Option<&str>,
    after: &str,
    current: Option<&str>,
) -> Result<Option<String>> {
    let mut value = object(current)?;
    let after = object(Some(after))?;
    let before_value = object(before)?;
    let expected = after
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(PROVIDER))
        .context("missing owned Pi provider")?;
    if let Some(providers) = value.get_mut("providers") {
        let providers = providers.as_object_mut().context("invalid Pi providers")?;
        ensure!(
            providers
                .get(PROVIDER)
                .is_none_or(|provider| provider == expected),
            "BitRouter Pi provider was edited"
        );
        providers.remove(PROVIDER);
        if providers.is_empty() && !before_value.contains_key("providers") {
            value.remove("providers");
        }
    }
    encoded(value, before)
}

pub(super) fn models_installed(after: &str, current: Option<&str>) -> Result<bool> {
    let value = object(current)?;
    let after = object(Some(after))?;
    Ok(value
        .get("providers")
        .and_then(Value::as_object)
        .and_then(|providers| providers.get(PROVIDER))
        .is_some_and(|provider| {
            after
                .get("providers")
                .and_then(|providers| providers.get(PROVIDER))
                == Some(provider)
        }))
}

pub(super) fn restore_auth(
    before: Option<&str>,
    after: &str,
    current: Option<&str>,
) -> Result<Option<String>> {
    let mut value = object(current)?;
    let after = object(Some(after))?;
    ensure!(
        value
            .get(PROVIDER)
            .is_none_or(|credential| Some(credential) == after.get(PROVIDER)),
        "BitRouter Pi credential was edited"
    );
    value.remove(PROVIDER);
    encoded(value, before)
}

pub(super) fn auth_installed(after: &str, current: Option<&str>) -> Result<bool> {
    let value = object(current)?;
    let after = object(Some(after))?;
    Ok(value
        .get(PROVIDER)
        .is_some_and(|credential| Some(credential) == after.get(PROVIDER)))
}

/// Recover only a literal connector credential, without evaluating Pi commands
/// or environment templates while checking the saved connection.
pub(super) fn stored_token(after: &str) -> Result<String> {
    let auth = object(Some(after))?;
    let credential = auth.get(PROVIDER).context("missing saved Pi credential")?;
    ensure!(
        credential.get("type").and_then(Value::as_str) == Some("api_key"),
        "saved Pi credential is not an API key"
    );
    let key = credential
        .get("key")
        .and_then(Value::as_str)
        .context("missing saved Pi API key")?;
    ensure!(
        !key.starts_with('!'),
        "executable Pi credentials cannot be checked automatically"
    );
    let mut token = String::new();
    let mut characters = key.chars();
    while let Some(character) = characters.next() {
        if character == '$' {
            match characters.next() {
                Some('$') => token.push('$'),
                Some('!') => token.push('!'),
                _ => anyhow::bail!("Pi credential expressions cannot be checked automatically"),
            }
        } else {
            token.push(character);
        }
    }
    ensure!(!token.is_empty(), "saved Pi API key is empty");
    Ok(token)
}

pub(super) fn restore_settings(
    before: Option<&str>,
    _after: &str,
    current: Option<&str>,
    model: &str,
) -> Result<Option<String>> {
    let mut value = object(current)?;
    if value.get("defaultProvider").and_then(Value::as_str) == Some(PROVIDER)
        && value.get("defaultModel").and_then(Value::as_str) == Some(model)
    {
        let before_value = object(before)?;
        for key in ["defaultProvider", "defaultModel"] {
            match before_value.get(key) {
                Some(original) => {
                    value.insert(key.into(), original.clone());
                }
                None => {
                    value.remove(key);
                }
            }
        }
    }
    encoded(value, before)
}

pub(super) fn settings_installed(_after: &str, current: Option<&str>) -> Result<bool> {
    let _ = object(current)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "team/automatos-exact";

    #[test]
    fn independent_provider_auth_and_defaults_survive_reconnect_and_restore() -> Result<()> {
        for selected in ["bitrouter", "native"] {
            let temp = tempfile::tempdir()?;
            let root = temp.path().canonicalize()?;
            let models = json!({"providers":{"native":{"baseUrl":"https://native.invalid/v1"}},"future":true});
            let auth = json!({"native":{"type":"api_key","key":"native-key"}});
            let settings = "{\"defaultProvider\":\"native\",\"defaultModel\":\"native-original\",\"theme\":\"dark\"}\n";
            std::fs::write(root.join("models.json"), models.to_string())?;
            std::fs::write(root.join("auth.json"), auth.to_string())?;
            std::fs::write(root.join("settings.json"), settings)?;
            let prepared = prepare(
                "http://127.0.0.1:4356",
                "gateway-key",
                MODEL,
                "bitrouter/automatos",
                Some(&root),
                Protocol::ChatCompletions,
            )?;
            let receipt =
                files::Receipt::new("http://127.0.0.1:4356".into(), MODEL.into(), prepared.files);
            let journal = root.join("receipt.json");
            receipt.apply(&journal)?;
            assert_eq!(
                std::fs::read_to_string(root.join("settings.json"))?,
                settings
            );
            let mut configured = files::json(&root.join("models.json"))?;
            assert_eq!(
                configured["providers"][PROVIDER]["models"][0],
                json!({"id":MODEL,"name":"bitrouter/automatos"})
            );
            assert_eq!(
                configured["providers"]["native"],
                models["providers"]["native"]
            );
            configured["providers"]["later-native"] = json!({"models":[{"id":"later"}]});
            std::fs::write(root.join("models.json"), configured.to_string())?;
            let mut credentials = files::json(&root.join("auth.json"))?;
            credentials["native"]["key"] = json!("native-rotated");
            std::fs::write(root.join("auth.json"), credentials.to_string())?;
            std::fs::write(root.join("settings.json"), json!({"defaultProvider":selected,"defaultModel": if selected == "bitrouter" {MODEL} else {"later-native"},"theme":"light"}).to_string())?;
            assert!(receipt.installed()?);
            assert!(
                prepare(
                    "http://127.0.0.1:4356",
                    "key",
                    MODEL,
                    MODEL,
                    Some(&root),
                    Protocol::Responses
                )
                .is_err()
            );
            receipt.restore(&journal)?;
            let restored = files::json(&root.join("models.json"))?;
            assert!(restored["providers"].get(PROVIDER).is_none());
            assert!(restored["providers"].get("later-native").is_some());
            let restored = files::json(&root.join("auth.json"))?;
            assert!(restored.get(PROVIDER).is_none());
            assert_eq!(restored["native"]["key"], "native-rotated");
            let restored = files::json(&root.join("settings.json"))?;
            assert_eq!(restored["defaultProvider"], "native");
            assert_eq!(
                restored["defaultModel"],
                if selected == "bitrouter" {
                    "native-original"
                } else {
                    "later-native"
                }
            );
            assert_eq!(restored["theme"], "light");
        }
        Ok(())
    }

    #[test]
    fn exact_restore_and_owned_conflicts_are_atomic() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let receipt = files::Receipt::new(
            "http://localhost".into(),
            MODEL.into(),
            prepare(
                "http://localhost",
                "!secret$KEY",
                MODEL,
                MODEL,
                Some(&root),
                Protocol::Responses,
            )?
            .files,
        );
        let journal = root.join("receipt.json");
        receipt.apply(&journal)?;
        assert_eq!(
            files::json(&root.join("auth.json"))?[PROVIDER]["key"],
            "$!secret$$KEY"
        );
        let original = std::fs::read_to_string(root.join("models.json"))?;
        let mut auth = files::json(&root.join("auth.json"))?;
        auth[PROVIDER]["key"] = json!("changed");
        std::fs::write(root.join("auth.json"), auth.to_string())?;
        assert!(!receipt.installed()?);
        assert!(receipt.restore(&journal).is_err());
        assert_eq!(std::fs::read_to_string(root.join("models.json"))?, original);
        assert!(journal.exists());
        auth[PROVIDER]["key"] = json!("$!secret$$KEY");
        std::fs::write(root.join("auth.json"), auth.to_string())?;
        receipt.restore(&journal)?;
        for name in ["models.json", "auth.json", "settings.json", "receipt.json"] {
            assert!(!root.join(name).exists());
        }
        Ok(())
    }

    #[test]
    fn protocol_paths_and_invalid_profiles_are_checked_before_writes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        for (protocol, api, url) in [
            (
                Protocol::ChatCompletions,
                "openai-completions",
                "http://localhost/proxy/v1",
            ),
            (
                Protocol::Responses,
                "openai-responses",
                "http://localhost/proxy/v1",
            ),
            (
                Protocol::Messages,
                "anthropic-messages",
                "http://localhost/proxy",
            ),
        ] {
            let changes = prepare(
                "http://localhost/proxy",
                "key",
                MODEL,
                MODEL,
                Some(&root),
                protocol,
            )?;
            let receipt =
                files::Receipt::new("http://localhost/proxy".into(), MODEL.into(), changes.files);
            let journal = root.join("receipt.json");
            receipt.apply(&journal)?;
            let configured = files::json(&root.join("models.json"))?;
            assert_eq!(configured["providers"][PROVIDER]["baseUrl"], url);
            assert_eq!(configured["providers"][PROVIDER]["api"], api);
            receipt.restore(&journal)?;
        }
        std::fs::write(root.join("models.json"), "{\"providers\":[]}")?;
        assert!(
            prepare(
                "http://localhost",
                "key",
                MODEL,
                MODEL,
                Some(&root),
                Protocol::ChatCompletions
            )
            .is_err()
        );
        assert!(!root.join("auth.json").exists());
        Ok(())
    }

    #[test]
    fn saved_token_is_literal_and_never_evaluates_config_expressions() -> Result<()> {
        for (key, token) in [
            ("fixture-key", "fixture-key"),
            ("$!fixture$$TOKEN", "!fixture$TOKEN"),
            ("a$$!b", "a$!b"),
        ] {
            let auth = json!({PROVIDER:{"type":"api_key","key":key}}).to_string();
            assert_eq!(stored_token(&auth)?, token);
        }
        for key in ["!some-command", "$TOKEN", "${TOKEN}", ""] {
            let auth = json!({PROVIDER:{"type":"api_key","key":key}}).to_string();
            assert!(stored_token(&auth).is_err());
        }
        Ok(())
    }
}
