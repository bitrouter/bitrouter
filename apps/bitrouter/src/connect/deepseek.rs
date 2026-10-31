//! Native Harness provider configuration. No npm command manages Desktop state.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};

use super::files::{self, Change, Restore};
use super::{Prepared, Protocol};

const PROVIDER: &str = "bitrouter";
const CREDENTIAL: &str = "BITROUTER_CONNECT_API_KEY";
const PATCH: &str = "cordis.patch.yml";

pub(super) fn instance(explicit: Option<&Path>) -> Result<PathBuf> {
    let path = match explicit {
        Some(path) => path.to_path_buf(),
        None => match std::env::var_os("DSH_HOME").filter(|value| !value.is_empty()) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(std::env::var_os("HOME").context("cannot locate user home")?)
                .join(".dsh"),
        },
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    // Do not canonicalize away a symlink that the shared transaction must reject.
    ensure!(
        path.is_dir(),
        "Harness home was not detected: {}",
        path.display()
    );
    Ok(path)
}

fn profile_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !matches!(name, "." | ".." | "node_modules")
            && !name.contains(['/', '\\'])
            && !name.chars().any(char::is_control),
        "invalid Harness profile name"
    );
    Ok(())
}

fn bundles(home: &Path, name: &str) -> Result<Vec<String>> {
    profile_name(name)?;
    let path = home.join("profiles").join(name).join("package.json");
    let text = files::read(&path)?.with_context(|| {
        format!("Harness profile {name} is not initialized; open its owning application first")
    })?;
    let value: Value = serde_json::from_str(&text).context("invalid Harness profile manifest")?;
    let bundles = value
        .pointer("/dsh/profile/bundles")
        .and_then(Value::as_array)
        .with_context(|| format!("{name} has no Harness profile bundle manifest"))?;
    let bundles = bundles
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("invalid Harness profile bundle")
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        bundles.iter().any(|name| name == "@deepseek-ai/dsh-base"),
        "Harness profile {name} does not contain the built-in base provider adapter"
    );
    Ok(bundles)
}

pub(super) fn profiles(home: &Path, requested: &[String]) -> Result<Vec<String>> {
    let mut selected = BTreeSet::new();
    if requested.is_empty() {
        for entry in std::fs::read_dir(home.join("profiles"))
            .context("no initialized Harness profiles were detected")?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name() == "node_modules" {
                continue;
            }
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Harness profile name is not UTF-8"))?;
            if entry.path().join("package.json").is_file() {
                bundles(home, &name)?;
                selected.insert(name);
            }
        }
    } else {
        for name in requested {
            bundles(home, name)?;
            selected.insert(name.clone());
        }
    }
    ensure!(
        !selected.is_empty(),
        "no initialized Harness profiles were detected; open Web, Desktop, or headless first, then select its --profile"
    );
    Ok(selected.into_iter().collect())
}

/// Preserve every untouched YAML row verbatim, including JavaScript tags.
struct PatchDocument {
    text: String,
    rows: Vec<Value>,
    starts: Vec<usize>,
    replacements: std::collections::BTreeMap<usize, Option<Value>>,
    appended: Vec<Value>,
}

impl PatchDocument {
    fn parse(text: Option<&str>) -> Result<Self> {
        let text = text.unwrap_or("").to_owned();
        let parsed: Vec<serde_saphyr::Spanned<Value>> = if text.trim().is_empty() {
            vec![]
        } else {
            serde_saphyr::from_str(&text)
                .map_err(|_| anyhow::anyhow!("expected a Harness YAML patch array"))?
        };
        let mut starts = Vec::new();
        for row in &parsed {
            ensure!(row.value.is_object(), "Harness patch row must be an object");
            let line = usize::try_from(row.referenced.line())
                .context("Harness patch line exceeds platform limits")?;
            ensure!(line > 0, "cannot locate Harness YAML patch row");
            let start = text
                .split_inclusive('\n')
                .take(line - 1)
                .map(str::len)
                .sum::<usize>();
            starts.push(start);
        }
        // Flow YAML/JSON is safe to reserialize only without custom YAML tags.
        let block = starts.windows(2).all(|pair| pair[0] < pair[1])
            && starts
                .iter()
                .all(|start| text[*start..].trim_start().starts_with("- "));
        let rows = parsed.into_iter().map(|row| row.value).collect();
        if !block {
            ensure!(
                !text
                    .lines()
                    .filter(|line| !line.trim_start().starts_with('#'))
                    .any(|line| line.contains('!')),
                "flow-style Harness patches containing YAML tags must be converted to block YAML first"
            );
            starts.clear();
        }
        Ok(Self {
            text,
            rows,
            starts,
            replacements: Default::default(),
            appended: vec![],
        })
    }

