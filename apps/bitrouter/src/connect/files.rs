//! Reversible, private configuration transactions. Receipts precede mutations.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum Restore {
    #[default]
    Exact,
    CodexConfig {
        model: String,
    },
    OpenCodeProvider,
    PiModels,
    PiAuth,
    PiSettings {
        model: String,
        #[serde(default)]
        models: Vec<String>,
    },
    DeepseekProfile,
    DeepseekCredentials {
        instance: PathBuf,
        profiles: Vec<PathBuf>,
    },
    HermesConfig {
        model: String,
    },
    OpenClawConfig {
        instance: PathBuf,
        catalogs: Vec<PathBuf>,
    },
    OpenClawModels,
    OpenClawCredential,
    ClaudeProfile,
    ClaudeMeta {
        profile: String,
    },
    ClaudeMode,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Change {
    pub path: PathBuf,
    before: Option<String>,
    after: String,
    #[serde(default)]
    restore: Restore,
}

impl Change {
    #[cfg(test)]
    pub fn new(path: PathBuf, after: String) -> Result<Self> {
        let before = read(&path)?;
        Ok(Self {
            path,
            before,
            after,
            restore: Restore::Exact,
        })
    }

    pub fn snapshot(path: PathBuf, before: Option<String>, after: String) -> Self {
        Self {
            path,
            before,
            after,
            restore: Restore::Exact,
        }
    }

    pub fn json(path: PathBuf, before: Option<String>, value: &serde_json::Value) -> Result<Self> {
        Ok(Self::snapshot(
            path,
            before,
            format!("{}\n", serde_json::to_string_pretty(value)?),
        ))
    }

    pub fn with_restore(mut self, restore: Restore) -> Self {
        self.restore = restore;
        self
    }

    fn switched_profile(&self, current: Option<&str>) -> Result<bool> {
        if let Restore::ClaudeMeta { profile } = &self.restore {
            let current = object(current)?;
            let before = object(self.before.as_deref())?;
            return Ok(current.get("appliedId").and_then(serde_json::Value::as_str)
                != Some(profile)
                && current.get("appliedId") != before.get("appliedId"));
        }
        Ok(false)
    }

