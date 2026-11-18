//! Seatbelt grants pathnames. Give it a private working copy, then publish
//! changes through the admitted directory descriptor after the shell exits.
use crate::workspace::children::{Access, open_child};
use anyhow::{Context, Result, ensure};
use rustix::fs::{AtFlags, Dir, FileType, statat};
use std::collections::BTreeMap;
use std::ffi::OsStr;

use std::fs::{self, File, Metadata, Permissions};
use std::io::{Read, Write};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, PermissionsExt},
};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::TempDir;

mod publish;

const MAX_ENTRIES: usize = 16_384;
const MAX_DEPTH: usize = 64;
pub(super) const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Eq, PartialEq)]
struct Id(u64, u64, SystemTime);

fn id(m: &Metadata) -> Result<Id> {
    Ok(Id(m.dev(), m.ino(), m.created()?))
}

#[derive(PartialEq)]
enum Data {
    Directory,
    File(Vec<u8>),
    Omitted,
}

struct Entry {
    id: Id,
    mode: u32,
    data: Data,
    mtime: SystemTime,
}

impl Entry {
    fn new(m: &Metadata, data: Data) -> Result<Self> {
        Ok(Self {
            id: id(m)?,
            mode: m.mode() & 0o777,
            data,
            mtime: m.modified()?,
        })
    }

    fn same_contents(&self, other: &Self) -> bool {
        self.data == other.data
            && self.mode == other.mode
            && (self.data == Data::Directory || self.mtime == other.mtime)
    }
}

type Tree = BTreeMap<PathBuf, Entry>;

fn read_file(file: &mut File, meta: &Metadata, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o6000 == 0,
        "unsafe shell working-copy file"
    );
    ensure!(
        meta.len() <= limit,
        "shell working-copy byte limit exceeded"
    );
    let mut data = Vec::new();
    Read::take(&mut *file, meta.len() + 1).read_to_end(&mut data)?;
    let end = file.metadata()?;
    ensure!(
        data.len() as u64 == meta.len()
            && end.nlink() == 1
            && (end.ctime(), end.ctime_nsec()) == (meta.ctime(), meta.ctime_nsec()),
        "file changed while reading shell working copy"
    );
    Ok(data)
}

struct Scan {
    tree: Tree,
    bytes_left: u64,
    device: u64,
    // Only original entries may be omitted. New links or special outputs
    // cannot be published as ordinary files.
    source: bool,
}

impl Scan {
    fn run(root: &File, source: bool) -> Result<Tree> {
        let mut scan = Self {
            tree: Tree::new(),
            bytes_left: MAX_BYTES,
            device: root.metadata()?.dev(),
            source,
        };
        scan.walk(root, Path::new(""), 0)?;
        Ok(scan.tree)
    }