    fn index(&self, id: &str) -> Result<Option<usize>> {
        for row in &self.rows {
            if row.get("id").and_then(Value::as_str) != Some(id) {
                ensure!(
                    !contains_id(row, id),
                    "nested Harness patch for {id} cannot be safely edited; use an explicit top-level override"
                );
            }
        }
        Ok(self
            .rows
            .iter()
            .rposition(|row| row.get("id").and_then(Value::as_str) == Some(id)))
    }

    fn row(&self, id: &str) -> Result<Option<&Value>> {
        Ok(self.index(id)?.map(|index| &self.rows[index]))
    }

    fn set(&mut self, id: &str, row: Option<Value>) -> Result<()> {
        if let Some(index) = self.index(id)? {
            if !self.starts.is_empty() {
                let end = self
                    .starts
                    .get(index + 1)
                    .copied()
                    .unwrap_or(self.text.len());
                ensure!(
                    !self.text[self.starts[index]..end].contains("!!"),
                    "Harness {id} override contains executable/tagged YAML; edit it explicitly before connecting"
                );
            }
            self.replacements.insert(index, row);
        } else if let Some(row) = row {
            self.appended.push(row);
        }
        Ok(())
    }

    fn render(&self) -> Result<String> {
        if self.starts.is_empty() {
            let mut rows: Vec<_> = self
                .rows
                .iter()
                .enumerate()
                .filter_map(|(index, value)| {
                    self.replacements
                        .get(&index)
                        .cloned()
                        .unwrap_or_else(|| Some(value.clone()))
                })
                .collect();
            rows.extend(self.appended.clone());
            let prefix = if self.rows.is_empty() {
                self.text
                    .lines()
                    .filter(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
                    .map(|line| format!("{line}\n"))
                    .collect::<String>()
            } else {
                String::new()
            };
            return Ok(format!("{prefix}{}", serde_saphyr::to_string(&rows)?));
        }
        let mut text = self.text[..self.starts[0]].to_owned();
        for (index, row) in self.rows.iter().enumerate() {
            let end = self
                .starts
                .get(index + 1)
                .copied()
                .unwrap_or(self.text.len());
            if let Some(replacement) = self.replacements.get(&index) {
                if let Some(replacement) = replacement {
                    text.push_str(&serde_saphyr::to_string(&[replacement])?);
                }
            } else {
                let _ = row;
                text.push_str(&self.text[self.starts[index]..end]);
            }
        }
        if !self.appended.is_empty() {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&serde_saphyr::to_string(&self.appended)?);
        }
        Ok(text)
    }
}

fn contains_id(value: &Value, id: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.get("id").and_then(Value::as_str) == Some(id)
                || map.values().any(|value| contains_id(value, id))
        }
        Value::Array(values) => values.iter().any(|value| contains_id(value, id)),
        _ => false,
    }
}

fn object(value: Option<&Value>) -> Result<Map<String, Value>> {
    value
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_object()
                .cloned()
                .context("Harness configuration must be an object")
        })
        .unwrap_or_else(|| Ok(Map::new()))
}

fn route(document: &PatchDocument) -> Result<Option<&Value>> {
    Ok(document
        .row("llm-pi-ai")?
        .and_then(|row| row.pointer("/config/providers/bitrouter")))
}

