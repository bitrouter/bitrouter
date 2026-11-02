//! OpenClaw's instance-wide custom provider and its agent-local credential replicas.
#[path = "openclaw/json5.rs"]
mod document;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::files::{self, Change, Restore};
use super::{Prepared, Protocol};

const PROVIDER: &str = "bitrouter";
const ALIAS: &str = "automatos";
const SECRET_PROVIDER: &str = "bitrouter-connect";

fn environment_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home() -> Result<PathBuf> {
    environment_path("OPENCLAW_HOME")
        .or_else(|| environment_path("HOME"))
        .or_else(|| environment_path("USERPROFILE"))
        .context("cannot locate OpenClaw home; supply --instance")
}

fn expand(path: PathBuf) -> Result<PathBuf> {
    let path = match path.to_str() {
        Some("~") => home()?,
        Some(text) if text.starts_with("~/") => home()?.join(&text[2..]),
        Some(text) => {
            ensure!(
                !text.contains("${"),
                "OpenClaw path contains environment substitution; use a resolved path"
            );
            path
        }
        None => anyhow::bail!("OpenClaw configuration path must be UTF-8"),
    };
    super::absolute(path)
}

pub(super) fn instance(explicit: Option<&Path>) -> Result<PathBuf> {
    let path = if let Some(path) = explicit {
        if path.is_file()
            || path
                .extension()
                .is_some_and(|extension| extension == "json" || extension == "json5")
        {
            path.to_owned()
        } else {
            path.join("openclaw.json")
        }
    } else if let Some(path) = environment_path("OPENCLAW_CONFIG_PATH") {
        path
    } else if let Some(path) = environment_path("OPENCLAW_STATE_DIR") {
        path.join("openclaw.json")
    } else {
        ensure!(
            std::env::var("OPENCLAW_PROFILE")
                .ok()
                .is_none_or(|value| value.trim().is_empty() || value == "default"),
            "OpenClaw named profile requires --instance pointing to its state directory"
        );
        home()?.join(".openclaw/openclaw.json")
    };
    let path = expand(path)?;
    // OpenClaw resolves config and agent state independently. Refuse a known
    // split instead of leaving its real generated credential catalogs unowned.
    let state = if let Some(path) = environment_path("OPENCLAW_STATE_DIR") {
        Some(expand(path)?)
    } else if let Some(config) = environment_path("OPENCLAW_CONFIG_PATH")
        && (explicit.is_none() || expand(config)? == path)
    {
        Some(home()?.join(".openclaw"))
    } else {
        None
    };
    ensure!(
        state
            .as_deref()
            .is_none_or(|state| path.parent() == Some(state)),
        "OpenClaw config and state directories differ; consolidate the selected openclaw.json into OPENCLAW_STATE_DIR before connecting"
    );
    Ok(path)
}

fn read(path: &Path) -> Result<(String, Option<String>)> {
    let before = files::read(path)?;
    Ok((before.clone().unwrap_or_else(|| "{}\n".into()), before))
}