    fn walk(&mut self, dir: &File, relative: &Path, depth: usize) -> Result<()> {
        ensure!(
            depth <= MAX_DEPTH,
            "shell working-copy depth limit exceeded"
        );
        for item in Dir::read_from(dir)? {
            let item = item?;
            let name = OsStr::from_bytes(item.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            ensure!(
                self.tree.len() < MAX_ENTRIES,
                "shell working-copy entry limit exceeded"
            );
            let path = relative.join(name);
            let st = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let ty = FileType::from_raw_mode(st.st_mode);
            let is_dir = ty == FileType::Directory;
            if st.st_dev as u64 != self.device
                || st.st_mode & 0o7000 != 0
                || !(is_dir || ty == FileType::RegularFile && st.st_nlink == 1)
            {
                ensure!(
                    self.source,
                    "unsupported shell output at {}",
                    path.display()
                );
                self.tree.insert(
                    path,
                    Entry {
                        id: Id(st.st_dev as u64, st.st_ino, SystemTime::UNIX_EPOCH),
                        mode: 0,
                        data: Data::Omitted,
                        mtime: SystemTime::UNIX_EPOCH,
                    },
                );
                continue;
            }
            let flags = if is_dir {
                Access::Directory
            } else {
                Access::Read
            };
            let mut file = open_child(dir, name, flags)
                .with_context(|| format!("opening {}", path.display()))?;
            let m = file.metadata()?;
            ensure!(
                m.dev() == self.device && m.ino() == st.st_ino && m.is_dir() == is_dir,
                "shell working-copy entry changed"
            );
            let data = if is_dir {
                Data::Directory
            } else {
                let data = read_file(&mut file, &m, self.bytes_left.min(MAX_FILE))
                    .with_context(|| format!("copying {}", path.display()))?;
                self.bytes_left -= data.len() as u64;
                Data::File(data)
            };
            self.tree.insert(path.clone(), Entry::new(&m, data)?);
            if is_dir {
                self.walk(&file, &path, depth + 1)?;
            }
        }
        Ok(())
    }
}

pub(super) struct WorkingCopy {
    private: TempDir,
    pub(super) path: PathBuf,
    pub(super) directory: File,
    before: Tree,
}

impl WorkingCopy {
    pub(super) fn empty() -> Result<Self> {
        // Ignore TMPDIR. The outer 0700 directory is outside the Seatbelt
        // grant, including when the command changes its working directory mode.
        let private = tempfile::Builder::new()
            .prefix("bro-shell-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in("/private/tmp")?;
        let path = private.path().join("workspace");
        fs::create_dir(&path)?;
        Ok(Self {
            directory: File::open(&path)?,
            private,
            path,
            before: Tree::new(),
        })
    }

    pub(super) fn prepare(root: &File) -> Result<Self> {
        let root_id = id(&root.metadata()?)?;
        for ancestor in Path::new("/private/tmp").ancestors() {
            ensure!(
                id(&fs::metadata(ancestor)?)? != root_id,
                "workspace contains the private shell staging location"
            );
        }
        let before = Scan::run(root, true)?;
        let mut copy = Self::empty()?;
        for (name, entry) in &before {
            let destination = copy.path.join(name);
            match &entry.data {
                Data::Omitted => continue,
                Data::Directory => fs::create_dir(&destination)?,
                Data::File(bytes) => {
                    let mut file = File::create_new(destination)?;
                    file.write_all(bytes)?;
                    file.set_modified(entry.mtime)?;
                    file.set_permissions(Permissions::from_mode(entry.mode))?;
                }
            }
        }
        for (name, entry) in before.iter().rev() {
            if entry.data == Data::Directory {
                fs::set_permissions(copy.path.join(name), Permissions::from_mode(entry.mode))?;
            }
        }
        copy.before = before;
        Ok(copy)
    }

    pub(super) fn publish(self, root: &File) -> Result<()> {
        let after = Scan::run(&self.directory, false)?;
        let mut started = false;
        let result = publish::changes(root, &self.before, &after, &mut started);
        if started && let Err(error) = result {
            let recovery = self.private.keep().join("workspace");
            return Err(error).with_context(|| {
                format!("private recovery copy retained at {}", recovery.display())
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn changing_an_original_file_or_parent_rejects_the_whole_write_set() -> Result<()> {
        for replace_parent in [false, true] {
            let fixture = tempfile::tempdir()?;
            let workspace = fixture.path().join("workspace");
            let peer = fixture.path().join("peer");
            fs::create_dir_all(workspace.join("sub"))?;
            fs::create_dir(&peer)?;
            fs::write(workspace.join("sub/note"), "admitted")?;
            fs::write(peer.join("note"), "peer")?;
            let root = File::open(&workspace)?;
            let copy = WorkingCopy::prepare(&root)?;
            assert_eq!(fs::metadata(copy.private.path())?.mode() & 0o777, 0o700);
            fs::write(copy.path.join("a-new"), "pending")?;
            fs::write(copy.path.join("sub/note"), "shell")?;
            if replace_parent {
                fs::rename(workspace.join("sub"), workspace.join("old"))?;
                symlink(&peer, workspace.join("sub"))?;
            } else {
                fs::write(workspace.join("sub/note"), "operator")?;
            }
            assert!(copy.publish(&root).is_err());
            assert_eq!(fs::read(peer.join("note"))?, b"peer");
            if replace_parent {
                assert_eq!(fs::read(workspace.join("old/note"))?, b"admitted");
            } else {
                assert_eq!(fs::read(workspace.join("sub/note"))?, b"operator");
            }
            assert!(!workspace.join("a-new").exists());
        }
        Ok(())
    }

    #[test]
    fn read_only_check_cannot_confirm_stale_inputs() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = File::open(fixture.path())?;
        fs::write(fixture.path().join("input"), "first")?;
        let copy = WorkingCopy::prepare(&root)?;
        fs::write(fixture.path().join("input"), "second")?;
        assert!(copy.publish(&root).is_err());
        Ok(())
    }

    #[test]
    fn an_unsupported_output_link_refuses_publication() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let root = File::open(fixture.path())?;
        let copy = WorkingCopy::prepare(&root)?;
        fs::write(copy.path.join("created"), "data")?;
        fs::hard_link(copy.path.join("created"), copy.path.join("alias"))?;
        assert!(copy.publish(&root).is_err());
        assert!(!fixture.path().join("created").exists());
        Ok(())
    }
}
