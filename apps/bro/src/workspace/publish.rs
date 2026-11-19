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
        expected: Option<(&mut File, &[u8])>,
        effect: &mut EffectStatus,
    ) -> Result<()> {
        let stage = &mut self.0;
        component(name)?;
        regular(&stage.file)?;
        if let Some((file, bytes)) = expected {
            #[cfg(unix)]
            check_name(&stage.parent.file, name, file)?;
            check_contents(file, bytes)?;
            ensure!(
                !file.metadata()?.permissions().readonly(),
                "existing target is read-only"
            );
            stage.replace(name, file, bytes, effect)?;
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
        name: &OsStr,
        original: &mut File,
        expected: &[u8],
        effect: &mut EffectStatus,
    ) -> Result<()> {
        use super::windows::{delete, rename_here};
        let recovery = OsString::from(format!(".bro-recovery-{}", uuid::Uuid::new_v4()));
        original.sync_all()?;
        finish_rename(rename_here(original, &recovery), effect)?;
        let retained = || {
            format!(
                "publication needs recovery; original entry retained at {}",
                recovery.to_string_lossy()
            )
        };
        let publish = (|| -> Result<()> {
            original.sync_all()?;
            check_contents(original, expected)?;
            rename_here(&self.file, name)?;
            Ok(())
        })();
        if let Err(error) = publish {
            rename_here(original, name).with_context(retained)?;
            original
                .sync_all()
                .context("could not sync restored workspace file")?;
            return Err(error);
        }
        self.published = true;
        self.file.sync_all().with_context(retained)?;
        delete(original).with_context(retained)?;
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    fn replace(&mut self, _: &OsStr, _: &mut File, _: &[u8], _: &mut EffectStatus) -> Result<()> {
        anyhow::bail!("workspace publication is unavailable")
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
