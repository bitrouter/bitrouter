//! Validate the write set against the original directory and durably publish it.
use super::{Data, Entry, Id, MAX_FILE, Tree, id, read_file};
use crate::workspace::children::{Access, open_child};
use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{AtFlags, CWD, Dir, RenameFlags, renameat_with, statat, unlinkat};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr};
use std::fs::{self, File, Permissions};
use std::io::{Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, PermissionsExt},
};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// Resolve every ancestor from the open root, accepting only the directory
// identities captured on admission to the copy or created by this publication.
fn parent(root: &File, path: &Path, dirs: &BTreeMap<PathBuf, Id>) -> Result<File> {
    let mut dir = root.try_clone()?;
    let mut relative = PathBuf::new();
    for component in path.parent().context("missing parent")?.iter() {
        relative.push(component);
        dir = open_child(&dir, component, Access::Directory)?;
        ensure!(
            dirs.get(&relative) == Some(&id(&dir.metadata()?)?),
            "workspace parent changed: {}",
            relative.display()
        );
    }
    Ok(dir)
}

fn leaf(path: &Path) -> Result<&OsStr> {
    path.file_name().context("missing workspace filename")
}

fn verify(file: &mut File, expected: &Entry, identity: Id) -> Result<()> {
    let m = file.metadata()?;
    ensure!(
        id(&m)? == identity && m.mode() & 0o777 == expected.mode,
        "workspace entry identity or permissions changed"
    );
    if let Data::File(data) = &expected.data {
        ensure!(
            m.modified()? == expected.mtime && read_file(file, &m, MAX_FILE)? == *data,
            "workspace file changed during shell execution"
        );
    } else {
        ensure!(m.is_dir(), "workspace directory changed");
    }
    Ok(())
}

fn access(entry: &Entry) -> Access {
    if entry.data == Data::Directory {
        Access::Directory
    } else {
        Access::Read
    }
}

fn absent(dir: &File, name: &OsStr) -> Result<()> {
    match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => bail!("workspace entry already exists"),
    }
}

// Darwin exposes each open directory through its volume/file-ID path. The
// caller holds the descriptor until the tempfile operation and fsync finish.
fn pinned_path(dir: &File) -> Result<PathBuf> {
    let m = dir.metadata()?;
    Ok(PathBuf::from(format!("/.vol/{}/{}", m.dev(), m.ino())))
}

fn private_dir(parent: &File, prefix: &str) -> Result<TempDir> {
    Ok(tempfile::Builder::new()
        .prefix(prefix)
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(pinned_path(parent)?)?)
}

