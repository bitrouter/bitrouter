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

enum Selection<'a> {
    Any,
    Directory(&'a Path),
    Executable(&'a Path),
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
        let install = Self::inspect_at(product, Selection::Executable(&exe), receipt)?;
        if let Some(install) = &install {
            ensure!(
                install.record["version"] == version,
                "installed executable version mismatch; preserve the executable and receipts for manual rollback"
            );
        }
        Ok(install)
    }

    /// Discover the product at the location in its guarded receipt.
    ///
    /// If supplied, `directory` selects the bin directory. A receipt for a
    /// different directory is outside the selection.
    ///
    /// # Errors
    /// Refuses malformed receipts or a selected executable with mismatched bytes.
    pub fn discover(product: Product, directory: Option<&Path>) -> Result<Option<Self>> {
        let canonical;
        let selection = if let Some(directory) = directory {
            canonical = match directory.canonicalize() {
                Ok(path) => path,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            Selection::Directory(&canonical)
        } else {
            Selection::Any
        };
        Self::inspect_at(product, selection, receipt_path(product)?)
    }

    fn inspect_at(
        product: Product,
        selection: Selection<'_>,
        receipt: PathBuf,
    ) -> Result<Option<Self>> {
        let stored = read_record(&receipt)?;
        let lock = receipt.with_file_name(".install-lock");
        let pending = if marked(&lock)? {
            read_record(&lock.join("receipt"))?
        } else {
            None
        };
        let Some(record) = stored.as_ref().or(pending.as_ref()) else {
            return Ok(None);
        };
        let binding = receipt_directory(product, record)?;
        let directory = match binding.canonicalize() {
            Ok(path) => path,
            Err(error)
                if matches!(selection, Selection::Directory(_))
                    && error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error).context("resolve receipt prefix"),
        };
        let owned = directory.join(product.binary());
        match selection {
            Selection::Directory(selected) if selected != directory => return Ok(None),
            Selection::Executable(exe) => {
                ensure!(exe == owned, "receipt prefix or executable name mismatch")
            }
            _ => {}
        }
        if let (Some(_), Some(pending)) = (&stored, &pending) {
            ensure!(
                receipt_directory(product, pending)?
                    .canonicalize()
                    .context("resolve interrupted receipt prefix")?
                    == directory,
                "installed and interrupted receipt prefixes bind different executables"
            );
        }
        if !regular_file(&owned)? {
            #[cfg(windows)]
            if let Some(backup) = verified_previous(&lock, stored.as_ref(), pending.as_ref())? {
                let record =
                    stored.context("missing previous receipt for Windows recovery copy")?;
                verify_independent(product, &owned, &backup)?;
                verify_missing_peer(product, &owned)?;
                return Ok(Some(Self {
                    product,
                    exe: owned,
                    receipt,
                    record,
                }));
            }
            ensure!(
                stored.is_none(),
                "receipted executable is missing at {}; preserve the receipts for manual recovery",
                owned.display()
            );
            return Ok(None);
        }
        verify_independent(product, &owned, &owned)?;
        let digest = sha256(&owned)?;
        let mut matching: Option<Value> = None;
        for record in [stored, pending].into_iter().flatten() {
            if record["binary_sha256"] == digest {
                ensure!(
                    matching
                        .as_ref()
                        .is_none_or(|prior| prior["version"] == record["version"]),
                    "conflicting receipt versions for the same executable; preserve the receipts for manual recovery"
                );
                matching = Some(record);
            }
        }
        let record = matching.context("installed executable digest mismatch; preserve the executable and receipts for manual rollback")?;
        Ok(Some(Self {
            product,
            exe: owned,
            receipt,
            record,
        }))
    }

    /// The canonical native executable bound to the receipt.
    pub fn executable(&self) -> &Path {
        &self.exe
    }

    /// Version recorded for the verified executable.
    pub fn version(&self) -> &str {
        self.record["version"].as_str().unwrap_or_default()
    }

    fn locks(&self) -> Result<(Lock, Lock)> {
        lock_installation(self.product, &self.exe, &self.receipt)
    }

    fn reconcile(&self, lock: &Lock) -> Result<()> {
        let now = Self::inspect_at(
            self.product,
            Selection::Executable(&self.exe),
            self.receipt.clone(),
        )?
        .context("receipt disappeared")?;
        ensure!(
            self.record == now.record,
            "installation changed during update"
        );
        let pending = lock.path.join("receipt");
        let journal = read_record(&pending)?;
        #[cfg(windows)]
        if let Some(backup) = verified_previous(
            &lock.path,
            read_record(&self.receipt)?.as_ref(),
            journal.as_ref(),
        )? {
            if !regular_file(&self.exe)? {
                let digest = self.record["binary_sha256"]
                    .as_str()
                    .context("missing recovery digest")?;
                restore_previous(&backup, &self.exe, digest)?;
            }
            fs::remove_file(backup)?;
        }
        if let Some(record) = journal {
            if record == now.record {
                fs::rename(pending, &self.receipt)?;
            } else {
                fs::remove_file(pending)?;
            }
        }
        Ok(())
    }

    /// Publish a verified candidate into an explicit flat bin directory.
    ///
    /// Returns true on first install and false if the exact candidate is
    /// already installed. A subsequent upgrade belongs to `apply`.
    ///
    /// # Errors
    /// Refuses to adopt or overwrite an existing executable or foreign receipt.
    pub fn create(product: Product, directory: &Path, candidate: Candidate) -> Result<bool> {
        ensure!(
            candidate.product == product,
            "candidate belongs to a different product"
        );
        ensure!(directory.is_absolute(), "install prefix must be absolute");
        let prefix = directory
            .to_str()
            .context("install prefix must be valid Unicode")?;
        let receipt = receipt_path(product)?;
        fs::create_dir_all(directory)?;
        let exe = directory.canonicalize()?.join(product.binary());
        let method = crate::install_method(&exe, None);
        ensure!(
            !matches!(method, "homebrew" | "npm"),
            "install prefix belongs to {method}"
        );
        fs::create_dir_all(receipt.parent().context("missing receipt directory")?)?;
        let (_bin_lock, lock) = lock_installation(product, &exe, &receipt)?;
        let digest = sha256(&candidate.path())?;
        if let Some(install) =
            Self::inspect_at(product, Selection::Executable(&exe), receipt.clone())?
        {
            install.reconcile(&lock)?;
            ensure!(
                install.version() == candidate.version && install.record["binary_sha256"] == digest,
                "{} is already installed at a different release",
                product.command()
            );
            return Ok(false);
        }
        ensure!(
            !regular_file(&exe)?,
            "refusing to overwrite an unowned executable at {}",
            exe.display()
        );
        #[cfg(windows)]
        ensure!(
            !regular_file(&lock.path.join("previous.exe"))?,
            "found Windows recovery evidence; preserve the receipts and previous executable for manual recovery"
        );
        verify_missing_peer(product, &exe)?;
        let record = json!({
            "source": {"owner":"bitrouter","name":"bitrouter","app_name":product.package(),"release_type":"github"},
            "provider": {"source":"cargo-dist"},
            "binaries": [product.binary()],
            "binary_aliases": {},
            "install_prefix": prefix,
            "install_layout": "flat",
            "version": candidate.version,
            "binary_sha256": digest,
        });
        let staged = stage_executable(&candidate.path(), &exe)?;
        staged.as_file().sync_all()?;
        let pending = stage_receipt(&lock, &record)?;
        staged.persist_noclobber(&exe)?;
        fs::rename(pending, receipt)?;
        Ok(true)
    }

    /// Complete a previously verified receipt transaction, restoring a missing
    /// Windows image from a digest-verified recovery copy when available.
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
        let previous_digest = self.record["binary_sha256"]
            .as_str()
            .context("missing installed executable digest")?
            .to_owned();
        let mut next = self.record;
        next["version"] = json!(candidate.version);
        next["binary_sha256"] = json!(sha256(&candidate.path())?);
        let pending = stage_receipt(&lock, &next)?;
        replace_executable(&candidate.path(), &self.exe, &lock.path, &previous_digest)?;
        fs::rename(pending, self.receipt)?;
        Ok(())
    }
}

