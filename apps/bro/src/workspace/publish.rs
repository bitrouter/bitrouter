//! Stage file contents and publish through the retained workspace objects.
use super::children::{Parent, component, regular};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::harness::runtime::EffectStatus;
use std::{
    ffi::{OsStr, OsString},
    fs::{File, Permissions},
    io::{self, Read, Seek, Write},
    sync::Arc,
};

pub(crate) struct StagedFile {
    parent: Arc<Parent>,
    file: File,
    name: OsString,
    #[cfg(unix)]
    scratch: File,
    #[cfg(unix)]
    keep_recovery: bool,
    #[cfg(windows)]
    published: bool,
}

pub(crate) struct PreparedFile(StagedFile);

#[cfg(unix)]
const PAYLOAD: &str = "file";

impl StagedFile {
    pub fn new(parent: Arc<Parent>) -> Result<Self> {
        let name = OsString::from(format!(".bro-publish-{}", uuid::Uuid::new_v4()));
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags, mkdirat, openat};
            mkdirat(&parent.file, &name, Mode::from_raw_mode(0o700))?;
            let scratch = super::children::open_child(
                &parent.file,
                &name,
                super::children::Access::Directory,
            )?;
            let file = File::from(openat(
                &scratch,
                PAYLOAD,
                OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?);
            Ok(Self {
                parent,
                file,
                name,
                scratch,
                keep_recovery: false,
            })
        }
        #[cfg(windows)]
        {
            let file = super::windows::open(
                &parent.file,
                &name,
                super::children::Access::ReadWrite,
                true,
            )?;
            Ok(Self {
                parent,
                file,
                name,
                published: false,
            })
        }
    }

    pub fn prepare(
        mut self,
        bytes: &[u8],
        permissions: Option<Permissions>,
    ) -> Result<PreparedFile> {
        self.file.write_all(bytes)?;
        if let Some(permissions) = permissions {
            self.file.set_permissions(permissions)?;
        }
        self.file.sync_all()?;
        Ok(PreparedFile(self))
    }
}

impl PreparedFile {
    pub fn publish(
        mut self,
        name: &OsStr,
        expected: Option<(File, &[u8])>,
        effect: &mut EffectStatus,
    ) -> Result<()> {
        let stage = &mut self.0;
        component(name)?;
        regular(&stage.file)?;
        if let Some((mut file, bytes)) = expected {
            #[cfg(unix)]
            check_name(&stage.parent.file, name, &file)?;
            check_contents(&mut file, bytes)?;
            ensure!(
                !file.metadata()?.permissions().readonly(),
                "existing target is read-only"
            );
            #[cfg(not(windows))]
            stage.replace(name, &mut file, bytes, effect)?;
            #[cfg(windows)]
            stage.replace(file, bytes, effect)?;
        } else {
            stage.install_new(name, effect)?;
        }
        stage.parent.sync_ancestors()?;
        Ok(())
    }
}

