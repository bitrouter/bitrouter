//! Validate the write set against the original directory and durably publish it.
use super::{CopyPurpose, Data, Entry, Id, MAX_FILE, Tree, id, read_file, write_entries};
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

type Before<'a> = BTreeMap<PathBuf, &'a Entry>;

// Resolve every ancestor from the open root, accepting only the directory
// identities captured on admission to the copy or created by this publication.
fn parent(root: &File, path: &Path, dirs: &BTreeMap<PathBuf, Id>) -> Result<File> {
    directory(root, path.parent().context("missing parent")?, dirs)
}

fn directory(root: &File, path: &Path, dirs: &BTreeMap<PathBuf, Id>) -> Result<File> {
    let mut dir = root.try_clone()?;
    let mut relative = PathBuf::new();
    for component in path.iter() {
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
        id(&m)? == identity && m.mode() & 0o7777 == expected.mode,
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

const ORIGINAL: &str = "original";

struct Escrow {
    directory: File,
    path: PathBuf,
}

impl Escrow {
    fn finish(self, parent: &File, cleanup: Result<()>) -> Result<()> {
        (|| -> Result<()> {
            cleanup?;
            self.directory.sync_all()?;
            fs::remove_dir(&self.path)?;
            parent.sync_all()?;
            Ok(())
        })()
        .with_context(|| format!("finishing workspace escrow at {}", self.path.display()))
    }
}

fn displace(dir: &File, name: &OsStr, apply: impl FnOnce(&File) -> Result<()>) -> Result<Escrow> {
    // The vault must survive a restore conflict. Leave a displaced original
    // under explicit cleanup instead of TempDir's recursive removal.
    let staging = private_dir(dir, ".bro-recovery-")?;
    let vault = File::open(staging.path())?;
    renameat_with(dir, name, &vault, ORIGINAL, RenameFlags::NOREPLACE)?;
    let escrow = Escrow {
        directory: vault,
        path: staging.keep(),
    };
    let applied = (|| {
        escrow.directory.sync_all()?;
        dir.sync_all()?;
        apply(&escrow.directory)
    })();
    if let Err(error) = applied {
        renameat_with(
            &escrow.directory,
            ORIGINAL,
            dir,
            name,
            RenameFlags::NOREPLACE,
        )
        .with_context(|| {
            format!(
                "{error:#}; restore conflict; displaced entry retained at {}",
                escrow.path.join(ORIGINAL).display()
            )
        })?;
        escrow.finish(dir, Ok(()))?;
        return Err(error);
    }
    Ok(escrow)
}

fn remove(dir: &File, name: &OsStr, expected: &Entry) -> Result<()> {
    displace(dir, name, |vault| {
        let mut original = open_child(vault, OsStr::new(ORIGINAL), access(expected))?;
        verify(&mut original, expected, expected.id)?;
        check_removal(&original)?;
        let flag = if expected.data == Data::Directory {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        unlinkat(vault, ORIGINAL, flag)?;
        Ok(())
    })?
    .finish(dir, Ok(()))
}

fn rename_spelling(dir: &File, source: &OsStr, path: &Path, expected: &Entry) -> Result<()> {
    let destination = leaf(path)?;
    displace(dir, source, |vault| {
        let mut original = open_child(vault, OsStr::new(ORIGINAL), access(expected))?;
        verify(&mut original, expected, expected.id)?;
        renameat_with(vault, ORIGINAL, dir, destination, RenameFlags::NOREPLACE)?;
        Ok(())
    })?
    .finish(dir, Ok(()))
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

// Resolve spelling changes using the workspace filesystem's own comparison
// rules. Descendants of renamed directories are compared under the new path.
fn spellings(
    root: &File,
    before: &Tree,
    after: &Tree,
    dirs: &BTreeMap<PathBuf, Id>,
) -> Result<BTreeMap<PathBuf, PathBuf>> {
    let by_inode: BTreeMap<_, _> = before
        .iter()
        .filter(|(_, entry)| entry.data != Data::Omitted)
        .map(|(p, e)| ((e.id.0, e.id.1), p))
        .collect();
    let mut originals = BTreeMap::from([(Path::new(""), PathBuf::new())]);
    let mut renamed = BTreeMap::new();
    for (path, next) in after {
        let Some(original_parent) = originals.get(path.parent().context("missing parent")?) else {
            continue;
        };
        let name = leaf(path)?;
        let mut original = original_parent.join(name);
        if !before.contains_key(&original) {
            let dir = parent(root, &original, dirs)?;
            let stat = match statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(rustix::io::Errno::NOENT) => continue,
                result => result?,
            };
            let alias = by_inode
                .get(&(stat.st_dev as u64, stat.st_ino))
                .filter(|p| p.parent() == Some(original_parent))
                .with_context(|| {
                    format!(
                        "shell output collides with workspace entry: {}",
                        path.display()
                    )
                })?;
            ensure!(
                !after.contains_key(&path.with_file_name(leaf(alias)?))
                    && renamed.insert((*alias).clone(), path.clone()).is_none(),
                "shell output has conflicting spellings at {}",
                path.display()
            );
            original = (*alias).clone();
        }
        if next.data == Data::Directory && dirs.contains_key(&original) {
            originals.insert(path.as_path(), original);
        }
    }
    Ok(renamed)
}

fn remap_before<'a>(before: &'a Tree, renamed: &BTreeMap<PathBuf, PathBuf>) -> Before<'a> {
    let mut remapped = Before::new();
    let mut moved_dirs = BTreeMap::new();
    for (path, entry) in before {
        let destination = renamed.get(path).cloned().or_else(|| {
            let parent: &PathBuf = moved_dirs.get(path.parent()?)?;
            Some(parent.join(path.file_name()?))
        });
        if let Some(destination) = &destination
            && entry.data == Data::Directory
        {
            moved_dirs.insert(path.as_path(), destination.clone());
        }
        remapped.insert(destination.unwrap_or_else(|| path.clone()), entry);
    }
    remapped
}

fn preflight(
    root: &File,
    before: &Before<'_>,
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

pub(super) type StageReplacement = fn(&Path, &Path, &Tree) -> Result<BTreeMap<PathBuf, Id>>;

pub(super) fn stage_replacement(
    destination: &Path,
    path: &Path,
    after: &Tree,
) -> Result<BTreeMap<PathBuf, Id>> {
    write_entries(
        destination,
        path.parent().context("missing parent")?,
        after
            .range(path.to_path_buf()..)
            .take_while(|(p, _)| p.starts_with(path)),
        CopyPurpose::Replacement,
    )
}

fn inspect_removed(
    dir: &File,
    name: &OsStr,
    path: &Path,
    before: &Before<'_>,
    delete: bool,
) -> Result<usize> {
    let expected = before
        .get(path)
        .filter(|e| e.data != Data::Omitted)
        .with_context(|| format!("unexpected entry in replaced tree: {}", path.display()))?;
    let mut file = open_child(dir, name, access(expected))?;
    verify(&mut file, expected, expected.id)?;
    check_removal(&file)?;
    let mut seen = 1;
    if expected.data == Data::Directory {
        for item in Dir::read_from(&file)? {
            let item = item?;
            let child = OsStr::from_bytes(item.file_name().to_bytes());
            if child == "." || child == ".." {
                continue;
            }
            seen += inspect_removed(&file, child, &path.join(child), before, delete)?;
        }
    }
    if delete {
        // Isolate each child before its final identity check and unlink.
        remove(dir, name, expected)?;
    }
    Ok(seen)
}

fn replace(
    dir: &File,
    path: &Path,
    before: &Before<'_>,
    after: &Tree,
    stage: StageReplacement,
) -> Result<BTreeMap<PathBuf, Id>> {
    let name = leaf(path)?;
    let staging = private_dir(dir, ".bro-new-")?;
    let staged = File::open(staging.path())?;
    let identities = stage(staging.path(), path, after)
        .with_context(|| format!("staging shell replacement at {}", path.display()))?;
    let original = OsStr::new(ORIGINAL);
    let expected_count = before
        .range(path.to_path_buf()..)
        .take_while(|(p, _)| p.starts_with(path))
        .count();
    let escrow = displace(dir, name, |vault| {
        ensure!(
            inspect_removed(vault, original, path, before, false)? == expected_count,
            "workspace entry removed from replaced tree"
        );
        renameat_with(&staged, name, dir, name, RenameFlags::NOREPLACE)?;
        Ok(())
    })?;
    let cleanup = (|| {
        // Keep the original in escrow until the new name is durable.
        dir.sync_all()?;
        staged.sync_all()?;
        inspect_removed(&escrow.directory, original, path, before, true)?;
        Ok(())
    })();
    escrow.finish(dir, cleanup)?;
    Ok(identities)
}

pub(super) fn changes(
    root: &File,
    original: &Tree,
    after: &Tree,
    recovery: &Path,
    started: &mut bool,
    stage: StageReplacement,
) -> Result<()> {
    let admitted_dirs = original
        .iter()
        .filter(|(_, e)| e.data == Data::Directory)
        .map(|(p, e)| (p.clone(), e.id))
        .collect();
    check_inputs(root, original, &admitted_dirs)?;
    let renamed = spellings(root, original, after, &admitted_dirs)?;
    let before = remap_before(original, &renamed);
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
    preflight(root, &before, after, &dirs, &changes)?;
    if changes.is_empty() && renamed.is_empty() {
        return Ok(());
    }
    // Keep the admitted bytes before any write, including in-place edits
    // whose inode, ownership and extended metadata must survive publication.
    let snapshot = recovery.join(ORIGINAL);
    fs::create_dir(&snapshot)?;
    write_entries(
        &snapshot,
        Path::new(""),
        original.iter(),
        CopyPurpose::Recovery,
    )
    .context("saving original shell inputs for recovery")?;
    File::open(recovery)?.sync_all()?;
    *started = true;

    for (source, destination) in &renamed {
        let dir = parent(root, destination, &dirs)?;
        rename_spelling(
            &dir,
            leaf(source)?,
            destination,
            original.get(source).context("missing original spelling")?,
        )
        .with_context(|| format!("renaming workspace entry to {}", destination.display()))?;
    }

    let replacements: BTreeSet<_> = changes
        .iter()
        .copied()
        .filter(|&path| {
            before
                .get(path)
                .zip(after.get(path))
                .is_some_and(|(old, new)| removed(old, Some(new)))
        })
        .map(PathBuf::as_path)
        .collect();
    let under_replacement =
        |path: &Path| path.ancestors().skip(1).any(|p| replacements.contains(p));
    let mut published = BTreeMap::new();
    for &path in &changes {
        if under_replacement(path) {
            continue;
        }
        let Some(new) = after.get(path) else { continue };
        let dir = parent(root, path, &dirs)?;
        let name = leaf(path)?;
        if replacements.contains(path.as_path()) {
            let identities = replace(&dir, path, &before, after, stage)?;
            for (old_path, _) in before
                .range(path.clone()..)
                .take_while(|(p, _)| p.starts_with(path))
            {
                dirs.remove(old_path);
            }
            dirs.extend(
                identities
                    .iter()
                    .filter(|(p, _)| after.get(*p).is_some_and(|e| e.data == Data::Directory))
                    .map(|(p, &identity)| (p.clone(), identity)),
            );
            published.extend(identities);
            continue;
        }
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
                    published.insert(path.clone(), id(&target.metadata()?)?);
                } else {
                    published.insert(path.clone(), create_file(&dir, name, bytes, new)?);
                }
            }
            Data::Omitted => bail!("unsupported shell output"),
        }
    }
    for &path in changes.iter().rev() {
        if let Some(old) = before.get(path)
            && after.get(path).is_none()
            && !under_replacement(path)
        {
            let dir = parent(root, path, &dirs)?;
            remove(&dir, leaf(path)?, old)
                .with_context(|| format!("removing {}", path.display()))?;
            dirs.remove(path);
        }
    }
    // Apply restrictive directory modes after their children have been written.
    for &path in changes.iter().rev() {
        if replacements.contains(path.as_path()) || under_replacement(path) {
            continue;
        }
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
    let mut spelling_checks: BTreeMap<_, BTreeSet<_>> = BTreeMap::new();
    for path in renamed.values() {
        spelling_checks
            .entry(path.parent().context("missing parent")?)
            .or_default()
            .insert(leaf(path)?);
    }
    for (relative, mut expected) in spelling_checks {
        let dir = directory(root, relative, &dirs)?;
        for item in Dir::read_from(&dir)?.take(super::MAX_ENTRIES + 2) {
            let item = item?;
            expected.remove(OsStr::from_bytes(item.file_name().to_bytes()));
            if expected.is_empty() {
                break;
            }
        }
        ensure!(
            expected.is_empty(),
            "workspace entry spelling changed during publication"
        );
    }
    for path in changes.into_iter().chain(renamed.values()) {
        if let Some(expected) = after.get(path) {
            let identity = dirs
                .get(path)
                .or_else(|| published.get(path))
                .or_else(|| before.get(path).map(|e| &e.id))
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