pub(super) fn prepare(
    endpoint: &str,
    token: &str,
    model: &str,
    display_name: &str,
    explicit: Option<&Path>,
    requested: &[String],
    protocol: Protocol,
) -> Result<Prepared> {
    let home = instance(explicit)?;
    let selected = profiles(&home, requested)?;
    let global = PatchDocument::parse(files::read(&home.join(PATCH))?.as_deref())?;
    for id in ["llm-pi-ai", "credentials"] {
        ensure!(
            global.row(id)?.is_none(),
            "home-level Harness {id} override takes precedence; move it to selected profiles before connecting"
        );
    }
    let credential_path = home.join(".credentials.yaml");
    let credential_before = files::read(&credential_path)?;
    let mut credential_value = credentials(credential_before.as_deref())?;
    let mut refs = object(credential_value.get("refs"))?;
    ensure!(
        !refs.contains_key(CREDENTIAL),
        "Harness credential {CREDENTIAL} already exists; choose a separate Harness instance or remove the unmanaged collision"
    );
    refs.insert(CREDENTIAL.into(), Value::String(token.into()));
    credential_value.insert("version".into(), json!(1));
    credential_value.insert("refs".into(), Value::Object(refs));
    let owned: Vec<_> = selected
        .iter()
        .map(|name| home.join("profiles").join(name).join(PATCH))
        .collect();
    let mut changes = vec![
        Change::snapshot(
            credential_path,
            credential_before,
            serde_saphyr::to_string(&credential_value)?,
        )
        .with_restore(Restore::DeepseekCredentials {
            instance: home.clone(),
            profiles: owned.clone(),
        }),
    ];
    let root = endpoint.trim_end_matches('/');
    let (api, base_url) = match protocol {
        Protocol::ChatCompletions => ("openai-completions", format!("{root}/v1")),
        Protocol::Responses => ("openai-responses", format!("{root}/v1")),
        // The Anthropic SDK adds /v1/messages itself; OpenAI adds only its resource.
        Protocol::Messages => ("anthropic-messages", root.to_owned()),
    };
    let provider = json!({"displayName":"BitRouter", "apiKeyEnv":CREDENTIAL, "api":api,
        "baseURL":base_url,
        "models":[{"id":model,"name":display_name,"input":["text"]}]});
    let mut surfaces = vec![];
    for (name, path) in selected.iter().zip(owned) {
        let before = files::read(&path)?;
        let mut document = PatchDocument::parse(before.as_deref())?;
        ensure!(
            route(&document)?.is_none(),
            "Harness profile {name} already has a bitrouter provider"
        );
        let existing = document.row("llm-pi-ai")?;
        let known_bundles = bundles(&home, name)?.iter().all(|name| {
            matches!(
                name.as_str(),
                "@deepseek-ai/dsh-base"
                    | "@deepseek-ai/dsh-web-app"
                    | "@deepseek-ai/dsh-headless"
                    | "@deepseek-ai/dsh-sdk-app"
                    | "@deepseek-ai/dsh-acp-app"
            )
        });
        ensure!(
            known_bundles || existing.and_then(|row| row.get("config")).is_some(),
            "Harness profile {name} has custom bundles without a complete llm-pi-ai override; expose its composed provider config as an explicit profile override first"
        );
        let mut row = object(existing)?;
        ensure!(
            row.get("disabled").is_none_or(|value| value == false),
            "Harness profile {name} disables llm-pi-ai; enable it explicitly first"
        );
        let mut config = object(row.get("config"))?;
        let mut providers = object(config.get("providers"))?;
        providers.insert(PROVIDER.into(), provider.clone());
        config.insert("providers".into(), Value::Object(providers));
        row.insert("id".into(), json!("llm-pi-ai"));
        row.insert("config".into(), Value::Object(config));
        document.set("llm-pi-ai", Some(Value::Object(row)))?;
        let mut credential_row = object(document.row("credentials")?)?;
        ensure!(credential_row.get("config").is_none_or(|value| value.is_null() || value.as_object().is_some_and(Map::is_empty)),
            "Harness profile {name} redirects its credential store; configure that store explicitly");
        credential_row.insert("id".into(), json!("credentials"));
        credential_row.insert("disabled".into(), json!(false));
        document.set("credentials", Some(Value::Object(credential_row)))?;
        changes.push(
            Change::snapshot(path, before, document.render()?)
                .with_restore(Restore::DeepseekProfile),
        );
        let surface = if name == "desktop" {
            "desktop"
        } else if name == "web" {
            "web"
        } else if name == "headless" {
            "headless"
        } else {
            "custom"
        };
        surfaces.push(format!("{surface}:{name}"));
    }
    Ok(Prepared { files: changes, instance: home.to_string_lossy().into_owned(), surfaces,
        notes: vec![format!("Configured Harness profiles: {}. Uninitialized profiles are skipped; after initializing another profile, use --update with --profile for every desired profile. An update without profile flags retains the recorded scope.", selected.join(", ")),
            "Existing providers and default model are retained. Select the BitRouter model in each profile; profile edits apply to its next request (restart headless processes).".into(),
            "Desktop owns its reserved desktop profile and runtime. The connector only edits its initialized profile; it never launches npm commands for Desktop.".into()] })
}

