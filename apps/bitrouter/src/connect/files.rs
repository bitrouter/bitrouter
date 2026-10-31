//! Reversible, private configuration transactions. Receipts precede mutations.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(super) struct Change {
    pub path: PathBuf,
    before: Option<String>,
    after: String,
}

impl Change {
    #[cfg(test)]
    pub fn new(path: PathBuf, after: String) -> Result<Self> {
        let before = read(&path)?;
        Ok(Self {
            path,
            before,
            after,
        })
    }

    pub fn snapshot(path: PathBuf, before: Option<String>, after: String) -> Self {
        Self {
            path,
            before,
            after,
        }
    }

    pub fn json(path: PathBuf, before: Option<String>, value: &serde_json::Value) -> Result<Self> {
        Ok(Self::snapshot(
            path,
            before,
            format!("{}\n", serde_json::to_string_pretty(value)?),
        ))
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Receipt {
    version: u32,
    pub endpoint: String,
    #[serde(default)]
    pub model: String,
    pub files: Vec<Change>,
}

impl Receipt {
    pub fn new(endpoint: String, model: String, files: Vec<Change>) -> Self {
        Self {
            version: 2,
            endpoint,
            model,
            files,
        }
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        read(path)?
            .map(|text| {
                let mut receipt: Self =
                    serde_json::from_str(&text).context("invalid connection receipt")?;
                ensure!(
                    matches!(receipt.version, 1 | 2),
                    "unsupported connection receipt version"
                );
                if receipt.version == 1 && receipt.model.is_empty() {
                    receipt.model = super::default_model();
                }
                super::validate_model(&receipt.model)?;
                Ok(receipt)
            })
            .transpose()
    }

    pub fn installed(&self) -> Result<bool> {
        for file in &self.files {
            if read(&file.path)?.as_deref() != Some(&file.after) {
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
        for file in &self.files {
            let current = read(&file.path)?;
            ensure!(
                current == file.before || current.as_deref() == Some(&file.after),
                "refusing to overwrite a changed file: {}; keep the receipt and reconcile this file before retrying --remove",
                file.path.display()
            );
        }
        // Undo activation pointers first, then provider/catalog files.
        for file in self.files.iter().rev() {
            let current = read(&file.path)?;
            if current == file.before {
                continue;
            }
            ensure!(
                current.as_deref() == Some(&file.after),
                "configuration changed during restore: {}",
                file.path.display()
            );
            match &file.before {
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
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
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
