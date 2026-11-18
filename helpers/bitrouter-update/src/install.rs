//! Read the cargo-dist guarded install receipt and coordinate with its installers.

use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Product, channel::Candidate};

pub(crate) fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(digest.finalize()));
        }
        digest.update(&buf[..n]);
    }
}

fn receipt_path(product: Product) -> Result<PathBuf> {
    let path = |name| {
        env::var_os(name)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    };
    let config = path("XDG_CONFIG_HOME")
        .or_else(|| {
            if cfg!(windows) {
                path("LOCALAPPDATA")
            } else {
                env::home_dir().map(|home| home.join(".config"))
            }
        })
        .context("cannot locate cargo-dist receipt directory")?;
    ensure!(config.is_absolute(), "receipt directory must be absolute");
    let receipt = |base: &Path| {
        base.join(product.package())
            .join(format!("{}-receipt.json", product.package()))
    };
    let primary = receipt(&config);
    #[cfg(windows)]
    if let Some(local) = path("LOCALAPPDATA") {
        // PowerShell honors XDG_CONFIG_HOME. The Windows POSIX installer uses
        // LOCALAPPDATA first, even when both variables are present.
        let alternate = receipt(&local);
        let evidence = |receipt: &Path| -> Result<bool> {
            let lock = receipt.with_file_name(".install-lock");
            Ok(receipt.try_exists()? || (marked(&lock)? && lock.join("receipt").try_exists()?))
        };
        if alternate != primary && evidence(&alternate)? {
            // Compare directories too when the first install stopped before
            // publishing its receipt. The two environment paths can be aliases.
            ensure!(
                !evidence(&primary)?
                    || same_file::is_same_file(
                        primary.with_file_name("."),
                        alternate.with_file_name(".")
                    )?,
                "ambiguous cargo-dist receipts in XDG_CONFIG_HOME and LOCALAPPDATA"
            );
            return Ok(alternate);
        }
    }
    Ok(primary)
}

/// Snapshot of an owned installation. Rechecked under both installer locks.
pub struct Installation {
    product: Product,
    exe: PathBuf,
    receipt: PathBuf,
    record: Value,
}

impl Installation {
    /// Locate the current product's receipt and validate its executable binding.
    ///
    /// # Errors
    /// A present but invalid receipt refuses the update, including forced tags.
    pub fn load(product: Product, exe: &Path, version: &str) -> Result<Option<Self>> {
        Self::load_at(product, exe, version, receipt_path(product)?)
    }

    fn load_at(
        product: Product,
        exe: &Path,
        version: &str,
        receipt: PathBuf,
    ) -> Result<Option<Self>> {
        let exe = exe.canonicalize()?;
        let receipt_lock = receipt.with_file_name(".install-lock");
        let journal = receipt_lock.join("receipt");
        let stored = read_record(&receipt)?;
        let pending = if marked(&receipt_lock)? {
            read_record(&journal)?
        } else {
            None
        };
        let mut matching = None;
        let digest = if stored.is_some() || pending.is_some() {
            sha256(&exe)?
        } else {
            return Ok(None);
        };
        for record in [stored, pending].into_iter().flatten() {
            let installation = Self {
                product,
                exe: exe.clone(),
                receipt: receipt.clone(),
                record,
            };
            installation.verify_binding()?;
            if installation.record["version"] == version
                && installation.record["binary_sha256"] == digest
            {
                matching = Some(installation);
            }
        }
        Ok(Some(matching.context("installed executable digest or version mismatch; preserve the executable and receipts for manual rollback")?))
    }