pub(super) fn profile_installed(after: &str, current: Option<&str>) -> Result<bool> {
    let after = PatchDocument::parse(Some(after))?;
    let current = PatchDocument::parse(current)?;
    Ok(route(&current)? == route(&after)?
        && route(&after)?.is_some()
        && current
            .row("credentials")?
            .and_then(|row| row.get("disabled"))
            == Some(&json!(false)))
}

pub(super) fn restore_profile(
    before: Option<&str>,
    after: &str,
    current: Option<&str>,
) -> Result<Option<String>> {
    if current == before {
        return Ok(before.map(str::to_owned));
    }
    let prior = PatchDocument::parse(before)?;
    let installed = PatchDocument::parse(Some(after))?;
    let mut document = PatchDocument::parse(current)?;
    let current_route = route(&document)?;
    ensure!(
        current_route.is_none() || current_route == route(&installed)?,
        "BitRouter Harness provider was edited"
    );
    if current_route.is_some() {
        let mut row = object(document.row("llm-pi-ai")?)?;
        let mut config = object(row.get("config"))?;
        let mut providers = object(config.get("providers"))?;
        providers.remove(PROVIDER);
        if providers.is_empty()
            && prior
                .row("llm-pi-ai")?
                .and_then(|row| row.pointer("/config/providers"))
                .is_none()
        {
            config.remove("providers");
        } else {
            config.insert("providers".into(), Value::Object(providers));
        }
        if config.is_empty()
            && prior
                .row("llm-pi-ai")?
                .and_then(|row| row.get("config"))
                .is_none()
        {
            row.remove("config");
        } else {
            row.insert("config".into(), Value::Object(config));
        }
        let replacement = if row.len() == 1 && prior.row("llm-pi-ai")?.is_none() {
            None
        } else {
            Some(Value::Object(row))
        };
        document.set("llm-pi-ai", replacement)?;
    }
    // Choosing a model in the native picker writes this preference after connect.
    // Restore only a default that still names the provider being removed.
    if document
        .row("agent-default-model")?
        .and_then(|row| row.pointer("/config/provider"))
        .and_then(Value::as_str)
        == Some(PROVIDER)
    {
        let mut row = object(document.row("agent-default-model")?)?;
        let mut config = object(row.get("config"))?;
        let prior_config = prior
            .row("agent-default-model")?
            .and_then(|row| row.get("config"));
        for key in ["provider", "model", "reasoningEffort"] {
            match prior_config.and_then(|value| value.get(key)) {
                Some(value) => {
                    config.insert(key.into(), value.clone());
                }
                None => {
                    config.remove(key);
                }
            }
        }
        if config.is_empty() && prior_config.is_none() {
            row.remove("config");
        } else {
            row.insert("config".into(), Value::Object(config));
        }
        let replacement = if row.len() == 1 && prior.row("agent-default-model")?.is_none() {
            None
        } else {
            Some(Value::Object(row))
        };
        document.set("agent-default-model", replacement)?;
    }
    if let Some(row) = document.row("credentials")?
        && row.get("disabled") == Some(&json!(false))
    {
        let mut row = object(Some(row))?;
        match prior
            .row("credentials")?
            .and_then(|row| row.get("disabled"))
        {
            Some(value) => {
                row.insert("disabled".into(), value.clone());
            }
            None => {
                row.remove("disabled");
            }
        }
        let replacement = if row.len() == 1 && prior.row("credentials")?.is_none() {
            None
        } else {
            Some(Value::Object(row))
        };
        document.set("credentials", replacement)?;
    }
    let rendered = document.render()?;
    if PatchDocument::parse(Some(&rendered))?.rows == prior.rows {
        Ok(before.map(str::to_owned))
    } else {
        Ok(Some(rendered))
    }
}