impl StagedFile {
    fn install_new(&mut self, name: &OsStr, effect: &mut EffectStatus) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let result = {
            use rustix::fs::{RenameFlags, renameat_with};
            check_name(&self.scratch, OsStr::new(PAYLOAD), &self.file)?;
            renameat_with(
                &self.scratch,
                PAYLOAD,
                &self.parent.file,
                name,
                RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from)
        };
        #[cfg(windows)]
        let result = super::windows::rename_here(&self.file, name);
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        let result = Err(io::Error::other("workspace publication is unavailable"));
        finish_rename(result, effect)?;
        #[cfg(unix)]
        self.scratch.sync_all()?;
        #[cfg(windows)]
        {
            self.published = true;
            self.file.sync_all()?;
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn replace(
        &mut self,
        name: &OsStr,
        file: &mut File,
        expected: &[u8],
        effect: &mut EffectStatus,
    ) -> Result<()> {
        use rustix::fs::{AtFlags, RenameFlags, renameat_with, unlinkat};
        self.scratch.sync_all()?;
        self.parent.sync()?;
        let payload = OsStr::new(PAYLOAD);
        check_name(&self.scratch, payload, &self.file)?;
        check_name(&self.parent.file, name, file)?;
        let result = renameat_with(
            &self.scratch,
            payload,
            &self.parent.file,
            name,
            RenameFlags::EXCHANGE,
        );
        finish_rename(result.map_err(io::Error::from), effect)?;
        self.keep_recovery = true;
        (|| -> Result<()> {
            self.scratch.sync_all()?;
            self.parent.sync()?;
            check_name(&self.parent.file, name, &self.file)?;
            check_name(&self.scratch, payload, file)?;
            check_contents(file, expected)?;
            unlinkat(&self.scratch, payload, AtFlags::empty())?;
            Ok(())
        })()
        .with_context(|| {
            format!(
                "publication needs recovery; displaced entry retained at {}/{}",
                self.name.to_string_lossy(),
                PAYLOAD
            )
        })?;
        self.keep_recovery = false;
        Ok(())
    }

    #[cfg(windows)]
    fn replace(
        &mut self,
        mut original: File,
        expected: &[u8],
        effect: &mut EffectStatus,
    ) -> Result<()> {
        use super::windows::{delete, file_name, rename_here};
        let name = file_name(&original)?;
        original.sync_all()?;
        let recovery = WindowsRecovery::persist(self, &name, expected)?;
        let recovery_location = || recovery.location();
        recovery
            .displace(&original, effect)
            .with_context(recovery_location)?;
        let publish = (|| -> Result<()> {
            check_contents(&mut original, expected)?;
            rename_here(&self.file, &name)?;
            Ok(())
        })();
        if let Err(error) = publish {
            rename_here(&original, &name).with_context(recovery_location)?;
            original.sync_all().with_context(recovery_location)?;
            recovery.remove()?;
            return Err(error);
        }
        self.published = true;
        self.file.sync_all().with_context(recovery_location)?;
        delete(&original).with_context(recovery_location)?;
        drop(original);
        recovery.remove()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    fn replace(&mut self, _: &OsStr, _: &mut File, _: &[u8], _: &mut EffectStatus) -> Result<()> {
        anyhow::bail!("workspace publication is unavailable")
    }
}

#[cfg(windows)]
const MAX_RECOVERY_RECORD_BYTES: usize = 8 * 1024;

#[cfg(windows)]
struct WindowsRecovery {
    record: PreparedFile,
    record_name: String,
    escrow_name: String,
}

#[cfg(windows)]
impl WindowsRecovery {
    fn persist(stage: &StagedFile, name: &OsStr, original: &[u8]) -> Result<Self> {
        use sha2::{Digest, Sha256};
        use std::os::windows::ffi::OsStrExt;

        let prefix = format!(".bro-recovery-{}", uuid::Uuid::new_v4());
        let record_name = format!("{prefix}.json");
        let escrow_name = format!("{prefix}.original");
        let name_utf16: Vec<u16> = name.encode_wide().collect();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "original_name": name.to_str(),
            "original_name_utf16": name_utf16,
            "escrow_name": escrow_name,
            "staged_name": stage.name.to_str().context("missing staging name")?,
            "original_sha256": hex::encode(Sha256::digest(original)),
        }))?;
        ensure!(
            bytes.len() <= MAX_RECOVERY_RECORD_BYTES,
            "Windows workspace recovery record is too large"
        );
        let mut record = StagedFile::new(stage.parent.clone())?.prepare(&bytes, None)?;
        super::windows::rename_here(&record.0.file, OsStr::new(&record_name))?;
        record.0.published = true;
        let recovery = Self {
            record,
            record_name,
            escrow_name,
        };
        recovery
            .record
            .0
            .file
            .sync_all()
            .with_context(|| recovery.location())?;
        Ok(recovery)
    }

    fn displace(&self, original: &File, effect: &mut EffectStatus) -> Result<()> {
        finish_rename(
            super::windows::rename_here(original, OsStr::new(&self.escrow_name)),
            effect,
        )?;
        original.sync_all()?;
        Ok(())
    }

    fn location(&self) -> String {
        format!(
            "workspace publication needs recovery; inspect {} beside the requested file (escrow: {})",
            self.record_name, self.escrow_name
        )
    }

    fn remove(self) -> Result<()> {
        super::windows::delete(&self.record.0.file).with_context(|| self.location())
    }
}

