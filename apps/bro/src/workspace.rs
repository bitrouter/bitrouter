//! A saved grant identifies one directory, including across worker restarts.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, time::SystemTime};

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
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
            options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        let handle = options.open(path).context("opening workspace grant")?;
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