    fn restored(&self, current: Option<&str>, preserve_mode: bool) -> Result<Option<String>> {
        // OpenClaw can materialize new agent catalogs after setup. Check for
        // credential copies even when the source config's bytes are unchanged.
        if let Restore::OpenClawConfig { instance, catalogs } = &self.restore {
            return super::openclaw::restore_config(
                self.before.as_deref(),
                &self.after,
                current,
                instance,
                catalogs,
            );
        }
        if current == self.before.as_deref() {
            return Ok(self.before.clone());
        }
        // A later profile can refer to an unchanged shared credential file.
        // Reference ownership must be checked even when its bytes match setup.
        if let Restore::DeepseekCredentials { instance, profiles } = &self.restore {
            return super::deepseek::restore_credentials(
                self.before.as_deref(),
                &self.after,
                current,
                instance,
                profiles,
            );
        }
        if matches!(self.restore, Restore::ClaudeMode) && preserve_mode {
            return Ok(current.map(str::to_owned));
        }
        if current == Some(&self.after) {
            return Ok(self.before.clone());
        }
        if let Restore::CodexConfig { model } = &self.restore {
            return super::codex::restore_config(
                self.before.as_deref(),
                &self.after,
                current,
                model,
            );
        }
        let before = self.before.as_deref();
        match &self.restore {
            Restore::OpenCodeProvider => {
                return super::opencode::restore_config(before, &self.after, current);
            }
            Restore::PiModels => return super::pi::restore_models(before, &self.after, current),
            Restore::PiAuth => return super::pi::restore_auth(before, &self.after, current),
            Restore::PiSettings { model, models } => {
                let models = if models.is_empty() {
                    std::slice::from_ref(model)
                } else {
                    models
                };
                return super::pi::restore_settings(before, &self.after, current, models);
            }
            Restore::DeepseekProfile => {
                return super::deepseek::restore_profile(before, &self.after, current);
            }
            Restore::HermesConfig { model } => {
                return super::hermes::restore_config(before, &self.after, current, model);
            }
            Restore::OpenClawModels => {
                return super::openclaw::restore_models(before, &self.after, current);
            }
            Restore::DeepseekCredentials { instance, profiles } => {
                return super::deepseek::restore_credentials(
                    before,
                    &self.after,
                    current,
                    instance,
                    profiles,
                );
            }
            _ => {}
        }
        ensure!(
            !matches!(self.restore, Restore::Exact | Restore::OpenClawCredential),
            "file was edited"
        );
        let mut value = object(current)?;
        let after = object(Some(&self.after))?;
        let before = object(self.before.as_deref())?;
        match &self.restore {
            Restore::Exact
            | Restore::OpenClawCredential
            | Restore::CodexConfig { .. }
            | Restore::OpenCodeProvider
            | Restore::PiModels
            | Restore::PiAuth
            | Restore::PiSettings { .. }
            | Restore::HermesConfig { .. }
            | Restore::OpenClawConfig { .. }
            | Restore::OpenClawModels
            | Restore::DeepseekProfile
            | Restore::DeepseekCredentials { .. } => anyhow::bail!("file was edited"),
            Restore::ClaudeProfile => {
                // Desktop seals this owned secret on first launch. No other
                // profile edits are implicitly discarded, including new keys.
                if value
                    .get("inferenceGatewayApiKey")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|key| key.starts_with("__sealed__:"))
                    && let Some(key) = after.get("inferenceGatewayApiKey")
                {
                    value.insert("inferenceGatewayApiKey".into(), key.clone());
                }
                ensure!(value == after, "BitRouter profile was edited");
                return Ok(self.before.clone());
            }
            Restore::ClaudeMode => {
                // A later explicit mode choice belongs to the user.
                if value.get("deploymentMode") == after.get("deploymentMode") {
                    restore_key(&mut value, &before, "deploymentMode");
                }
            }
            Restore::ClaudeMeta { profile } => {
                let entries = value
                    .get_mut("entries")
                    .and_then(serde_json::Value::as_array_mut)
                    .context("invalid Desktop profile entries")?;
                let expected = after
                    .get("entries")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|entries| entries.iter().find(|entry| entry["id"] == *profile))
                    .context("missing owned profile entry")?;
                let owned: Vec<_> = entries
                    .iter()
                    .filter(|entry| entry["id"] == *profile)
                    .collect();
                ensure!(
                    owned.len() <= 1 && owned.iter().all(|entry| *entry == expected),
                    "BitRouter profile entry was edited"
                );
                entries.retain(|entry| entry["id"] != *profile);
                if entries.is_empty() && !before.contains_key("entries") {
                    value.remove("entries");
                }
                if value.get("appliedId").and_then(serde_json::Value::as_str) == Some(profile) {
                    restore_key(&mut value, &before, "appliedId");
                }
            }
        }
        if value == before {
            Ok(self.before.clone())
        } else {
            Ok(Some(format!("{}\n", serde_json::to_string_pretty(&value)?)))
        }
    }

    fn installed(&self, current: Option<&str>) -> Result<bool> {
        if let Restore::OpenClawConfig { instance, catalogs } = &self.restore {
            return super::openclaw::config_installed(&self.after, current, instance, catalogs);
        }
        if current == Some(&self.after) {
            return Ok(true);
        }
        if current.is_none() || matches!(self.restore, Restore::Exact | Restore::OpenClawCredential)
        {
            return Ok(false);
        }
        if self.restored(current, false).is_err() {
            return Ok(false);
        }
        if matches!(self.restore, Restore::CodexConfig { .. }) {
            return super::codex::config_installed(&self.after, current);
        }
        match &self.restore {
            Restore::OpenCodeProvider => {
                return super::opencode::config_installed(&self.after, current);
            }
            Restore::PiModels => return super::pi::models_installed(&self.after, current),
            Restore::PiAuth => return super::pi::auth_installed(&self.after, current),
            Restore::PiSettings { .. } => {
                return super::pi::settings_installed(&self.after, current);
            }
            Restore::DeepseekProfile => {
                return super::deepseek::profile_installed(&self.after, current);
            }
            Restore::HermesConfig { .. } => {
                return super::hermes::config_installed(&self.after, current);
            }
            Restore::OpenClawModels => {
                return super::openclaw::models_installed(&self.after, current);
            }
            Restore::DeepseekCredentials { .. } => {
                return super::deepseek::credentials_installed(&self.after, current);
            }
            _ => {}
        }
        let value = object(current)?;
        let after = object(Some(&self.after))?;
        Ok(match &self.restore {
            Restore::ClaudeMeta { profile } => {
                value.get("appliedId") == after.get("appliedId")
                    && value
                        .get("entries")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|entries| entries.iter().any(|entry| entry["id"] == *profile))
            }
            Restore::ClaudeMode => value.get("deploymentMode") == after.get("deploymentMode"),
            Restore::ClaudeProfile => true,
            Restore::Exact
            | Restore::OpenClawCredential
            | Restore::CodexConfig { .. }
            | Restore::OpenCodeProvider
            | Restore::PiModels
            | Restore::PiAuth
            | Restore::PiSettings { .. }
            | Restore::HermesConfig { .. }
            | Restore::OpenClawConfig { .. }
            | Restore::OpenClawModels
            | Restore::DeepseekProfile
            | Restore::DeepseekCredentials { .. } => false,
        })
    }
}