fn finish_rename(result: io::Result<()>, effect: &mut EffectStatus) -> io::Result<()> {
    if !matches!(&result, Err(error) if error.kind() == io::ErrorKind::AlreadyExists) {
        *effect = EffectStatus::Unknown;
    }
    result
}

fn check_contents(file: &mut File, expected: &[u8]) -> Result<()> {
    regular(file)?;
    file.rewind()?;
    let mut bytes = Vec::new();
    file.take(expected.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes == expected, "file changed while preparing the edit");
    Ok(())
}

#[cfg(unix)]
fn check_name(parent: &File, name: &OsStr, file: &File) -> io::Result<()> {
    use rustix::fs::{AtFlags, fstat, statat};
    let held = fstat(file)?;
    let named = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if (held.st_dev, held.st_ino) != (named.st_dev, named.st_ino) {
        return Err(io::Error::other("workspace entry was replaced"));
    }
    Ok(())
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        #[cfg(unix)]
        if !self.keep_recovery {
            use rustix::fs::{AtFlags, unlinkat};
            if check_name(&self.scratch, OsStr::new(PAYLOAD), &self.file).is_ok()
                && let Err(error) = unlinkat(&self.scratch, PAYLOAD, AtFlags::empty())
            {
                eprintln!("bro: could not remove staged workspace file: {error}");
            }
            if check_name(&self.parent.file, &self.name, &self.scratch).is_ok()
                && let Err(error) = unlinkat(&self.parent.file, &self.name, AtFlags::REMOVEDIR)
            {
                eprintln!("bro: could not remove workspace staging directory: {error}");
            }
        }
        #[cfg(windows)]
        if !self.published
            && let Err(error) = super::windows::delete(&self.file)
        {
            eprintln!(
                "bro: could not remove staged workspace file {}: {error}",
                self.name.to_string_lossy()
            );
        }
    }
}

#[cfg(all(test, windows))]
#[test]
fn interrupted_replacement_leaves_a_locatable_original() -> Result<()> {
    use super::{Directory, children::Access, windows::file_name};
    use sha2::{Digest, Sha256};
    use std::{fs, os::windows::ffi::OsStrExt};

    let project = tempfile::tempdir()?;
    let target = project.path().join("Café.txt");
    let original_bytes = b"original project notes";
    fs::write(&target, original_bytes)?;
    let root = Arc::new(Directory::open(&project.path().canonicalize()?)?);
    let parent = Parent::root(&root)?;
    let original = parent.open(OsStr::new("café.TXT"), Access::ReadWrite)?;
    let original_name = file_name(&original)?;
    let stage = StagedFile::new(parent)?.prepare(b"prepared edit", None)?;
    let recovery = WindowsRecovery::persist(&stage.0, &original_name, original_bytes)?;
    let mut effect = EffectStatus::NotExecuted;
    recovery.displace(&original, &mut effect)?;
    assert_eq!(effect, EffectStatus::Unknown);

    drop(recovery);
    drop(original);
    drop(stage);
    assert!(!target.try_exists()?);
    let record = fs::read_dir(project.path())?
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .context("recovery record missing after closing the publisher")?;
    let data = fs::read(record.path())?;
    assert!(data.len() <= MAX_RECOVERY_RECORD_BYTES);
    let data: serde_json::Value = serde_json::from_slice(&data)?;
    assert_eq!(data["original_name"], "Café.txt");
    let name = target.file_name().context("test target missing filename")?;
    assert_eq!(
        data["original_name_utf16"],
        serde_json::json!(name.encode_wide().collect::<Vec<_>>())
    );
    let escrow = data["escrow_name"]
        .as_str()
        .context("missing escrow name")?;
    let staged = data["staged_name"]
        .as_str()
        .context("missing staged name")?;
    assert!(!project.path().join(staged).try_exists()?);
    let saved = fs::read(project.path().join(escrow))?;
    assert_eq!(saved, original_bytes);
    assert_eq!(data["original_sha256"], hex::encode(Sha256::digest(&saved)));
    Ok(())
}
