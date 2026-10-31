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
        if current == self.before.as_deref() {
            return Ok(self.before.clone());
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
        ensure!(!matches!(self.restore, Restore::Exact), "file was edited");
        let mut value = object(current)?;
        let after = object(Some(&self.after))?;
        let before = object(self.before.as_deref())?;
        match &self.restore {
            Restore::Exact | Restore::CodexConfig { .. } => anyhow::bail!("file was edited"),
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
        if current == Some(&self.after) {
            return Ok(true);
        }
        if current.is_none() || matches!(self.restore, Restore::Exact) {
            return Ok(false);
        }
        if self.restored(current, false).is_err() {
            return Ok(false);
        }
        if matches!(self.restore, Restore::CodexConfig { .. }) {
            return super::codex::config_installed(&self.after, current);
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
            Restore::Exact | Restore::CodexConfig { .. } => false,
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
    pub files: Vec<Change>,
}

impl Receipt {
    pub fn new(endpoint: String, model: String, files: Vec<Change>) -> Self {
        Self {
            version: 4,
            endpoint,
            model,
            display_name: None,
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

    pub fn load(path: &Path) -> Result<Option<Self>> {
        read(path)?
            .map(|text| {
                let mut receipt: Self =
                    serde_json::from_str(&text).context("invalid connection receipt")?;
                ensure!(
                    matches!(receipt.version, 1..=4),
                    "unsupported connection receipt version"
                );
                if receipt.version == 1 && receipt.model.is_empty() {
                    receipt.model = super::default_model();
                }
                super::validate_model(&receipt.model)?;
                super::validate_display_name(receipt.display_name())?;
                Ok(receipt)
            })
            .transpose()
    }

    pub fn installed(&self) -> Result<bool> {
        for file in &self.files {
            if !file.installed(read(&file.path)?.as_deref())? {
                return Ok(false);
            }
        }
        Ok(true)
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
        fs::remove_file(receipt_path)?;
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

fn check_path(path: &Path) -> Result<()> {
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