    fn verify_binding(&self) -> Result<()> {
        let r = &self.record;
        let name = self.product.binary();
        ensure!(
            r["source"]["owner"] == "bitrouter"
                && r["source"]["name"] == "bitrouter"
                && r["source"]["app_name"] == self.product.package()
                && r["source"]["release_type"] == "github"
                && r["provider"]["source"] == "cargo-dist"
                && r["binaries"] == json!([name])
                && r["binary_aliases"] == json!({})
                && r["version"].is_string()
                && r["binary_sha256"].as_str().is_some_and(|h| h.len() == 64
                    && h.bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())),
            "receipt product, executable, aliases or version mismatch; preserve any M0 installation for migration"
        );
        let prefix = Path::new(
            r["install_prefix"]
                .as_str()
                .context("receipt lacks install_prefix")?,
        );
        ensure!(prefix.is_absolute(), "receipt prefix must be absolute");
        let dir = match r["install_layout"].as_str() {
            Some("cargo-home") => prefix.join("bin"),
            Some("flat") => prefix.to_owned(),
            _ => bail!("unsupported receipt layout"),
        }
        .canonicalize()
        .context("resolve receipt prefix")?;
        let owned = dir.join(&name);
        ensure!(
            owned == self.exe && fs::symlink_metadata(&owned)?.is_file(),
            "receipt prefix or executable name mismatch"
        );
        let peer = match self.product {
            Product::Service => Product::Bro,
            Product::Bro => Product::Service,
        };
        let peer = dir.join(peer.binary());
        if peer.try_exists()? {
            ensure!(
                !same_file::is_same_file(&owned, peer)?,
                "linked product executables require M0 migration"
            );
        }
        Ok(())
    }

    fn locks(&self) -> Result<(Lock, Lock)> {
        let bin = self.exe.parent().context("missing executable directory")?;
        let bin_lock = Lock::acquire(
            bin.join(format!(".{}-install-lock", self.product.package())),
            false,
        )?;
        let receipt_lock = Lock::acquire(self.receipt.with_file_name(".install-lock"), true)?;
        ensure!(
            receipt_path(self.product)? == self.receipt,
            "installation receipt location changed during update"
        );
        Ok((bin_lock, receipt_lock))
    }

    fn reconcile(&self, lock: &Lock) -> Result<()> {
        let version = self.record["version"]
            .as_str()
            .context("missing receipt version")?;
        let now = Self::load_at(self.product, &self.exe, version, self.receipt.clone())?
            .context("receipt disappeared")?;
        ensure!(
            self.record == now.record,
            "installation changed during update"
        );
        #[cfg(windows)]
        {
            let backup = lock.path.join("previous.exe");
            if backup.try_exists()? {
                let prior = read_record(&self.receipt)?
                    .context("missing previous receipt for Windows recovery copy")?;
                ensure!(
                    fs::symlink_metadata(&backup)?.is_file()
                        && prior["binary_sha256"] == sha256(&backup)?,
                    "previous Windows executable conflicts with its receipt; preserve {}",
                    backup.display()
                );
                fs::remove_file(backup)?;
            }
        }
        let pending = lock.path.join("receipt");
        if let Some(record) = read_record(&pending)? {
            if record == now.record {
                fs::rename(pending, &self.receipt)?;
            } else {
                fs::remove_file(pending)?;
            }
        }
        Ok(())
    }

    /// Complete a previously verified receipt transaction when already current.
    ///
    /// # Errors
    /// An active, unrecognized or conflicting transaction retains its evidence.
    pub fn finish_interrupted(&self) -> Result<()> {
        let bin_lock = self
            .exe
            .with_file_name(format!(".{}-install-lock", self.product.package()));
        if self.receipt.with_file_name(".install-lock").try_exists()? || bin_lock.try_exists()? {
            let (_bin_lock, lock) = self.locks()?;
            self.reconcile(&lock)?;
        }
        Ok(())
    }

    /// Replace the verified product and update only its own receipt.
    ///
    /// # Errors
    /// Refuses a concurrently changed installation. A failed swap retains the
    /// staged receipt until a writer can verify which executable is in place.
    pub fn apply(self, candidate: Candidate) -> Result<()> {
        ensure!(
            candidate.product == self.product,
            "candidate belongs to a different product"
        );
        let (_bin_lock, lock) = self.locks()?;
        self.reconcile(&lock)?;
        let records = self.receipt.parent().context("missing receipt directory")?;
        let mut next = self.record;
        next["version"] = json!(candidate.version);
        next["binary_sha256"] = json!(sha256(&candidate.path())?);
        let mut staged = tempfile::NamedTempFile::new_in(records)?;
        serde_json::to_writer(staged.as_file_mut(), &next)?;
        staged.flush()?;
        staged.as_file().sync_all()?;
        let pending = lock.path.join("receipt");
        staged.persist(&pending)?;
        replace_executable(&candidate.path(), &self.exe, &lock.path)?;
        fs::rename(pending, self.receipt)?;
        Ok(())
    }
}

#[cfg(unix)]
fn replace_executable(candidate: &Path, exe: &Path, _lock: &Path) -> Result<()> {
    let staged =
        tempfile::NamedTempFile::new_in(exe.parent().context("missing executable directory")?)?;
    fs::copy(candidate, staged.path())?;
    fs::set_permissions(staged.path(), fs::metadata(exe)?.permissions())?;
    staged.as_file().sync_all()?;
    staged.persist(exe)?;
    Ok(())
}

#[cfg(windows)]
fn replace_executable(candidate: &Path, exe: &Path, lock: &Path) -> Result<()> {
    // self-replace must relocate the running image before installing its
    // successor. Keep a digest-verifiable copy at a known path through that gap.
    let backup = lock.join("previous.exe");
    let mut copy =
        tempfile::NamedTempFile::new_in(lock.parent().context("missing receipt directory")?)?;
    io::copy(&mut fs::File::open(exe)?, copy.as_file_mut())?;
    copy.as_file().sync_all()?;
    copy.persist(&backup)?;
    if let Err(error) = self_replace::self_replace(candidate) {
        if !exe.try_exists()? {
            let staged = tempfile::NamedTempFile::new_in(
                exe.parent().context("missing executable directory")?,
            )?;
            fs::copy(&backup, staged.path())?;
            staged
                .persist_noclobber(exe)
                .context("restore Windows executable from previous.exe")?;
        }
        return Err(error.into());
    }
    fs::remove_file(backup)?;
    Ok(())
}

