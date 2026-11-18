//! A saved grant identifies one directory, including across worker restarts.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub(crate) enum WorkspaceId {
    Unix {
        device: u64,
        inode: u64,
        created: SystemTime,
    },
    Windows {
        volume: u64,
        file_id: [u8; 16],
        created: SystemTime,
    },
}

impl WorkspaceId {
    pub fn read(path: &Path) -> Result<Self> {
        Self::from_handle(&open_directory(path)?)
    }

    fn from_handle(handle: &fs::File) -> Result<Self> {
        let metadata = handle.metadata()?;
        ensure!(metadata.is_dir(), "workspace grant requires a directory");
        // Directory numbers can be recycled after deletion. Require a birth
        // time from the same handle, rather than mutable mtime or ctime.
        let created = metadata
            .created()
            .context("workspace birth time unavailable")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(metadata.ino() != 0, "workspace inode unavailable");
            Ok(Self::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
                created,
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
            };
            let mut info = FILE_ID_INFO::default();
            // SAFETY: handle stays open and the output is a full FILE_ID_INFO.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    handle.as_raw_handle(),
                    FileIdInfo,
                    (&raw mut info).cast(),
                    size_of::<FILE_ID_INFO>() as u32,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("workspace file ID unavailable");
            }
            ensure!(
                info.FileId.Identifier != [0; 16],
                "workspace file ID unavailable"
            );
            Ok(Self::Windows {
                volume: info.VolumeSerialNumber,
                file_id: info.FileId.Identifier,
                created,
            })
        }
        #[cfg(not(any(unix, windows)))]
        anyhow::bail!("workspace directory identity unavailable on this platform")
    }
}

fn open_directory(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options.open(path)
}

/// Keep the directory object alive for the entire dispatch. Never turn a
/// handle-relative path back into the caller's mutable workspace pathname.
pub(crate) struct Directory {
    pub file: fs::File,
    pub path: PathBuf,
    #[cfg(windows)]
    _ancestors: Vec<fs::File>,
}

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        #[cfg(windows)]
        let (file, ancestors) = {
            use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            let mut handles = Vec::new();
            for dir in path
                .ancestors()
                .filter(|p| p.has_root())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                let handle = fs::OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(dir)?;
                ensure!(
                    handle.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                    "workspace path contains a reparse point"
                );
                handles.push(handle);
            }
            (
                handles.pop().context("workspace has no directory handle")?,
                handles,
            )
        };
        #[cfg(not(windows))]
        let file = open_directory(path)?;
        let path = object_path(&file, path)?;
        Ok(Self {
            file,
            path,
            #[cfg(windows)]
            _ancestors: ancestors,
        })
    }

    pub fn verify(&self, expected: &WorkspaceId) -> Result<()> {
        ensure!(
            &WorkspaceId::from_handle(&self.file)? == expected,
            "workspace directory changed since admission"
        );
        Ok(())
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn object_path(file: &fs::File, _name: &Path) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let path = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
    };
    #[cfg(target_os = "macos")]
    let path = {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        PathBuf::from(format!("/.vol/{}/{}", meta.dev(), meta.ino()))
    };
    #[cfg(windows)]
    let path = _name.to_path_buf();
    // procfs exposes an open descriptor as a magic symlink.
    #[cfg(target_os = "linux")]
    let alias = fs::File::open(&path)?;
    #[cfg(not(target_os = "linux"))]
    let alias = open_directory(&path)?;
    ensure!(
        WorkspaceId::from_handle(file)? == WorkspaceId::from_handle(&alias)?,
        "workspace handle path is unavailable"
    );
    Ok(path)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn object_path(_: &fs::File, _: &Path) -> Result<PathBuf> {
    anyhow::bail!("no handle-bound workspace access on this platform")
}

// Darwin realpath(3) walks components individually and cannot resolve the
// synthetic volume ID component in /.vol/<device>/<inode>.
pub(crate) fn physical_path(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::OpenOptionsExt;
        // A searchable directory can permit creation without enumeration.
        // Preserve those permissions until the publisher attempts its fsync.
        let access = if fs::metadata(path)?.is_dir() {
            libc::O_SEARCH
        } else {
            libc::O_EVTONLY | libc::O_NONBLOCK
        };
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(access)
            .open(path)?;
        let name = rustix::fs::getpath(&file)?;
        Ok(std::ffi::OsString::from_vec(name.into_bytes()).into())
    }
    #[cfg(not(target_os = "macos"))]
    path.canonicalize()
}