fn receipt_directory(product: Product, r: &Value) -> Result<PathBuf> {
    let name = product.binary();
    ensure!(
        r["source"]["owner"] == "bitrouter"
            && r["source"]["name"] == "bitrouter"
            && r["source"]["app_name"] == product.package()
            && r["source"]["release_type"] == "github"
            && r["provider"]["source"] == "cargo-dist"
            && r["binaries"] == json!([name])
            && r["binary_aliases"] == json!({})
            && r["version"].as_str().is_some_and(|v| !v.is_empty())
            && r["binary_sha256"].as_str().is_some_and(|h| h.len() == 64
                && h.bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())),
        "receipt product, executable, aliases or version mismatch; preserve the executable and receipts for manual recovery"
    );
    let prefix = Path::new(
        r["install_prefix"]
            .as_str()
            .context("missing receipt prefix")?,
    );
    ensure!(prefix.is_absolute(), "receipt prefix must be absolute");
    Ok(match r["install_layout"].as_str() {
        Some("cargo-home") => prefix.join("bin"),
        Some("flat") => prefix.to_owned(),
        _ => bail!("unsupported receipt layout"),
    })
}

fn peer_product(product: Product) -> Product {
    match product {
        Product::Bro => Product::Service,
        Product::Service => Product::Bro,
    }
}

