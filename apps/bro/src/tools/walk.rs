//! Enumerate each directory through its retained handle. Ignore matchers use
//! workspace-relative names; their inputs are opened with the same file rules.
use super::files::contents;
use crate::workspace::children::{Access, Entries, Kind, Parent};
use anyhow::{Result, ensure};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct Rules {
    ignore: Vec<Arc<Gitignore>>,
    git: Vec<Arc<Gitignore>>,
    exclude: Option<Arc<Gitignore>>,
}

fn rules(dir: &Parent, name: &str, relative: &Path) -> Result<Option<Arc<Gitignore>>> {
    let mut file = match dir.open(OsStr::new(name), Access::Read) {
        Ok(f) => f,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(None);
        }
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let bytes = contents(&mut file)?;
    let mut builder = GitignoreBuilder::new(relative);
    for line in String::from_utf8_lossy(&bytes).lines() {
        builder.add_line(None, line)?;
    }
    Ok(Some(Arc::new(builder.build()?)))
}

impl Rules {
    fn add_dir(&mut self, dir: &Arc<Parent>, path: &Path) -> Result<()> {
        if let Some(m) = rules(dir, ".ignore", path)? {
            self.ignore.push(m);
        }
        if let Some(m) = rules(dir, ".gitignore", path)? {
            self.git.push(m);
        }
        if let Ok(git) = dir.child(OsStr::new(".git"))
            && let Ok(info) = git.child(OsStr::new("info"))
        {
            self.exclude = rules(&info, "exclude", path)?;
        }
        Ok(())
    }
    fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.ignore
            .iter()
            .rev()
            .chain(self.git.iter().rev())
            .chain(&self.exclude)
            .map(|r| r.matched(path, is_dir))
            .find(|m| !m.is_none())
            .is_some_and(|m| m.is_ignore())
    }
}

struct Frame {
    dir: Arc<Parent>,
    path: PathBuf,
    entries: Entries,
    rules: Rules,
}

impl Frame {
    fn new(dir: Arc<Parent>, path: PathBuf, mut rules: Rules) -> Result<Self> {
        rules.add_dir(&dir, &path)?;
        let entries = dir.entries()?;
        Ok(Self {
            dir,
            path,
            entries,
            rules,
        })
    }
}

pub(super) fn walk(
    dir: Arc<Parent>,
    relative: &Path,
    cancel: &CancellationToken,
    mut visit: impl FnMut(&Parent, &OsStr, &Path, Kind) -> Result<bool>,
) -> Result<()> {
    // Include in-workspace ancestors when the search begins in a subtree.
    let mut inherited = Rules::default();
    let mut prefix = PathBuf::new();
    for (ancestor, name) in dir.ancestors().into_iter().zip(relative) {
        inherited.add_dir(ancestor, &prefix)?;
        prefix.push(name);
    }
    let mut stack = vec![Frame::new(dir, relative.to_owned(), inherited)?];
    while let Some(frame) = stack.last_mut() {
        ensure!(!cancel.is_cancelled(), "search cancelled");
        let Some(entry) = frame.entries.next() else {
            stack.pop();
            continue;
        };
        let (name, kind) = entry?;
        let path = frame.path.join(&name);
        if name == ".git"
            || kind == Kind::Link
            || frame.rules.ignored(&path, kind == Kind::Directory)
        {
            continue;
        }
        if !visit(&frame.dir, &name, &path, kind)? {
            break;
        }
        if kind == Kind::Directory {
            let next = Frame::new(frame.dir.child(&name)?, path, frame.rules.clone())?;
            stack.push(next);
        }
    }
    Ok(())
}
