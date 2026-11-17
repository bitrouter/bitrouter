//! Cross-process serialization and atomic, synced profiles outside projects.
use super::{KeyStorage, Profile};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(super) struct Store {
    pub path: PathBuf,
    _lock: File,
}
#[cfg(unix)]
fn private(path: &Path, directory: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("reading Agent storage metadata")?;
    if metadata.file_type().is_symlink() || metadata.is_dir() != directory {
        bail!("Agent storage must use ordinary private files/directories");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("Agent storage permissions must be owner-only");
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn private(_path: &Path, _directory: bool) -> Result<()> {
    bail!(
        "Agent profile storage requires qualified owner-only file permissions on this platform; no credentials were saved"
    )
}
fn create_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
pub(super) fn default_root() -> Result<PathBuf> {
    let account = super::super::account::credentials::default_credentials_path()?;
    Ok(account
        .parent()
        .context("missing account data directory")?
        .join("agent-connections"))
}
pub(super) fn prepare(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        bail!("Agent storage directory must be absolute");
    }
    if !root.exists() {
        let parent = root.parent().context("missing Agent storage parent")?;
        fs::create_dir_all(parent).context("creating Agent storage parent")?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).context("creating private Agent directory"),
        }
    }
    private(root, true)?;
    let root = fs::canonicalize(root)?;
    if root.starts_with(std::env::current_dir()?)
        || root.ancestors().any(|path| path.join(".git").exists())
    {
        bail!(
            "Agent credentials must be stored outside the project; choose an external XDG_DATA_HOME"
        );
    }
    Ok(root)
}
impl Store {
    pub fn lock(root: &Path, id: &str) -> Result<Self> {
        if id.is_empty()
            || id.len() > 100
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            bail!("invalid Agent profile ID");
        }
        let root = prepare(root)?;
        let path = root.join(format!("{id}.json"));
        let lock_path = root.join(format!("{id}.lock"));
        let lock = match create_options().open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                private(&lock_path, false)?;
                OpenOptions::new().write(true).open(&lock_path)?
            }
            Err(error) => return Err(error).context("opening Agent profile lock"),
        };
        lock.try_lock()
            .context("Agent profile is busy; retry after the other command finishes")?;
        // The held OS lock excludes writers for this profile. Remove only its
        // owner-only temporary files left by interrupted atomic replacements.
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(rest) = name.strip_prefix(&format!("{id}.")) else {
                continue;
            };
            if let Some(uuid) = rest.strip_suffix(".tmp")
                && uuid::Uuid::parse_str(uuid).is_ok()
            {
                private(&entry.path(), false)?;
                fs::remove_file(entry.path())?;
            }
        }
        Ok(Self { path, _lock: lock })
    }
    pub fn load(&self) -> Result<Option<Profile>> {
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("reading Agent profile metadata"),
        };
        private(&self.path, false)?;
        let mut bytes = Vec::new();
        File::open(&self.path)?
            .take(1_048_577)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1_048_576 {
            bail!("Agent profile exceeds storage bound");
        }
        let profile: Profile = serde_json::from_slice(&bytes)
            .context("invalid Agent profile; no credentials were sent")?;
        if profile.format != 1 {
            bail!("unsupported Agent profile format; update bro");
        }
        if self.path.file_stem().and_then(|name| name.to_str()) != Some(profile.id.as_str()) {
            bail!("Agent profile ID does not match its storage slot");
        }
        Ok(Some(profile))
    }
    pub fn save(&self, profile: &Profile) -> Result<()> {
        let temp = self
            .path
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let outcome = (|| {
            let mut file = create_options().open(&temp)?;
            file.write_all(&serde_json::to_vec(profile)?)?;
            file.sync_all()?;
            fs::rename(&temp, &self.path)?;
            sync_parent(&self.path)
        })();
        if temp.exists() {
            let _ = fs::remove_file(&temp);
        }
        outcome.context("persisting Agent profile; credentials were not confirmed")
    }
}
fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        // First use can create several containing directories. Flush every
        // link, including after a prior process died during their creation.
        // A failed flush must refuse activation just like a failed file write.
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("missing storage parent"))?;
        for directory in parent.ancestors() {
            File::open(directory)?.sync_all()?;
        }
    }
    Ok(())
}
pub(super) fn create_key(backend: KeyStorage) -> Result<super::KeyRef> {
    let secret = super::crypto::generate()?;
    match backend {
        KeyStorage::File => Ok(super::KeyRef::File { pkcs8: secret }),
        KeyStorage::Keychain => {
            let account = format!("connection-{}", uuid::Uuid::new_v4());
            let entry=keyring::Entry::new("bitrouter-agent-v1",&account).map_err(|_|anyhow::anyhow!("OS key store unavailable; explicitly select --key-storage file for private file storage"))?;
            entry.set_password(&secret).map_err(|_| {
                anyhow::anyhow!(
                    "OS key store write failed; explicitly select --key-storage file if needed"
                )
            })?;
            if entry
                .get_password()
                .map_err(|_| anyhow::anyhow!("OS key store read-back failed"))?
                != secret
            {
                bail!("OS key store read-back mismatch");
            }
            Ok(super::KeyRef::Keychain { account })
        }
    }
}
pub(super) fn read_key(key: &super::KeyRef) -> Result<String> {
    match key {
        super::KeyRef::File { pkcs8 } => Ok(pkcs8.clone()),
        super::KeyRef::Keychain { account } => keyring::Entry::new("bitrouter-agent-v1", account)
            .and_then(|entry| entry.get_password())
            .map_err(|_| anyhow::anyhow!("Agent signing key unavailable in OS key store")),
    }
}
pub(super) fn remove_key(key: &super::KeyRef) -> Result<()> {
    if let super::KeyRef::Keychain { account } = key {
        match keyring::Entry::new("bitrouter-agent-v1", account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(_) => bail!("Agent was revoked but OS key store cleanup failed; retry disconnect"),
        }
    }
    Ok(())
}
