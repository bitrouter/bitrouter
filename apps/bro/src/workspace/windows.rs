//! Native relative opens and directory enumeration use the actual parent handle.
use super::children::{Access, Kind, component, regular};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    mem::offset_of,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
        io::{AsRawHandle, FromRawHandle},
    },
    ptr,
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
            FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
            FILE_WRITE_THROUGH, FileRenameInformation, NtCreateFile, NtSetInformationFile,
        },
    },
    Win32::{
        Foundation::{
            ERROR_NO_MORE_FILES, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
        },
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    },
};

pub(super) fn open(parent: &File, name: &OsStr, access: Access, create: bool) -> io::Result<File> {
    component(name)?;
    let mut wide: Vec<u16> = name.encode_wide().collect();
    let len = u16::try_from(wide.len() * 2).map_err(|_| io::Error::other("file name too long"))?;
    let unicode = UNICODE_STRING {
        Length: len,
        MaximumLength: len,
        Buffer: wide.as_mut_ptr(),
    };
    let attrs = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        ..Default::default()
    };
    let mut handle = ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    let rights = match access {
        Access::Directory | Access::Read => FILE_GENERIC_READ,
        Access::ReadWrite => FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
    };
    let options = if access == Access::Directory {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    };
    // SAFETY: both counted structures, the UTF-16 buffer and the directory
    // handle live through this synchronous call. FILE_CREATE is exclusive.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            rights,
            &attrs,
            &mut status,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            if create && access == Access::ReadWrite {
                0
            } else {
                FILE_SHARE_READ
            },
            if create { FILE_CREATE } else { FILE_OPEN },
            options
                | FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | if create || access == Access::ReadWrite {
                    FILE_WRITE_THROUGH
                } else {
                    0
                },
            ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(status_error(result));
    }
    // SAFETY: success transferred ownership of one file handle to this caller.
    let file = unsafe { File::from_raw_handle(handle) };
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected workspace reparse point",
        ));
    }
    if access != Access::Directory {
        regular(&file)?;
    }
    Ok(file)
}

fn status_error(status: i32) -> io::Error {
    // SAFETY: the conversion accepts any native status and uses no pointers.
    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

pub(super) fn rename_here(file: &File, name: &OsStr) -> io::Result<()> {
    component(name)?;
    let wide: Vec<u16> = name.encode_wide().collect();
    let offset = offset_of!(FILE_RENAME_INFORMATION, FileName);
    let name_len = u16::try_from(wide.len() * size_of::<u16>())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name too long"))?;
    let len = size_of::<FILE_RENAME_INFORMATION>() + usize::from(name_len);
    let mut buffer = vec![0usize; len.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: the zeroed header disables replacement and uses the source file's
    // directory. The aligned allocation covers the header and counted name.
    let result = unsafe {
        (*info).FileNameLength = name_len.into();
        ptr::copy_nonoverlapping(
            wide.as_ptr(),
            buffer.as_mut_ptr().cast::<u8>().add(offset).cast(),
            wide.len(),
        );
        NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            info.cast(),
            len as u32,
            FileRenameInformation,
        )
    };
    if result < 0 {
        return Err(status_error(result));
    }
    Ok(())
}

pub(super) fn delete(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the handle is live and the input is a complete disposition record.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&raw const info).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) type Entries = std::vec::IntoIter<(OsString, Kind)>;

pub(super) fn entries(dir: &File, cursor: &std::sync::Mutex<()>) -> io::Result<Entries> {
    let _cursor = cursor
        .lock()
        .map_err(|_| io::Error::other("workspace directory cursor unavailable"))?;
    let entries = DirectoryScan {
        file: dir,
        buffer: vec![0; 8192],
        offset: None,
        first: true,
        done: false,
    }
    .collect::<io::Result<Vec<_>>>()?;
    Ok(entries.into_iter())
}

struct DirectoryScan<'a> {
    file: &'a File,
    buffer: Vec<u64>,
    offset: Option<usize>,
    first: bool,
    done: bool,
}

impl DirectoryScan<'_> {
    fn entry(&mut self) -> io::Result<Option<(OsString, Kind)>> {
        let size = self.buffer.len() * 8;
        if self.offset.is_none() {
            let class = if self.first {
                FileFullDirectoryRestartInfo
            } else {
                FileFullDirectoryInfo
            };
            self.first = false;
            // SAFETY: buffer is writable, suitably aligned, and exactly size bytes.
            if unsafe {
                GetFileInformationByHandleEx(
                    self.file.as_raw_handle(),
                    class,
                    self.buffer.as_mut_ptr().cast(),
                    size as u32,
                )
            } == 0
            {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                    return Ok(None);
                }
                return Err(e);
            }
            self.offset = Some(0);
        }
        let offset = self.offset.take().ok_or(io::ErrorKind::InvalidData)?;
        if !offset.is_multiple_of(8) || offset > size - size_of::<FILE_FULL_DIR_INFO>() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // SAFETY: the kernel returned aligned directory records; check the
        // variable name length against this buffer before creating a slice.
        let record = unsafe {
            &*self
                .buffer
                .as_ptr()
                .cast::<u8>()
                .add(offset)
                .cast::<FILE_FULL_DIR_INFO>()
        };
        let name_len = record.FileNameLength as usize;
        let begin = offset + offset_of!(FILE_FULL_DIR_INFO, FileName);
        if !name_len.is_multiple_of(2) || name_len > size - begin {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if record.NextEntryOffset != 0 {
            let next = offset
                .checked_add(record.NextEntryOffset as usize)
                .ok_or(io::ErrorKind::InvalidData)?;
            if next < begin + name_len || next >= size {
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.offset = Some(next);
        }
        // SAFETY: begin and name_len are even and within the aligned allocation.
        let units = unsafe {
            std::slice::from_raw_parts(
                self.buffer.as_ptr().cast::<u8>().add(begin).cast::<u16>(),
                name_len / 2,
            )
        };
        let name = OsString::from_wide(units);
        let attrs = record.FileAttributes;
        let kind = if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            Kind::Link
        } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            Kind::Directory
        } else if attrs & FILE_ATTRIBUTE_DEVICE != 0 {
            Kind::Special
        } else {
            Kind::File
        };
        Ok(Some((name, kind)))
    }
}

impl Iterator for DirectoryScan<'_> {
    type Item = io::Result<(OsString, Kind)>;
    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            match self.entry() {
                Ok(Some((name, _))) if name == "." || name == ".." => continue,
                Ok(Some(e)) => return Some(Ok(e)),
                Ok(None) => self.done = true,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        None
    }
}