fn verify_independent(product: Product, exe: &Path, image: &Path) -> Result<()> {
    let method = crate::install_method(exe, None);
    ensure!(
        !matches!(method, "homebrew" | "npm"),
        "receipted executable belongs to {method}"
    );
    let peer = exe.with_file_name(peer_product(product).binary());
    if peer.try_exists()? {
        ensure!(
            !same_file::is_same_file(image, peer)?,
            "linked product executables cannot be updated independently; preserve both executables and receipts for manual recovery"
        );
    }
    Ok(())
}

fn verify_missing_peer(product: Product, exe: &Path) -> Result<()> {
    let peer = exe.with_file_name(peer_product(product).binary());
    match peer.symlink_metadata() {
        Ok(meta) if meta.is_symlink() => ensure!(
            peer.canonicalize().is_ok_and(|target| target != exe),
            "linked product executables cannot be installed independently"
        ),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn lock_installation(product: Product, exe: &Path, receipt: &Path) -> Result<(Lock, Lock)> {
    let bin = exe.parent().context("missing executable directory")?;
    let bin_lock = Lock::acquire(
        bin.join(format!(".{}-install-lock", product.package())),
        false,
    )?;
    let receipt_lock = Lock::acquire(receipt.with_file_name(".install-lock"), true)?;
    ensure!(
        receipt_path(product)? == receipt,
        "installation receipt location changed during update"
    );
    Ok((bin_lock, receipt_lock))
}

fn stage_receipt(lock: &Lock, record: &Value) -> Result<PathBuf> {
    let records = lock.path.parent().context("missing receipt directory")?;
    let mut staged = tempfile::NamedTempFile::new_in(records)?;
    serde_json::to_writer(staged.as_file_mut(), record)?;
    staged.flush()?;
    staged.as_file().sync_all()?;
    let pending = lock.path.join("receipt");
    staged.persist(&pending)?;
    Ok(pending)
}

fn stage_executable(candidate: &Path, exe: &Path) -> Result<tempfile::NamedTempFile> {
    let mut staged =
        tempfile::NamedTempFile::new_in(exe.parent().context("missing executable directory")?)?;
    io::copy(&mut fs::File::open(candidate)?, staged.as_file_mut())?;
    fs::set_permissions(staged.path(), fs::metadata(candidate)?.permissions())?;
    Ok(staged)
}

fn replace_on_disk(candidate: &Path, exe: &Path) -> Result<()> {
    let staged = stage_executable(candidate, exe)?;
    #[cfg(unix)]
    fs::set_permissions(staged.path(), fs::metadata(exe)?.permissions())?;
    staged.as_file().sync_all()?;
    staged.persist(exe)?;
    Ok(())
}

#[cfg(unix)]
fn replace_executable(
    candidate: &Path,
    exe: &Path,
    _lock: &Path,
    _previous_digest: &str,
) -> Result<()> {
    replace_on_disk(candidate, exe)
}

#[cfg(windows)]
fn verified_previous(
    lock: &Path,
    prior: Option<&Value>,
    pending: Option<&Value>,
) -> Result<Option<PathBuf>> {
    let backup = lock.join("previous.exe");
    if !regular_file(&backup)? {
        return Ok(None);
    }
    let pending = pending.context("missing pending Windows receipt; preserve previous.exe")?;
    let prior = prior.context("missing previous receipt for Windows recovery copy")?;
    ensure!(
        prior["binary_sha256"] == sha256(&backup)?,
        "previous Windows executable conflicts with its receipt; preserve {}",
        backup.display()
    );
    ensure!(
        prior["binary_sha256"] != pending["binary_sha256"]
            || prior["version"] == pending["version"],
        "conflicting receipt versions for the same executable; preserve the receipts for manual recovery"
    );
    Ok(Some(backup))
}

#[cfg(windows)]
fn restore_previous(backup: &Path, exe: &Path, digest: &str) -> Result<()> {
    let staged = stage_executable(backup, exe)?;
    ensure!(
        sha256(staged.path())? == digest,
        "Windows recovery copy changed while staging"
    );
    staged.as_file().sync_all()?;
    staged
        .persist_noclobber(exe)
        .context("restore Windows executable from previous.exe")?;
    Ok(())
}

#[cfg(windows)]
fn replace_executable(
    candidate: &Path,
    exe: &Path,
    lock: &Path,
    previous_digest: &str,
) -> Result<()> {
    if env::current_exe()?.canonicalize()? != exe {
        return replace_on_disk(candidate, exe).with_context(|| {
            format!(
                "replace {}; stop the local gateway if it is running and retry",
                exe.display()
            )
        });
    }
    // self-replace must relocate the running image before installing its
    // successor. Keep a digest-verifiable copy at a known path through that gap.
    let backup = lock.join("previous.exe");
    let mut copy =
        tempfile::NamedTempFile::new_in(lock.parent().context("missing receipt directory")?)?;
    io::copy(&mut fs::File::open(exe)?, copy.as_file_mut())?;
    ensure!(
        sha256(copy.path())? == previous_digest,
        "Windows executable changed while creating the recovery copy"
    );
    copy.as_file().sync_all()?;
    copy.persist_noclobber(&backup)?;
    if let Err(error) = self_replace::self_replace(candidate) {
        if !regular_file(exe)? {
            ensure!(regular_file(&backup)?, "missing Windows recovery copy");
            restore_previous(&backup, exe, previous_digest)?;
        }
        return Err(error.into());
    }
    fs::remove_file(backup)?;
    Ok(())
}

fn read_record(path: &Path) -> Result<Option<Value>> {
    if !regular_file(path)? {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("decode receipt at {}", path.display()))?,
    ))
}

