//! Commit bytes through a retained file or an exclusive relative creation.
use super::children::{Parent, component, regular};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Seek, Write},
};

pub(crate) fn verify_destination(parent: &Parent, name: &OsStr, file: &File) -> io::Result<()> {
    regular(file)?;
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, statat};
        let held = rustix::fs::fstat(file)?;
        let named = statat(&parent.file, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if (held.st_dev, held.st_ino) != (named.st_dev, named.st_ino) {
            return Err(io::Error::other("file was replaced"));
        }
    }
    #[cfg(windows)]
    let _ = (parent, name);
    Ok(())
}

pub(crate) fn create_file(parent: &Parent, name: &OsStr) -> io::Result<File> {
    component(name)?;
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(File::from(openat(
            &parent.file,
            name,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?))
    }
    #[cfg(windows)]
    super::windows::open(&parent.file, name, super::children::Access::ReadWrite, true)
}

pub(crate) fn write_file(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    regular(file)?;
    file.rewind()?;
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()
}