fn remove(dir: &File, name: &OsStr, expected: &Entry) -> Result<()> {
    // Isolate the entry before validating and unlinking it. Keep the escrow
    // on any restore conflict; TempDir::drop must never recursively delete a
    // displaced original.
    let staging = private_dir(dir, ".bro-recovery-")?;
    let vault = File::open(staging.path())?;
    let recovery = staging.keep();
    let displaced = OsStr::new("original");
    if let Err(e) = renameat_with(dir, name, &vault, displaced, RenameFlags::NOREPLACE) {
        fs::remove_dir(&recovery)?;
        return Err(e.into());
    }
    let removal = (|| -> Result<()> {
        vault.sync_all()?;
        dir.sync_all()?;
        let mut original = open_child(&vault, displaced, access(expected))?;
        verify(&mut original, expected, expected.id)?;
        check_removal(&original)?;
        let flag = if expected.data == Data::Directory {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        unlinkat(&vault, displaced, flag)?;
        Ok(())
    })();
    if removal.is_err() {
        renameat_with(&vault, displaced, dir, name, RenameFlags::NOREPLACE).with_context(|| {
            format!(
                "restore conflict; displaced entry retained at {}",
                recovery.join(displaced).display()
            )
        })?;
    }
    vault.sync_all()?;
    fs::remove_dir(&recovery)?;
    dir.sync_all()?;
    removal
}

fn create_directory(dir: &File, name: &OsStr) -> Result<Id> {
    let pending = private_dir(dir, ".bro-new-")?;
    let child = File::open(pending.path())?;
    let identity = id(&child.metadata()?)?;
    renameat_with(CWD, pinned_path(&child)?, dir, name, RenameFlags::NOREPLACE)?;
    let _ = pending.keep();
    dir.sync_all()?;
    Ok(identity)
}

fn create_file(dir: &File, name: &OsStr, bytes: &[u8], entry: &Entry) -> Result<Id> {
    let parent = pinned_path(dir)?;
    let mut pending = tempfile::NamedTempFile::new_in(&parent)?;
    pending.write_all(bytes)?;
    pending
        .as_file()
        .set_permissions(Permissions::from_mode(entry.mode))?;
    pending.as_file().set_modified(entry.mtime)?;
    pending.as_file().sync_all()?;
    let identity = id(&pending.as_file().metadata()?)?;
    renameat_with(
        CWD,
        pinned_path(pending.as_file())?,
        dir,
        name,
        RenameFlags::NOREPLACE,
    )?;
    pending.disable_cleanup(true);
    dir.sync_all()?;
    Ok(identity)
}

fn removed(old: &Entry, new: Option<&Entry>) -> bool {
    new.is_none_or(|e| std::mem::discriminant(&e.data) != std::mem::discriminant(&old.data))
}

fn check_removal(file: &File) -> Result<()> {
    // A move in the private copy may appear as delete + create. Never discard
    // a resource fork, custom xattr or ACL that this projection cannot replay.
    let path = CString::new(pinned_path(file)?.as_os_str().as_bytes())?;
    // SAFETY: COPYFILE_CHECK inspects only the held vnode; a null destination
    // and state are valid for this operation.
    let acl = unsafe {
        libc::copyfile(
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            libc::COPYFILE_CHECK | libc::COPYFILE_ACL,
        )
    };
    if acl < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(acl == 0, "entry has an extended ACL");
    let mut names = [0u8; 4096];
    // SAFETY: flistxattr writes at most the supplied buffer size.
    let n =
        unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len(), 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(
        names[..n as usize]
            .split(|b| *b == 0)
            .all(|n| n.is_empty() || n == b"com.apple.provenance"),
        "entry has extended attributes"
    );
    Ok(())
}

// A successful check can depend on a file it never writes, or on the absence
// of a file. Validate the admitted input inventory and contents before accepting
// the shell's exit status, even when the write set is empty.
fn check_inputs(root: &File, before: &Tree, dirs: &BTreeMap<PathBuf, Id>) -> Result<()> {
    let mut seen = 0;
    for relative in std::iter::once(Path::new("")).chain(dirs.keys().map(PathBuf::as_path)) {
        let dir = if relative.as_os_str().is_empty() {
            root.try_clone()?
        } else {
            let parent = parent(root, relative, dirs)?;
            let dir = open_child(&parent, leaf(relative)?, Access::Directory)?;
            ensure!(
                dirs.get(relative) == Some(&id(&dir.metadata()?)?),
                "workspace directory changed"
            );
            dir
        };
        for item in Dir::read_from(&dir)? {
            let item = item?;
            let name = OsStr::from_bytes(item.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            let path = relative.join(name);
            let expected = before.get(&path).with_context(|| {
                format!(
                    "new workspace entry during shell execution: {}",
                    path.display()
                )
            })?;
            if expected.data == Data::Omitted {
                let st = statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
                ensure!(
                    st.st_ino == expected.id.1 && st.st_dev as u64 == expected.id.0,
                    "excluded workspace entry changed"
                );
            } else {
                verify(
                    &mut open_child(&dir, name, access(expected))?,
                    expected,
                    expected.id,
                )
                .with_context(|| format!("workspace input changed: {}", path.display()))?;
            }
            seen += 1;
            ensure!(
                seen <= before.len(),
                "workspace inventory changed during shell execution"
            );
        }
    }
    ensure!(
        seen == before.len(),
        "workspace entry removed during shell execution"
    );
    Ok(())
}

fn preflight(
    root: &File,
    before: &Tree,
    after: &Tree,
    dirs: &BTreeMap<PathBuf, Id>,
    changes: &BTreeSet<&PathBuf>,
) -> Result<()> {
    // Check the whole write set before applying any of it. Unrelated operator
    // edits are outside the write set and remain intact.
    for &path in changes {
        if let Some(old) = before.get(path) {
            ensure!(
                old.data != Data::Omitted,
                "shell output collides with excluded entry: {}",
                path.display()
            );
            let dir = parent(root, path, dirs)?;
            let mut file = open_child(&dir, leaf(path)?, access(old))?;
            verify(&mut file, old, old.id)
                .with_context(|| format!("shell publication conflict at {}", path.display()))?;
            if removed(old, after.get(path)) {
                check_removal(&file)
                    .with_context(|| format!("cannot safely remove {}", path.display()))?;
                if old.data == Data::Directory {
                    for item in Dir::read_from(&file)? {
                        let item = item?;
                        let name = OsStr::from_bytes(item.file_name().to_bytes());
                        if name == "." || name == ".." {
                            continue;
                        }
                        ensure!(
                            before
                                .get(&path.join(name))
                                .is_some_and(|e| e.data != Data::Omitted),
                            "directory contains an excluded or new entry: {}",
                            path.display()
                        );
                    }
                }
            }
        } else if path
            .parent()
            .is_some_and(|p| p.as_os_str().is_empty() || dirs.contains_key(p))
        {
            absent(&parent(root, path, dirs)?, leaf(path)?)?;
        }
    }
    Ok(())
}

pub(super) fn changes(root: &File, before: &Tree, after: &Tree, started: &mut bool) -> Result<()> {
    let mut dirs: BTreeMap<_, _> = before
        .iter()
        .filter(|(_, e)| e.data == Data::Directory)
        .map(|(p, e)| (p.clone(), e.id))
        .collect();
    let mut changes = BTreeSet::new();
    for path in before.keys().chain(after.keys()) {
        match (before.get(path), after.get(path)) {
            (Some(e), None) if e.data == Data::Omitted => (),
            (Some(e), Some(next)) if e.same_contents(next) => (),
            _ => {
                changes.insert(path);
            }
        }
    }
    check_inputs(root, before, &dirs)?;
    preflight(root, before, after, &dirs, &changes)?;
    if changes.is_empty() {
        return Ok(());
    }
    *started = true;
    // Publish destinations before removing sources whenever possible. Only
    // type replacements (and their old children) have to vacate a name first.
    let must_vacate = |path: &Path| {
        path.ancestors().any(|a| {
            before
                .get(a)
                .zip(after.get(a))
                .is_some_and(|(old, new)| removed(old, Some(new)))
        })
    };
    let mut published_files = BTreeMap::new();
    for &path in changes.iter().rev() {
        if let Some(old) = before.get(path)
            && removed(old, after.get(path))
            && must_vacate(path)
        {
            let dir = parent(root, path, &dirs)?;
            remove(&dir, leaf(path)?, old)
                .with_context(|| format!("removing {}", path.display()))?;
            dirs.remove(path);
        }
    }
    for &path in &changes {
        let Some(new) = after.get(path) else { continue };
        let dir = parent(root, path, &dirs)?;
        let name = leaf(path)?;
        match &new.data {
            Data::Directory => {
                if !dirs.contains_key(path) {
                    dirs.insert(path.clone(), create_directory(&dir, name)?);
                }
            }
            Data::File(bytes) => {
                if let Some(old) = before.get(path).filter(|e| matches!(e.data, Data::File(_))) {
                    // Preserve ownership, ACLs and extended attributes. Open
                    // without truncation and check the same descriptor we write.
                    let access = if old.data == new.data {
                        Access::Read
                    } else {
                        Access::ReadWrite
                    };
                    let mut target = open_child(&dir, name, access)?;
                    verify(&mut target, old, old.id)?;
                    if old.data != new.data {
                        target.rewind()?;
                        target.write_all(bytes)?;
                        target.set_len(bytes.len() as u64)?;
                    }
                    if old.mode != new.mode {
                        target.set_permissions(Permissions::from_mode(new.mode))?;
                    }
                    target.set_modified(new.mtime)?;
                    target.sync_all()?;
                    published_files.insert(path, id(&target.metadata()?)?);
                } else {
                    published_files.insert(path, create_file(&dir, name, bytes, new)?);
                }
            }
            Data::Omitted => bail!("unsupported shell output"),
        }
    }
    for &path in changes.iter().rev() {
        if let Some(old) = before.get(path)
            && after.get(path).is_none()
            && !must_vacate(path)
        {
            let dir = parent(root, path, &dirs)?;
            remove(&dir, leaf(path)?, old)
                .with_context(|| format!("removing {}", path.display()))?;
            dirs.remove(path);
        }
    }
    // Apply restrictive directory modes after their children have been written.
    for &path in changes.iter().rev() {
        if let Some(e) = after.get(path).filter(|e| e.data == Data::Directory) {
            let dir = parent(root, path, &dirs)?;
            let child = open_child(&dir, leaf(path)?, Access::Directory)?;
            ensure!(
                Some(&id(&child.metadata()?)?) == dirs.get(path),
                "workspace directory changed during publication"
            );
            child.set_permissions(Permissions::from_mode(e.mode))?;
            child.sync_all()?;
        }
    }
    for &path in &changes {
        if let Some(expected) = after.get(path) {
            let identity = dirs
                .get(path)
                .or_else(|| published_files.get(path))
                .context("missing published identity")?;
            let dir = parent(root, path, &dirs)?;
            verify(
                &mut open_child(&dir, leaf(path)?, access(expected))?,
                expected,
                *identity,
            )
            .with_context(|| format!("verifying published {}", path.display()))?;
        } else if path
            .parent()
            .is_some_and(|p| p.as_os_str().is_empty() || dirs.contains_key(p))
        {
            absent(&parent(root, path, &dirs)?, leaf(path)?)?;
        }
    }
    Ok(())
}