fn regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "expected a regular file at {}",
                path.display()
            );
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                ensure!(
                    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                    "expected a regular file at {}",
                    path.display()
                );
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    Ok(true)
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
        #[cfg(windows)]
        if !matches!(fs::symlink_metadata(self.path.join("previous.exe")), Err(e) if e.kind() == io::ErrorKind::NotFound)
        {
            return;
        }
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
    fn active_and_unmarked_locks_remain_exclusive() -> Result<()> {
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

    #[test]
    fn discovers_a_guarded_cargo_home_installation_from_its_receipt() -> Result<()> {
        let root = tempfile::tempdir()?;
        let dir = root.path().join("cargo").join("bin");
        fs::create_dir_all(&dir)?;
        let product = Product::Service;
        let exe = dir.join(product.binary());
        let receipt = root.path().join("installed.json");
        fs::write(&exe, b"guarded service")?;
        let record = json!({
            "source": {"owner":"bitrouter","name":"bitrouter","app_name":product.package(),"release_type":"github"},
            "provider": {"source":"cargo-dist"},
            "binaries": [product.binary()],
            "binary_aliases": {},
            "version": "1.0.0",
            "binary_sha256": sha256(&exe)?,
            "install_layout": "cargo-home",
            "install_prefix": dir.parent(),
        });
        fs::write(&receipt, serde_json::to_vec(&record)?)?;
        let selected = dir.canonicalize()?;
        let install =
            Installation::inspect_at(product, Selection::Directory(&selected), receipt.clone())?
                .context("missing guarded service")?;
        assert_eq!(install.executable(), exe.canonicalize()?);
        assert_eq!(install.version(), "1.0.0");
        fs::write(&exe, b"unrelated service")?;
        let other = root.path().canonicalize()?;
        assert!(
            Installation::inspect_at(product, Selection::Directory(&other), receipt.clone())?
                .is_none()
        );
        assert!(
            Installation::inspect_at(product, Selection::Directory(&selected), receipt.clone())
                .is_err()
        );
        assert!(Installation::inspect_at(product, Selection::Any, receipt).is_err());
        Ok(())
    }
}
