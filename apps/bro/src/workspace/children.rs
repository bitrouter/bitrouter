//! Child lookup relative to a retained directory. Validate the open object
//! before exposing its contents or using it as the next lookup root.
use std::{
    ffi::OsStr,
    fs::File,
    io,
    path::{Component, Path},
    sync::Arc,
};

use super::Directory;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Directory,
    Read,
    ReadWrite,
}

pub(super) fn component(name: &OsStr) -> io::Result<()> {
    if !matches!(Path::new(name).components().next(), Some(Component::Normal(n)) if n == name) {
        return Err(io::Error::other("expected one ordinary child name"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if name.encode_wide().any(|c| c == 0 || c == u16::from(b':')) {
            return Err(io::Error::other("invalid Windows workspace child name"));
        }
    }
    Ok(())
}

#[cfg(unix)]
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
    #[cfg(unix)]
    let links = std::os::unix::fs::MetadataExt::nlink(&metadata);
    #[cfg(windows)]
    let links = {
        use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, GetFileInformationByHandle,
        };
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected workspace reparse point",
            ));
        }
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the borrowed handle stays open and info is the full output structure.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        info.nNumberOfLinks
    };
    if !metadata.is_file() || links != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a singly linked regular workspace file",
        ));
    }
    Ok(())
}

/// Retain the directory and its ancestor chain throughout the operation.
pub(crate) struct Parent {
    pub file: File,
    previous: Option<Arc<Parent>>,
    #[cfg(windows)]
    _grant: Option<Arc<Directory>>,
}

impl Parent {
    pub fn root(root: &Arc<Directory>) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            file: root.file.try_clone()?,
            previous: None,
            #[cfg(windows)]
            _grant: Some(root.clone()),
        }))
    }

    pub fn child(self: &Arc<Self>, name: &OsStr) -> io::Result<Arc<Self>> {
        component(name)?;
        #[cfg(unix)]
        let file = match open_child(&self.file, name, Access::Directory) {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                use rustix::fs::{Mode, OFlags, openat};
                #[cfg(target_os = "linux")]
                let access = OFlags::PATH;
                #[cfg(target_os = "macos")]
                let access = OFlags::from_bits_retain(libc::O_SEARCH as _);
                File::from(openat(
                    &self.file,
                    name,
                    access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?)
            }
            result => result?,
        };
        #[cfg(windows)]
        let file = super::windows::open(&self.file, name, Access::Directory, false)?;
        Ok(self.with_child(file))
    }

    fn with_child(self: &Arc<Self>, file: File) -> Arc<Self> {
        Arc::new(Self {
            file,
            previous: Some(self.clone()),
            #[cfg(windows)]
            _grant: None,
        })
    }

    pub fn create(self: &Arc<Self>, name: &OsStr) -> io::Result<Arc<Self>> {
        component(name)?;
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, mkdirat};
            mkdirat(&self.file, name, Mode::from_raw_mode(0o777))?;
            self.sync()?;
            self.child(name)
        }
        #[cfg(windows)]
        {
            let file = super::windows::open(&self.file, name, Access::Directory, true)?;
            Ok(self.with_child(file))
        }
    }

    pub fn open(&self, name: &OsStr, access: Access) -> io::Result<File> {
        #[cfg(unix)]
        return open_child(&self.file, name, access);
        #[cfg(windows)]
        return super::windows::open(&self.file, name, access, false);
    }

    pub fn ancestors(&self) -> Vec<&Arc<Parent>> {
        let mut dirs = Vec::new();
        let mut prior = self.previous.as_ref();
        while let Some(dir) = prior {
            dirs.push(dir);
            prior = dir.previous.as_ref();
        }
        dirs.reverse();
        dirs
    }

    pub fn entries(&self) -> io::Result<Entries> {
        #[cfg(unix)]
        {
            Ok(Entries(rustix::fs::Dir::new(self.reopen()?)?))
        }
        #[cfg(windows)]
        {
            super::windows::Entries::new(&self.file)
        }
    }

    #[cfg(unix)]
    fn reopen(&self) -> io::Result<File> {
        use rustix::fs::{Mode, OFlags, openat};
        Ok(File::from(openat(
            &self.file,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?))
    }

    #[cfg(unix)]
    fn sync(&self) -> io::Result<()> {
        self.reopen()?.sync_all()
    }

    pub fn sync_ancestors(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let mut current = Some(self);
            while let Some(dir) = current {
                dir.sync()?;
                current = dir.previous.as_deref();
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Directory,
    File,
    Link,
    Special,
}

#[cfg(unix)]
pub(crate) struct Entries(rustix::fs::Dir);
#[cfg(windows)]
pub(crate) type Entries = super::windows::Entries;

#[cfg(unix)]
impl Iterator for Entries {
    type Item = io::Result<(std::ffi::OsString, Kind)>;
    fn next(&mut self) -> Option<Self::Item> {
        use rustix::fs::{AtFlags, FileType, statat};
        use std::os::unix::ffi::OsStrExt;

        loop {
            let entry = match self.0.next()? {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error.into())),
            };
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let ty = match entry.file_type() {
                FileType::Unknown => match self
                    .0
                    .fd()
                    .and_then(|fd| statat(fd, name, AtFlags::SYMLINK_NOFOLLOW))
                {
                    Ok(stat) => FileType::from_raw_mode(stat.st_mode),
                    Err(rustix::io::Errno::NOENT) => continue,
                    Err(error) => return Some(Err(error.into())),
                },
                ty => ty,
            };
            let kind = match ty {
                FileType::Directory => Kind::Directory,
                FileType::RegularFile => Kind::File,
                FileType::Symlink => Kind::Link,
                _ => Kind::Special,
            };
            return Some(Ok((OsStr::from_bytes(name.to_bytes()).to_owned(), kind)));
        }
    }
}
