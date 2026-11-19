//! Resolve logical names once, then carry held objects through the operation.
use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Seek},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::harness::runtime::EffectStatus;

use super::{MAX_FILE_BYTES, WorkspaceTools, validate_relative};
use crate::workspace::children::{Access, Parent, regular};
use crate::workspace::publish::{create_file, verify_destination, write_file};

pub(super) enum Node {
    File(File),
    Directory(Arc<Parent>),
}

pub(super) struct Destination {
    parent: Arc<Parent>,
    name: OsString,
    file: Option<File>,
}

pub(super) fn contents(file: &mut File) -> Result<Vec<u8>> {
    regular(file)?;
    file.rewind()?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "file exceeds the 2 MiB limit"
    );
    Ok(bytes)
}

impl WorkspaceTools {
    fn pinned_directory(&self, relative: &Path) -> Result<Arc<Parent>> {
        let mut dir = Parent::root(&self.directory)?;
        for name in relative {
            dir = dir.child(name)?;
        }
        Ok(dir)
    }

    pub(super) fn lookup(&self, user: Option<&str>) -> Result<(std::path::PathBuf, Node)> {
        let relative = match user {
            None | Some(".") => {
                return Ok((
                    Default::default(),
                    Node::Directory(Parent::root(&self.directory)?),
                ));
            }
            Some(s) => validate_relative(s).map_err(anyhow::Error::msg)?,
        };
        // Resolve explicitly requested in-workspace symlinks to a relative
        // spelling. Every subsequent component open is relative and nofollow.
        let mapped = self
            .resolved(&self.root.join(relative))
            .map_err(anyhow::Error::msg)?;
        let relative = mapped.strip_prefix(&self.root)?.to_owned();
        let Some(name) = relative.file_name() else {
            return Ok((relative, Node::Directory(Parent::root(&self.directory)?)));
        };
        let parent = self.pinned_directory(relative.parent().context("missing parent")?)?;
        let node = match parent.child(name) {
            Ok(dir) => Node::Directory(dir),
            Err(e) if e.kind() == io::ErrorKind::NotADirectory => {
                Node::File(parent.open(name, Access::Read)?)
            }
            Err(e) => return Err(e.into()),
        };
        Ok((relative, node))
    }

    pub(super) fn destination(
        &self,
        path: &str,
        create: bool,
        effect: &mut EffectStatus,
    ) -> Result<Destination> {
        let relative = validate_relative(path).map_err(anyhow::Error::msg)?;
        let name = relative.file_name().context("missing filename")?.to_owned();
        let full = self.root.join(relative.parent().context("missing parent")?);
        let mut existing = full.as_path();
        if create {
            while !existing.try_exists()? {
                existing = existing.parent().context("missing workspace parent")?;
            }
        }
        let mapped = self.resolved(existing).map_err(anyhow::Error::msg)?;
        let mut parent = self.pinned_directory(mapped.strip_prefix(&self.root)?)?;
        for child in full.strip_prefix(existing)? {
            let next = match parent.child(child) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    *effect = EffectStatus::Unknown;
                    match parent.create(child) {
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                            parent.child(child)?
                        }
                        result => result?,
                    }
                }
                result => result?,
            };
            parent = next;
        }
        let file = match parent.open(&name, Access::ReadWrite) {
            Ok(file) => {
                ensure!(
                    !file.metadata()?.permissions().readonly(),
                    "existing target is read-only"
                );
                Some(file)
            }
            Err(e) if create && e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        Ok(Destination { parent, name, file })
    }
}

impl Destination {
    pub fn original(&mut self) -> Result<Option<Vec<u8>>> {
        self.file.as_mut().map(contents).transpose()
    }

    pub fn publish(
        mut self,
        data: &[u8],
        original: Option<&[u8]>,
        effect: &mut EffectStatus,
    ) -> Result<()> {
        if let Some(file) = &mut self.file {
            ensure!(
                contents(file)?.as_slice() == original.context("missing original")?,
                "file changed while preparing the edit"
            );
            verify_destination(&self.parent, &self.name, file)?;
            *effect = EffectStatus::Unknown;
            write_file(file, data)?;
        } else {
            let mut file = match create_file(&self.parent, &self.name) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(error.into());
                }
                result => {
                    *effect = EffectStatus::Unknown;
                    result?
                }
            };
            write_file(&mut file, data)?;
        }
        self.parent.sync_ancestors()?;
        Ok(())
    }
}