fn credentials(text: Option<&str>) -> Result<Map<String, Value>> {
    let value: Value = match text.filter(|text| !text.trim().is_empty()) {
        Some(text) => serde_saphyr::from_str(text).map_err(|_| {
            anyhow::anyhow!("invalid Harness credential YAML; values are omitted from diagnostics")
        })?,
        None => json!({}),
    };
    let map = object(Some(&value))?;
    ensure!(
        map.is_empty() || map.get("version") == Some(&json!(1)),
        "unsupported Harness credential document; expected version 1"
    );
    let refs = object(map.get("refs"))?;
    ensure!(
        refs.values().all(Value::is_string),
        "invalid Harness credential reference values"
    );
    Ok(map)
}

pub(super) fn credentials_installed(after: &str, current: Option<&str>) -> Result<bool> {
    let after = credentials(Some(after))?;
    let current = credentials(current)?;
    Ok(current.get("refs").and_then(|value| value.get(CREDENTIAL))
        == after.get("refs").and_then(|value| value.get(CREDENTIAL)))
}

pub(super) fn stored_token(after: &str) -> Result<String> {
    credentials(Some(after))?
        .get("refs")
        .and_then(|value| value.get(CREDENTIAL))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .context("saved Harness connection has no gateway credential")
}

fn references(value: &Value) -> bool {
    match value {
        Value::String(value) => value == CREDENTIAL,
        Value::Object(values) => values.values().any(references),
        Value::Array(values) => values.iter().any(references),
        _ => false,
    }
}

