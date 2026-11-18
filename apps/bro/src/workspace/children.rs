//! Open a single Unix child relative to a retained parent directory.
use std::{
    ffi::OsStr,
    fs::File,
    io,
    path::{Component, Path},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Directory,
    Read,
    ReadWrite,
}

fn component(name: &OsStr) -> io::Result<()> {
    if !matches!(Path::new(name).components().next(), Some(Component::Normal(n)) if n == name) {
        return Err(io::Error::other("expected one ordinary child name"));
    }
    Ok(())
}

pub(crate) fn open_child(parent: &File, name: &OsStr, access: Access) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, openat};
    component(name)?;
    let flags = match access {
        Access::Directory => OFlags::RDONLY | OFlags::DIRECTORY,
        Access::Read => OFlags::RDONLY,
        Access::ReadWrite => OFlags::RDWR,
    };
    let file = File::from(openat(
        parent,
        name,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )?);
    if access != Access::Directory {
        regular(&file)?;
    }
    Ok(file)
}

pub(crate) fn regular(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    let links = std::os::unix::fs::MetadataExt::nlink(&metadata);
    if !metadata.is_file() || links != 1 {
        return Err(io::Error::other(
            "expected a singly linked regular workspace file",
        ));
    }
    Ok(())
}