fn read_record(path: &Path) -> Result<Option<Value>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => ensure!(
            meta.is_file(),
            "expected a regular receipt at {}",
            path.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    Ok(Some(
        serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("decode receipt at {}", path.display()))?,
    ))
}

fn directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_dir()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn marked(path: &Path) -> Result<bool> {
    Ok(directory(path)? && directory(&path.join("lease-v1"))?)
}

struct Lock {
    path: PathBuf,
    // Keep the stable lease inode and its lock across both validation and cleanup.
    _lease: fs::File,
}

impl Lock {
    fn acquire(path: PathBuf, allow_receipt: bool) -> Result<Self> {
        let lease_path = path.with_added_extension("lease");
        match fs::symlink_metadata(&lease_path) {
            Ok(meta) => ensure!(
                meta.is_file(),
                "invalid installer lease at {}",
                lease_path.display()
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lease_path)?;
        lease
            .try_lock()
            .with_context(|| format!("active installer at {}", path.display()))?;
        match fs::create_dir(&path) {
            Ok(()) => fs::create_dir(path.join("lease-v1"))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                ensure!(
                    marked(&path)?,
                    "unrecognized installer lock at {}. Stop product writers and preserve both lock directories, executable and receipts before manual recovery.",
                    path.display()
                );
                for entry in fs::read_dir(&path)? {
                    let entry = entry?;
                    ensure!(
                        entry.file_name() == "lease-v1"
                            || (allow_receipt
                                && (entry.file_name() == "receipt"
                                    || (cfg!(windows) && entry.file_name() == "previous.exe"))),
                        "unrecognized installer journal at {}",
                        entry.path().display()
                    );
                }
            }
            Err(e) => return Err(e.into()),
        }
        Ok(Self {
            path,
            _lease: lease,
        })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Keep the marker and journal together until the next writer reconciles.
        if matches!(fs::symlink_metadata(self.path.join("receipt")), Err(e) if e.kind() == io::ErrorKind::NotFound)
            && let Err(e) =
                fs::remove_dir(self.path.join("lease-v1")).and_then(|()| fs::remove_dir(&self.path))
        {
            eprintln!("Preserved installer lock at {}: {e}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_and_legacy_locks_remain_exclusive() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join(".install-lock");
        let held = Lock::acquire(path.clone(), true)?;
        assert!(Lock::acquire(path.clone(), true).is_err());
        drop(held);
        fs::create_dir(&path)?;
        assert!(Lock::acquire(path.clone(), true).is_err());
        assert!(path.is_dir());
        Ok(())
    }

    #[test]
    fn interrupted_receipt_follows_the_installed_bytes() -> Result<()> {
        for product in [Product::Service, Product::Bro] {
            let root = tempfile::tempdir()?;
            let exe = root.path().join(product.binary());
            let receipt = root.path().join("installed.json");
            fs::write(&exe, b"old")?;
            let old = json!({
                "source": {"owner":"bitrouter","name":"bitrouter","app_name":product.package(),"release_type":"github"},
                "provider":{"source":"cargo-dist"}, "binaries":[product.binary()], "binary_aliases":{},
                "version":"1", "binary_sha256":sha256(&exe)?, "install_layout":"flat","install_prefix":root.path()
            });
            fs::write(&exe, b"new")?;
            let mut next = old.clone();
            next["version"] = json!("2");
            next["binary_sha256"] = json!(sha256(&exe)?);
            let path = root.path().join(".install-lock");
            let journal = path.join("receipt");
            for (bytes, version, want) in [(b"old", "1", &old), (b"new", "2", &next)] {
                fs::write(&exe, bytes)?;
                fs::write(&receipt, serde_json::to_vec(&old)?)?;
                let held = Lock::acquire(path.clone(), true)?;
                fs::write(&journal, serde_json::to_vec(&next)?)?;
                drop(held);
                let snapshot = Installation::load_at(product, &exe, version, receipt.clone())?
                    .context("missing installation")?;
                assert_eq!(read_record(&receipt)?.as_ref(), Some(&old));
                let held = Lock::acquire(path.clone(), true)?;
                snapshot.reconcile(&held)?;
                drop(held);
                assert!(!path.exists());
                assert_eq!(read_record(&receipt)?.as_ref(), Some(want));
            }
            let held = Lock::acquire(path.clone(), true)?;
            fs::write(&journal, serde_json::to_vec(&old)?)?;
            fs::write(&exe, b"unknown")?;
            assert!(Installation::load_at(product, &exe, "2", receipt.clone()).is_err());
            drop(held);
            assert_eq!(read_record(&journal)?.as_ref(), Some(&old));
            assert_eq!(read_record(&receipt)?.as_ref(), Some(&next));
        }
        Ok(())
    }
}