pub(super) fn restore_credentials(
    before: Option<&str>,
    after: &str,
    current: Option<&str>,
    home: &Path,
    owned: &[PathBuf],
) -> Result<Option<String>> {
    if current == before {
        return Ok(before.map(str::to_owned));
    }
    let prior = credentials(before)?;
    let installed = credentials(Some(after))?;
    let mut current = credentials(current)?;
    let mut refs = object(current.get("refs"))?;
    let expected = installed
        .get("refs")
        .and_then(|value| value.get(CREDENTIAL));
    ensure!(
        refs.get(CREDENTIAL).is_none() || refs.get(CREDENTIAL) == expected,
        "BitRouter Harness credential was edited"
    );
    let mut patches = vec![home.join(PATCH)];
    for entry in std::fs::read_dir(home.join("profiles"))? {
        let entry = entry?;
        if entry.file_name() == "node_modules" {
            continue;
        }
        // Read even uninitialized profiles: they may have been created after connection.
        ensure!(
            !entry.file_type()?.is_symlink(),
            "cannot prove credential is unused in a symlinked Harness profile"
        );
        if entry.file_type()?.is_dir() {
            patches.push(entry.path().join(PATCH));
        }
    }
    for path in patches {
        let mut document = PatchDocument::parse(files::read(&path)?.as_deref())
            .with_context(|| format!("cannot prove credential is unused in {}", path.display()))?;
        if path == home.join(PATCH) {
            ensure!(
                document
                    .row("agent-default-model")?
                    .and_then(|row| row.pointer("/config/provider"))
                    .and_then(Value::as_str)
                    != Some(PROVIDER),
                "home-level Harness default still selects BitRouter; select another default in that home patch before removing this connection"
            );
        }
        if owned.contains(&path) {
            for row in &mut document.rows {
                if row.get("id").and_then(Value::as_str) == Some("llm-pi-ai")
                    && let Some(provider) = row
                        .pointer_mut("/config/providers/bitrouter")
                        .and_then(Value::as_object_mut)
                {
                    provider.remove("apiKeyEnv");
                }
            }
        }
        if document.rows.iter().any(references) {
            return Ok(Some(serde_saphyr::to_string(&current)?));
        }
    }
    refs.remove(CREDENTIAL);
    if refs.is_empty() && !prior.contains_key("refs") {
        current.remove("refs");
    } else {
        current.insert("refs".into(), Value::Object(refs));
    }
    if prior.is_empty() && current.len() == 1 && current.get("version") == Some(&json!(1)) {
        current.clear();
    }
    if current == prior {
        Ok(before.map(str::to_owned))
    } else {
        Ok(Some(serde_saphyr::to_string(&current)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_rows_keep_unrelated_javascript_and_unicode_verbatim() -> Result<()> {
        let original = "# header\n- id: unrelated\n  config:\n    text: 你好\n    dynamic: !!js process.cwd()\n";
        let mut patch = PatchDocument::parse(Some(original))?;
        patch.set(
            "llm-pi-ai",
            Some(json!({"id":"llm-pi-ai","config":{"providers":{}}})),
        )?;
        let rendered = patch.render()?;
        assert!(rendered.starts_with(original));
        assert_eq!(PatchDocument::parse(Some(&rendered))?.rows.len(), 2);
        let empty = "# `!!js` expressions allowed.\n[]\n";
        let mut patch = PatchDocument::parse(Some(empty))?;
        patch.set(
            "credentials",
            Some(json!({"id":"credentials","disabled":false})),
        )?;
        assert!(
            patch
                .render()?
                .starts_with("# `!!js` expressions allowed.\n")
        );
        Ok(())
    }

    #[test]
    fn model_selection_restore_retains_new_settings() -> Result<()> {
        let before = "[]\n";
        let after = "- id: llm-pi-ai\n  config:\n    providers:\n      bitrouter: {apiKeyEnv: BITROUTER_CONNECT_API_KEY}\n- id: credentials\n  disabled: false\n";
        let current = format!(
            "{after}- id: agent-default-model\n  config: {{provider: bitrouter, model: 'bitrouter/automatos'}}\n- id: ui\n  config: {{theme: dark}}\n"
        );
        let restored =
            restore_profile(Some(before), after, Some(&current))?.context("restored patch")?;
        let restored = PatchDocument::parse(Some(&restored))?;
        assert!(route(&restored)?.is_none());
        assert!(restored.row("agent-default-model")?.is_none());
        assert_eq!(
            restored
                .row("ui")?
                .and_then(|row| row.pointer("/config/theme")),
            Some(&json!("dark"))
        );
        Ok(())
    }

    #[test]
    fn later_home_default_prevents_provider_and_credential_removal() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let home = temporary.path().canonicalize()?;
        std::fs::create_dir(home.join("profiles"))?;
        std::fs::write(
            home.join(PATCH),
            "- id: agent-default-model\n  config: {provider: bitrouter, model: bitrouter/automatos}\n",
        )?;
        let after = "version: 1\nrefs:\n  BITROUTER_CONNECT_API_KEY: fixture\n";
        assert!(restore_credentials(None, after, Some(after), &home, &[]).is_err());
        Ok(())
    }

    #[test]
    fn messages_sdk_uses_gateway_root_and_keeps_reverse_proxy_prefix() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let home = temporary.path().canonicalize()?;
        let profile = home.join("profiles/web");
        std::fs::create_dir_all(&profile)?;
        std::fs::write(profile.join("package.json"),json!({"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-base","@deepseek-ai/dsh-web-app"]}}}).to_string())?;
        let prepared = prepare(
            "http://127.0.0.1:4356/team",
            "fixture",
            "bitrouter/automatos",
            "bitrouter/automatos",
            Some(&home),
            &["web".into()],
            Protocol::Messages,
        )?;
        files::Receipt::new(
            "http://127.0.0.1:4356/team".into(),
            "bitrouter/automatos".into(),
            prepared.files,
        )
        .apply(&home.join("receipt.json"))?;
        let patch = PatchDocument::parse(files::read(&profile.join(PATCH))?.as_deref())?;
        assert_eq!(
            route(&patch)?.and_then(|value| value.get("baseURL")),
            Some(&json!("http://127.0.0.1:4356/team"))
        );
        Ok(())
    }
}