fn object(text: Option<&str>) -> Result<serde_json::Map<String, serde_json::Value>> {
    match text {
        Some(text) => serde_json::from_str(text).context("expected a JSON object"),
        None => Ok(serde_json::Map::new()),
    }
}

fn restore_key(
    value: &mut serde_json::Map<String, serde_json::Value>,
    before: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) {
    match before.get(key) {
        Some(original) => {
            value.insert(key.into(), original.clone());
        }
        None => {
            value.remove(key);
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Receipt {
    version: u32,
    pub endpoint: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    pub native: Option<Native>,
    pub files: Vec<Change>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Native {
    pub instance: String,
    pub protocol: super::Protocol,
    pub profiles: Vec<String>,
    pub surfaces: Vec<String>,
    pub notes: Vec<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Replacement {
    previous: Receipt,
    rollback: Receipt,
    #[serde(default)]
    active_restored: bool,
}

impl Receipt {
    pub fn new(endpoint: String, model: String, files: Vec<Change>) -> Self {
        Self {
            version: 7,
            endpoint,
            model,
            display_name: None,
            native: None,
            files,
        }
    }

    pub fn with_display_name(mut self, name: Option<String>) -> Self {
        self.display_name = name;
        self
    }

    pub fn display_name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.model)
    }

    pub fn models(&self) -> &[String] {
        if let Some(native) = &self.native
            && !native.models.is_empty()
        {
            &native.models
        } else if self.model.is_empty() {
            &[]
        } else {
            std::slice::from_ref(&self.model)
        }
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        read(path)?
            .map(|text| {
                let mut receipt: Self =
                    serde_json::from_str(&text).context("invalid connection receipt")?;
                receipt.validate()?;
                Ok(receipt)
            })
            .transpose()
    }

    fn validate(&mut self) -> Result<()> {
        ensure!(
            matches!(self.version, 1..=7),
            "unsupported connection receipt version"
        );
        if self.version == 1 && self.model.is_empty() {
            self.model = super::default_model();
        }
        super::validate_model(&self.model)?;
        if self.version >= 7
            && let Some(native) = &self.native
        {
            ensure!(
                !native.models.is_empty(),
                "missing native connection models"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for model in self.models() {
            super::validate_model(model)?;
            ensure!(seen.insert(model), "duplicate model in connection receipt");
        }
        ensure!(
            self.models().first() == Some(&self.model),
            "connection receipt primary model differs from its models"
        );
        super::validate_display_name(self.display_name())?;
        Ok(())
    }

    pub fn installed(&self) -> Result<bool> {
        for file in &self.files {
            if !file.installed(read(&file.path)?.as_deref())? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn stored_token(&self) -> Result<String> {
        for file in &self.files {
            match &file.restore {
                Restore::OpenCodeProvider => return super::opencode::stored_token(&file.after),
                Restore::PiAuth => return super::pi::stored_token(&file.after),
                Restore::DeepseekCredentials { .. } => {
                    return super::deepseek::stored_token(&file.after);
                }
                Restore::HermesConfig { .. } => return super::hermes::stored_token(&file.after),
                Restore::OpenClawCredential => {
                    return super::openclaw::stored_token(&file.after);
                }
                _ => {}
            }
        }
        anyhow::bail!("saved native credential was not found")
    }

    pub fn apply(&self, receipt_path: &Path) -> Result<()> {
        // Check the complete plan before touching any client file.
        for file in &self.files {
            ensure!(
                read(&file.path)? == file.before,
                "configuration changed during preview: {}",
                file.path.display()
            );
        }
        ensure!(
            !receipt_path.exists(),
            "connection receipt already exists; remove the previous connection first"
        );
        write(receipt_path, &serde_json::to_string_pretty(self)?)?;
        for file in &self.files {
            let result = (|| {
                ensure!(
                    read(&file.path)? == file.before,
                    "configuration changed while connecting: {}",
                    file.path.display()
                );
                write(&file.path, &file.after)
            })();
            if let Err(error) = result {
                // Also works after interruption: restore accepts original or written bytes.
                return match self.restore(receipt_path) {
                    Ok(()) => Err(error.context("connection rolled back")),
                    Err(_) => Err(error.context("connection incomplete; receipt retained; use connect --remove after resolving changed files")),
                };
            }
        }
        Ok(())
    }

    pub fn restore(&self, receipt_path: &Path) -> Result<()> {
        self.restore_files()?;
        fs::remove_file(receipt_path)?;
        Ok(())
    }

    fn restore_files(&self) -> Result<()> {
        let current: Vec<_> = self
            .files
            .iter()
            .map(|file| read(&file.path))
            .collect::<Result<_>>()?;
        let mut preserve_mode = false;
        for (file, current) in self.files.iter().zip(&current) {
            preserve_mode |= file.switched_profile(current.as_deref())?;
        }
        // Validate every owned field before writing anything. Keep the exact
        // current bytes for the second race check, not the original snapshot.
        let restored: Vec<_> = self
            .files
            .iter()
            .zip(&current)
            .map(|(file, current)| {
                file.restored(current.as_deref(), preserve_mode)
                    .with_context(|| {
                        format!(
                            "refusing to overwrite changed settings: {}; receipt retained",
                            file.path.display()
                        )
                    })
            })
            .collect::<Result<_>>()?;
        // Undo activation pointers first, then provider/catalog files.
        for ((file, current), restored) in self.files.iter().zip(&current).zip(&restored).rev() {
            if current == restored {
                continue;
            }
            ensure!(
                read(&file.path)? == *current,
                "configuration changed during restore: {}",
                file.path.display()
            );
            match restored {
                Some(original) => write(&file.path, original)?,
                None => fs::remove_file(&file.path)?,
            }
        }
        Ok(())
    }

    /// Persist recovery information before temporarily removing owned settings.
    /// The builder sees the restored baseline and may fail without losing setup.
    pub fn replace(self, path: &Path, build: impl FnOnce() -> Result<Self>) -> Result<Self> {
        let journal = path.with_extension("update.json");
        ensure!(
            !journal.exists(),
            "pending connection update must be recovered first"
        );
        let mut rollback = Vec::new();
        for file in &self.files {
            let current = read(&file.path)?;
            let restored = file.restored(current.as_deref(), false)?;
            // Recovery accepts an absent restored file too.
            rollback.push(Change {
                path: file.path.clone(),
                before: current,
                after: restored.unwrap_or_default(),
                restore: Restore::Exact,
            });
        }
        let recovery = Replacement {
            rollback: Self::new(self.endpoint.clone(), self.model.clone(), rollback),
            previous: self,
            active_restored: false,
        };
        write(&journal, &serde_json::to_string_pretty(&recovery)?)?;
        let result: Result<Self> = (|| {
            recovery.previous.restore(path)?;
            let next = build()?;
            next.apply(path)?;
            fs::remove_file(&journal)?;
            Ok(next)
        })();
        match result {
            Ok(next) => Ok(next),
            Err(error) => match Self::recover_update(path) {
                Ok(()) => Err(error.context("update rolled back; previous connection retained")),
                Err(recovery) => {
                    Err(error.context(format!("update interrupted; recovery retained: {recovery}")))
                }
            },
        }
    }

    pub fn recover_update(path: &Path) -> Result<()> {
        let journal = path.with_extension("update.json");
        let Some(text) = read(&journal)? else {
            return Ok(());
        };
        let mut recovery: Replacement =
            serde_json::from_str(&text).context("invalid update recovery record")?;
        recovery.previous.validate()?;
        recovery.rollback.validate()?;
        if !recovery.active_restored {
            if let Some(active) = Self::load(path)? {
                active.restore_files()?;
            }
            // Once original installed bytes start returning, the newer receipt
            // must never interpret them as edits to its own provider.
            recovery.active_restored = true;
            write(&journal, &serde_json::to_string_pretty(&recovery)?)?;
        }
        // Validate the entire baseline before recovering original exact bytes.
        let mut validated = Vec::new();
        for file in &recovery.rollback.files {
            let current = read(&file.path)?;
            ensure!(
                current == file.before
                    || current.as_deref() == Some(&file.after)
                    || (current.is_none() && file.after.is_empty()),
                "configuration changed during update recovery: {}",
                file.path.display()
            );
            validated.push(current);
        }
        for (file, current) in recovery.rollback.files.iter().zip(validated) {
            ensure!(
                read(&file.path)? == current,
                "configuration changed during update recovery: {}",
                file.path.display()
            );
            match &file.before {
                Some(text) => write(&file.path, text)?,
                None if file.path.exists() => fs::remove_file(&file.path)?,
                None => {}
            }
        }
        write(path, &serde_json::to_string_pretty(&recovery.previous)?)?;
        fs::remove_file(journal)?;
        Ok(())
    }
}

pub(super) fn read(path: &Path) -> Result<Option<String>> {
    check_path(path)?;
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

pub(super) fn json(path: &Path) -> Result<serde_json::Value> {
    Ok(json_snapshot(path)?.0)
}

pub(super) fn json_snapshot(path: &Path) -> Result<(serde_json::Value, Option<String>)> {
    let before = read(path)?;
    let value = match &before {
        Some(text) => {
            let value: serde_json::Value = serde_json::from_str(text)
                .with_context(|| format!("invalid JSON in {}", path.display()))?;
            ensure!(
                value.is_object(),
                "expected an object in {}",
                path.display()
            );
            value
        }
        None => serde_json::json!({}),
    };
    Ok((value, before))
}

pub(super) fn check_path(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "connection path must be absolute");
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                ensure!(
                    !meta.file_type().is_symlink(),
                    "refusing symlinked configuration path: {}",
                    ancestor.display()
                );
                if ancestor == path {
                    ensure!(
                        meta.is_file() || meta.is_dir(),
                        "configuration path is not a regular file: {}",
                        path.display()
                    );
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if meta.is_file() {
                            ensure!(
                                meta.nlink() == 1,
                                "refusing multiply linked configuration: {}",
                                path.display()
                            );
                        }
                    }
                } else {
                    ensure!(meta.is_dir(), "configuration parent is not a directory");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn lock(path: &Path) -> Result<fs::File> {
    let parent = path.parent().context("lock has no parent")?;
    private_dir(parent)?;
    check_path(path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock()
        .context("another connect operation is running")?;
    Ok(file)
}

fn private_dir(path: &Path) -> Result<()> {
    check_path(path)?;
    ensure!(
        !path.exists() || path.is_dir(),
        "configuration parent is not a directory"
    );
    if !path.exists() {
        if let Some(parent) = path.parent() {
            private_dir(parent)?;
        }
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(path)?;
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<()> {
    check_path(path)?;
    let parent = path.parent().context("configuration has no parent")?;
    private_dir(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(text.as_bytes())?;
    temporary.as_file().sync_all()?;
    check_path(path)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_partial_transaction_and_refuses_later_edits() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let a = root.join("a.json");
        let b = root.join("b.json");
        let journal = root.join("receipt.json");
        fs::write(&a, "original")?;
        let receipt = Receipt::new(
            "http://localhost".into(),
            "team/test".into(),
            vec![
                Change::new(a.clone(), "connected".into())?,
                Change::new(b.clone(), "new".into())?,
            ],
        );
        receipt.apply(&journal)?;
        assert!(receipt.installed()?);
        fs::write(&a, "user edit")?;
        assert!(receipt.restore(&journal).is_err());
        assert_eq!(fs::read_to_string(&b)?, "new");
        fs::write(&a, "connected")?;
        receipt.restore(&journal)?;
        assert_eq!(fs::read_to_string(&a)?, "original");
        assert!(!b.exists());
        // Simulate interruption after the receipt and only the first write.
        write(&journal, &serde_json::to_string(&receipt)?)?;
        write(&a, "connected")?;
        receipt.restore(&journal)?;
        assert_eq!(fs::read_to_string(&a)?, "original");
        Ok(())
    }

    #[test]
    fn stale_preview_is_rejected_before_receipt_or_other_writes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let existing = root.join("settings.json");
        fs::write(&existing, "before")?;
        let receipt = Receipt::new(
            "http://localhost".into(),
            "team/test".into(),
            vec![
                Change::new(root.join("catalog.json"), "new catalog".into())?,
                Change::new(existing.clone(), "connected".into())?,
            ],
        );
        fs::write(&existing, "edited during preview")?;
        let journal = root.join("receipt.json");
        assert!(receipt.apply(&journal).is_err());
        assert!(!journal.exists());
        assert!(!root.join("catalog.json").exists());
        assert_eq!(fs::read_to_string(existing)?, "edited during preview");
        Ok(())
    }

    #[test]
    fn legacy_receipts_remain_restorable_and_new_receipts_keep_the_model() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let journal = root.join("receipt.json");
        let path = root.join("client.json");
        let receipt = Receipt::new(
            "http://localhost".into(),
            "team/test".into(),
            vec![Change::new(path.clone(), "connected".into())?],
        );
        receipt.apply(&journal)?;
        let loaded = Receipt::load(&journal)?.context("missing receipt")?;
        assert_eq!(loaded.model, "team/test");
        let mut legacy = serde_json::to_value(&receipt)?;
        legacy["version"] = serde_json::json!(1);
        legacy
            .as_object_mut()
            .context("invalid receipt")?
            .remove("model");
        write(&journal, &legacy.to_string())?;
        let loaded = Receipt::load(&journal)?.context("missing legacy receipt")?;
        assert_eq!(loaded.model, super::super::DEFAULT_MODEL);
        loaded.restore(&journal)?;
        assert!(!path.exists());
        legacy["version"] = serde_json::json!(2);
        write(&journal, &legacy.to_string())?;
        assert!(Receipt::load(&journal).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn private_receipts_and_symlink_refusal() -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let a = root.join("a");
        write(&a, "secret")?;
        assert_eq!(fs::metadata(&a)?.permissions().mode() & 0o777, 0o600);
        let link = root.join("link");
        symlink(&a, &link)?;
        assert!(read(&link).is_err());
        assert!(write(&link, "replacement").is_err());
        assert_eq!(fs::read_to_string(a)?, "secret");
        Ok(())
    }
}