fn no_includes(value: &Value) -> Result<()> {
    match value {
        Value::Object(object) => {
            ensure!(
                !object.contains_key("$include"),
                "OpenClaw $include configuration cannot be safely connected; select a self-contained instance or consolidate its provider/agent configuration first"
            );
            for child in object.values() {
                no_includes(child)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                no_includes(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn ref_for(model: &str) -> String {
    format!("{PROVIDER}/{model}")
}

fn alias_refs(model: &str) -> Vec<String> {
    let mut refs = vec![ref_for(model)];
    if model.starts_with("bitrouter/") {
        refs.push(model.into());
    }
    refs
}

fn aliases_match(actual: &Value, expected: &Value, model: &str) -> bool {
    let owned = expected
        .pointer("/agents/defaults/models")
        .and_then(|models| models.get(ref_for(model)));
    let models = actual.pointer("/agents/defaults/models");
    let values: Vec<_> = alias_refs(model)
        .iter()
        .filter_map(|reference| models.and_then(|models| models.get(reference)))
        .collect();
    !values.is_empty() && values.iter().all(|value| Some(*value) == owned)
}

fn allows(policy: Option<&Value>, reference: &str, scope: &str) -> Result<()> {
    let Some(policy) = policy else {
        return Ok(());
    };
    let policy = policy
        .as_object()
        .with_context(|| format!("OpenClaw {scope}.modelPolicy must be an object"))?;
    let Some(allow) = policy.get("allow") else {
        return Ok(());
    };
    let allow = allow
        .as_array()
        .with_context(|| format!("OpenClaw {scope}.modelPolicy.allow must be an array"))?;
    ensure!(
        allow.iter().all(Value::is_string),
        "OpenClaw {scope}.modelPolicy.allow must contain strings"
    );
    ensure!(
        allow.is_empty()
            || allow.iter().filter_map(Value::as_str).any(|entry| {
                let entry = entry.trim();
                entry == reference
                    || entry == ALIAS
                    || entry == "bitrouter/automatos"
                    || entry.strip_suffix("/*").is_some_and(|prefix| {
                        !prefix.is_empty()
                            && !prefix.contains('*')
                            && !prefix.split('/').any(|segment| {
                                segment.is_empty() || segment.chars().any(char::is_whitespace)
                            })
                            && reference
                                .strip_prefix(prefix)
                                .is_some_and(|suffix| suffix.starts_with('/'))
                    })
            }),
        "OpenClaw {scope}.modelPolicy.allow excludes {reference}; explicitly permit that exact model in OpenClaw before connecting"
    );
    Ok(())
}

fn permitted(value: &Value, reference: &str) -> Result<()> {
    no_includes(value)?;
    ensure!(
        value.pointer("/agents/list").is_none(),
        "OpenClaw legacy agents.list is unsupported; migrate with the current OpenClaw release before connecting"
    );
    let defaults = value.pointer("/agents/defaults");
    let policy = defaults.and_then(|defaults| defaults.get("modelPolicy"));
    if policy.is_none()
        && value.pointer("/meta/migrations/modelPolicyAllowlist") != Some(&Value::Bool(true))
        && let Some(models) = defaults
            .and_then(|defaults| defaults.get("models"))
            .and_then(Value::as_object)
    {
        ensure!(
            models.is_empty() || models.contains_key(reference),
            "OpenClaw agents.defaults.models is a legacy model allowlist that excludes {reference}; migrate and explicitly permit that model before connecting"
        );
    }
    allows(policy, reference, "agents.defaults")?;
    if let Some(entries) = value.pointer("/agents/entries") {
        for (id, entry) in entries
            .as_object()
            .context("OpenClaw agents.entries must be an object")?
        {
            let scope = format!("agents.entries.{id}");
            let own_policy = entry
                .get("modelPolicy")
                .filter(|policy| policy.get("allow").is_some());
            allows(own_policy.or(policy), reference, &scope)?;
            if let Some(models) = entry.get("models").and_then(Value::as_object) {
                ensure!(
                    !models.values().any(|value| value
                        .get("alias")
                        .and_then(Value::as_str)
                        .is_some_and(|alias| alias.eq_ignore_ascii_case(ALIAS))),
                    "OpenClaw {scope}.models already owns the automatos alias"
                );
            }
        }
    }
    Ok(())
}

fn node_at<'a>(root: &'a document::Node, keys: &[&str]) -> Option<&'a document::Node> {
    keys.iter().try_fold(root, |node, key| {
        node.member(key).map(|member| &member.node)
    })
}

fn set(text: &str, keys: &[&str], value: &Value) -> Result<String> {
    let root = document::parse(text)?;
    if let Some(node) = node_at(&root, keys) {
        let mut output = text.to_owned();
        output.replace_range(node.span.clone(), &serde_json::to_string_pretty(value)?);
        return Ok(output);
    }
    for length in (0..keys.len()).rev() {
        if let Some(parent) = node_at(&root, &keys[..length]) {
            let mut nested = value.clone();
            for key in keys[length + 1..].iter().rev() {
                nested = json!({*key: nested});
            }
            return document::insert(text, parent, keys[length], &nested);
        }
    }
    anyhow::bail!("cannot patch OpenClaw configuration")
}

fn remove(text: &str, keys: &[&str]) -> Result<String> {
    let root = document::parse(text)?;
    let Some(parent) = node_at(&root, &keys[..keys.len() - 1]) else {
        return Ok(text.into());
    };
    if parent.member(keys[keys.len() - 1]).is_none() {
        return Ok(text.into());
    }
    document::remove(text, parent, keys[keys.len() - 1])
}

fn catalog_paths(instance: &Path, value: &Value) -> Result<Vec<PathBuf>> {
    let state = instance
        .parent()
        .context("OpenClaw configuration has no parent")?;
    let mut paths = BTreeSet::new();
    let entries = value.pointer("/agents/entries").and_then(Value::as_object);
    if entries.is_none_or(|entries| entries.is_empty()) {
        paths.insert(state.join("agents/main/agent/models.json"));
    }
    if let Some(entries) = entries {
        for (id, entry) in entries {
            ensure!(
                !id.is_empty()
                    && !matches!(id.as_str(), "." | "..")
                    && !id.contains(['/', '\\'])
                    && !id.chars().any(char::is_control),
                "invalid OpenClaw agent ID"
            );
            let path = match entry.get("agentDir") {
                Some(path) => expand(PathBuf::from(
                    path.as_str()
                        .context("OpenClaw agentDir must be a string")?,
                ))?,
                None => state.join("agents").join(id).join("agent"),
            };
            paths.insert(path.join("models.json"));
        }
    }
    let agents = state.join("agents");
    if agents.is_dir() {
        for entry in std::fs::read_dir(agents)? {
            let entry = entry?;
            ensure!(
                !entry.file_type()?.is_symlink(),
                "OpenClaw agent directory is symlinked: {}; resolve its credential ownership before connecting or removing",
                entry.path().display()
            );
            if entry.file_type()?.is_dir() {
                paths.insert(entry.path().join("agent/models.json"));
            }
        }
    }
    if let Some(path) = environment_path("OPENCLAW_AGENT_DIR") {
        paths.insert(expand(path)?.join("models.json"));
    }
    Ok(paths.into_iter().collect())
}

pub(super) fn prepare(
    endpoint: &str,
    token: &str,
    model: &str,
    display_name: &str,
    explicit: Option<&Path>,
    profiles: &[String],
    protocol: Protocol,
) -> Result<Prepared> {
    ensure!(
        profiles.is_empty(),
        "OpenClaw --profile is unsupported; select the gateway state directory with --instance"
    );
    ensure!(
        protocol == Protocol::ChatCompletions,
        "OpenClaw connector currently verifies only --protocol chat-completions"
    );
    ensure!(
        std::env::var("OPENCLAW_CONFIG_READONLY").ok().as_deref() != Some("1")
            && std::env::var("OPENCLAW_NIX_MODE").ok().as_deref() != Some("1"),
        "OpenClaw configuration is read-only"
    );
    for (name, value) in [
        ("endpoint", endpoint),
        ("model", model),
        ("display name", display_name),
    ] {
        ensure!(
            !value.contains("${"),
            "OpenClaw {name} contains environment substitution syntax; supply a literal value"
        );
    }
    let path = instance(explicit)?;
    let (text, before) = read(&path)?;
    let root = document::parse(&text).context("invalid OpenClaw JSON5 configuration")?;
    let reference = ref_for(model);
    permitted(&root.value, &reference)?;
    ensure!(
        root.value
            .pointer("/models/providers")
            .and_then(Value::as_object)
            .is_none_or(|providers| !providers
                .keys()
                .any(|key| key.eq_ignore_ascii_case(PROVIDER))),
        "OpenClaw already defines a BitRouter provider"
    );
    let metadata = root
        .value
        .pointer("/agents/defaults/models")
        .and_then(Value::as_object);
    ensure!(
        metadata.is_none_or(|models| !models.values().any(|value| value
            .get("alias")
            .and_then(Value::as_str)
            .is_some_and(|alias| alias.eq_ignore_ascii_case(ALIAS)))),
        "OpenClaw already owns the automatos alias"
    );
    ensure!(
        alias_refs(model)
            .iter()
            .all(|reference| metadata.and_then(|models| models.get(reference)).is_none()),
        "OpenClaw already has metadata for {reference}; preserve it before connecting"
    );
    let credential_path = path
        .parent()
        .context("OpenClaw configuration has no parent")?
        .join("bitrouter-connect-credential.json");
    ensure!(
        files::read(&credential_path)?.is_none(),
        "OpenClaw credential file already exists: {}; preserve it before connecting",
        credential_path.display()
    );
    ensure!(
        root.value
            .pointer("/secrets/providers")
            .and_then(Value::as_object)
            .is_none_or(|providers| !providers.contains_key(SECRET_PROVIDER)),
        "OpenClaw already owns the bitrouter-connect secret provider"
    );
    let provider = json!({"baseUrl":format!("{}/v1",endpoint.trim_end_matches('/')), "api":"openai-completions", "auth":"api-key", "authHeader":true, "headers":{"x-bitrouter-harness":"openclaw"}, "apiKey":{"source":"file","provider":SECRET_PROVIDER,"id":"/apiKey"}, "models":[{"id":model,"name":display_name}]});
    let mut after = set(&text, &["models", "providers", PROVIDER], &provider)?;
    after = set(
        &after,
        &["secrets", "providers", SECRET_PROVIDER],
        &json!({"source":"file","path":credential_path,"mode":"json"}),
    )?;
    after = set(
        &after,
        &["agents", "defaults", "models", &reference],
        &json!({"alias":ALIAS}),
    )?;
    if root.value.pointer("/agents/defaults/modelPolicy").is_none()
        && root.value.pointer("/meta/migrations/modelPolicyAllowlist") != Some(&Value::Bool(true))
    {
        after = set(&after, &["agents", "defaults", "modelPolicy"], &json!({}))?;
    }
    let catalogs = catalog_paths(&path, &root.value)?;
    let mut changes = vec![
        Change::json(credential_path, None, &json!({"apiKey":token}))?
            .with_restore(Restore::OpenClawCredential),
    ];
    let mut catalog_provider = provider.clone();
    catalog_provider["apiKey"] = json!("secretref-managed");
    for catalog in &catalogs {
        let (text, before) = read(catalog)?;
        let root = document::parse(&text)
            .with_context(|| format!("invalid OpenClaw catalog {}", catalog.display()))?;
        no_includes(&root.value)?;
        ensure!(
            root.value
                .get("providers")
                .and_then(Value::as_object)
                .is_none_or(|providers| !providers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case(PROVIDER))),
            "OpenClaw agent catalog already defines BitRouter: {}",
            catalog.display()
        );
        changes.push(
            Change::snapshot(
                catalog.clone(),
                before,
                set(&text, &["providers", PROVIDER], &catalog_provider)?,
            )
            .with_restore(Restore::OpenClawModels),
        );
    }
    changes.insert(
        1,
        Change::snapshot(path.clone(), before, after).with_restore(Restore::OpenClawConfig {
            instance: path.clone(),
            catalogs: catalogs.clone(),
        }),
    );
    Ok(Prepared { files:changes, instance:path.display().to_string(), surfaces:vec!["openclaw_cli_tui".into(),"openclaw_gateway_control_ui".into(),"openclaw_macos_gateway_companion".into()], notes:vec![
        format!("Select bitrouter/automatos (alias automatos); the exact gateway request model remains {model}. Existing primary/fallback defaults are retained."),
        "OpenClaw 2026.10.9 custom provider configuration. Start/reload the selected gateway yourself; connected TUI, Control UI and Desktop companions use that gateway. Remote gateways must be configured on their host.".into(),
        "The private bitrouter-connect-credential.json stores the credential. A native file SecretRef keeps the key out of OpenClaw config backups and generated catalogs; auth: api-key prioritizes this credential over environment/profile defaults. No shell variable is needed when starting OpenClaw.".into(),
        format!("--instance declares the gateway state directory. Start native OpenClaw with OPENCLAW_STATE_DIR={} (or its matching native named profile); a custom config filename additionally requires OPENCLAW_CONFIG_PATH={}. Setting only a config path does not relocate native agent state. --profile does not select OpenClaw agent IDs.",path.parent().context("OpenClaw configuration has no parent")?.display(),path.display()),
        "Existing model policies are checked without widening them. An explicit empty policy preserves unrestricted selection while adding an alias on a fresh legacy configuration.".into(),
        "Only Chat Completions is accepted by this connector. Model limits, pricing, image and reasoning capabilities use OpenClaw defaults and are not verified gateway claims. Existing sessions selecting BitRouter must select another model after removal; newly created agent credential copies block removal until reconciled.".into(),
    ] })
}

pub(super) fn stored_token(after: &str) -> Result<String> {
    document::parse(after)?
        .value
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("connection receipt has no OpenClaw credential")
}

fn untracked(instance: &Path, value: &Value, catalogs: &[PathBuf]) -> Result<()> {
    for path in catalog_paths(instance, value)? {
        if catalogs.contains(&path) {
            continue;
        }
        if let Some(text) = files::read(&path)? {
            let value = document::parse(&text)?.value;
            ensure!(
                value.pointer("/providers/bitrouter").is_none(),
                "OpenClaw has an unrecorded BitRouter credential catalog at {}; reconcile that agent before removal",
                path.display()
            );
        }
    }
    Ok(())
}

pub(super) fn config_installed(
    after: &str,
    current: Option<&str>,
    instance: &Path,
    catalogs: &[PathBuf],
) -> Result<bool> {
    let Some(current) = current else {
        return Ok(false);
    };
    let expected = document::parse(after)?.value;
    let actual = document::parse(current)?.value;
    let model = expected
        .pointer("/models/providers/bitrouter/models/0/id")
        .and_then(Value::as_str)
        .context("missing owned OpenClaw model")?;
    let reference = ref_for(model);
    Ok(permitted_installed(&actual, &reference).is_ok()
        && untracked(instance, &actual, catalogs).is_ok()
        && actual.pointer("/models/providers/bitrouter")
            == expected.pointer("/models/providers/bitrouter")
        && actual.pointer("/secrets/providers/bitrouter-connect")
            == expected.pointer("/secrets/providers/bitrouter-connect")
        && aliases_match(&actual, &expected, model))
}

fn permitted_installed(value: &Value, reference: &str) -> Result<()> {
    // The owned inherited alias is expected during checks; per-agent alias edits
    // still require explicit reconciliation rather than changing their meaning.
    permitted(value, reference)
}

fn cleanup_empty(mut text: String, before: &Value, paths: &[&[&str]]) -> Result<String> {
    for keys in paths {
        let value = document::parse(&text)?.value;
        let pointer = format!("/{}", keys.join("/"));
        if before.pointer(&pointer).is_none()
            && value
                .pointer(&pointer)
                .is_some_and(|value| value.as_object().is_some_and(|object| object.is_empty()))
        {
            text = remove(&text, keys)?;
        }
    }
    Ok(text)
}

fn owned_selection(value: &str, reference: &str) -> bool {
    value == reference || value == "bitrouter/automatos" || value == ALIAS
}

fn uses_secret(value: &Value, path: Option<&Value>) -> bool {
    match value {
        Value::Object(object) => {
            (object.get("source").and_then(Value::as_str) == Some("file")
                && object.get("provider").and_then(Value::as_str) == Some(SECRET_PROVIDER))
                || path.is_some_and(|path| object.get("path") == Some(path))
                || object.values().any(|value| uses_secret(value, path))
        }
        Value::Array(array) => array.iter().any(|value| uses_secret(value, path)),
        _ => false,
    }
}

fn restore_selection(text: &str, before: &Value, keys: &[&str], reference: &str) -> Result<String> {
    let root = document::parse(text)?.value;
    let Some(value) = keys.iter().try_fold(&root, |value, key| value.get(*key)) else {
        return Ok(text.into());
    };
    let previous = keys.iter().try_fold(before, |value, key| value.get(*key));
    let mut restored = value.clone();
    if value
        .as_str()
        .is_some_and(|value| owned_selection(value, reference))
    {
        return match previous {
            Some(previous) => set(text, keys, previous),
            None => remove(text, keys),
        };
    }
    if let Some(object) = restored.as_object_mut() {
        if object
            .get("primary")
            .and_then(Value::as_str)
            .is_some_and(|value| owned_selection(value, reference))
        {
            match previous
                .and_then(|value| value.get("primary"))
                .or_else(|| previous.filter(|value| value.is_string()))
            {
                Some(previous) => {
                    object.insert("primary".into(), previous.clone());
                }
                None => {
                    object.remove("primary");
                }
            }
        }
        if let Some(fallbacks) = object.get_mut("fallbacks").and_then(Value::as_array_mut) {
            fallbacks.retain(|value| {
                !value
                    .as_str()
                    .is_some_and(|value| owned_selection(value, reference))
            });
        }
        if object.is_empty() {
            return remove(text, keys);
        }
    }
    if restored == *value {
        Ok(text.into())
    } else {
        set(text, keys, &restored)
    }
}

pub(super) fn restore_config(
    before: Option<&str>,
    after: &str,
    current: Option<&str>,
    instance: &Path,
    catalogs: &[PathBuf],
) -> Result<Option<String>> {
    let original = document::parse(before.unwrap_or("{}"))?.value;
    let expected = document::parse(after)?.value;
    let mut text = current.unwrap_or("{}").to_owned();
    let actual = document::parse(&text)?.value;
    no_includes(&actual)?;
    untracked(instance, &actual, catalogs)?;
    let model = expected
        .pointer("/models/providers/bitrouter/models/0/id")
        .and_then(Value::as_str)
        .context("missing owned OpenClaw model")?;
    let reference = ref_for(model);
    let expected_secret = expected
        .pointer("/secrets/providers/bitrouter-connect")
        .context("missing owned OpenClaw secret provider")?;
    let actual_secret = actual.pointer("/secrets/providers/bitrouter-connect");
    ensure!(
        actual_secret.is_none() || actual_secret == Some(expected_secret),
        "BitRouter OpenClaw secret provider was edited"
    );
    let mut other_references = actual.clone();
    if let Some(providers) = other_references
        .pointer_mut("/models/providers")
        .and_then(Value::as_object_mut)
    {
        providers.remove(PROVIDER);
    }
    if let Some(providers) = other_references
        .pointer_mut("/secrets/providers")
        .and_then(Value::as_object_mut)
    {
        providers.remove(SECRET_PROVIDER);
    }
    ensure!(
        !uses_secret(&other_references, Some(&expected_secret["path"]))
            && actual
                .pointer("/secrets/defaults/file")
                .and_then(Value::as_str)
                != Some(SECRET_PROVIDER),
        "another OpenClaw setting now references the BitRouter credential; preserve or replace that reference before removal"
    );
    ensure!(
        actual.pointer("/models/providers/bitrouter").is_none()
            || actual.pointer("/models/providers/bitrouter")
                == expected.pointer("/models/providers/bitrouter"),
        "BitRouter OpenClaw provider was edited"
    );
    text = remove(&text, &["models", "providers", PROVIDER])?;
    let expected_alias = expected
        .pointer("/agents/defaults/models")
        .and_then(|models| models.get(&reference));
    for reference in alias_refs(model) {
        let actual_alias = actual
            .pointer("/agents/defaults/models")
            .and_then(|models| models.get(&reference));
        ensure!(
            actual_alias.is_none() || actual_alias == expected_alias,
            "BitRouter OpenClaw alias was edited"
        );
        text = remove(&text, &["agents", "defaults", "models", &reference])?;
    }
    text = remove(&text, &["secrets", "providers", SECRET_PROVIDER])?;
    let remaining_metadata = document::parse(&text)?
        .value
        .pointer("/agents/defaults/models")
        .and_then(Value::as_object)
        .is_some_and(|models| !models.is_empty());
    if original.pointer("/agents/defaults/modelPolicy").is_none()
        && actual.pointer("/agents/defaults/modelPolicy") == Some(&json!({}))
        && (!remaining_metadata
            || actual.pointer("/meta/migrations/modelPolicyAllowlist") == Some(&Value::Bool(true)))
    {
        text = remove(&text, &["agents", "defaults", "modelPolicy"])?;
    }
    let mut scopes = vec![vec!["agents", "defaults"]];
    if let Some(entries) = actual.pointer("/agents/entries").and_then(Value::as_object) {
        for id in entries.keys() {
            scopes.push(vec!["agents", "entries", id]);
        }
    }
    for scope in scopes {
        for key in ["model", "imageModel", "utilityModel"] {
            let mut keys = scope.clone();
            keys.push(key);
            text = restore_selection(&text, &original, &keys, &reference)?;
        }
    }
    text = cleanup_empty(
        text,
        &original,
        &[
            &["models", "providers"],
            &["models"],
            &["secrets", "providers"],
            &["secrets"],
            &["agents", "defaults", "models"],
            &["agents", "defaults"],
            &["agents"],
        ],
    )?;
    if document::parse(&text)?.value == original {
        Ok(before.map(str::to_owned))
    } else {
        Ok(Some(text))
    }
}

pub(super) fn models_installed(after: &str, current: Option<&str>) -> Result<bool> {
    let Some(current) = current else {
        return Ok(false);
    };
    Ok(document::parse(current)?
        .value
        .pointer("/providers/bitrouter")
        == document::parse(after)?
            .value
            .pointer("/providers/bitrouter"))
}

pub(super) fn restore_models(
    before: Option<&str>,
    after: &str,
    current: Option<&str>,
) -> Result<Option<String>> {
    let original = document::parse(before.unwrap_or("{}"))?.value;
    let expected = document::parse(after)?.value;
    let text = current.unwrap_or("{}");
    let actual = document::parse(text)?.value;
    no_includes(&actual)?;
    let mut other_providers = actual.clone();
    if let Some(providers) = other_providers
        .get_mut("providers")
        .and_then(Value::as_object_mut)
    {
        providers.remove(PROVIDER);
    }
    ensure!(
        !uses_secret(&other_providers, None),
        "another OpenClaw agent provider now references the BitRouter credential; preserve or replace it before removal"
    );
    ensure!(
        actual.pointer("/providers/bitrouter").is_none()
            || actual.pointer("/providers/bitrouter") == expected.pointer("/providers/bitrouter"),
        "BitRouter OpenClaw agent catalog was edited"
    );
    let text = cleanup_empty(
        remove(text, &["providers", PROVIDER])?,
        &original,
        &[&["providers"]],
    )?;
    if document::parse(&text)?.value == original {
        Ok(before.map(str::to_owned))
    } else {
        Ok(Some(text))
    }
}
