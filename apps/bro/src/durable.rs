//! Publish each new directory entry before a checkpoint may confirm its effects.
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub(crate) fn create_dirs(path: &Path, private: bool) -> io::Result<()> {
    let absolute = std::path::absolute(path)?;
    let mut pending = Vec::new();
    let mut existing = absolute.as_path();
    while !existing.try_exists()? {
        pending.push(existing);
        existing = existing.parent().ok_or(io::ErrorKind::NotFound)?;
    }
    if !existing.is_dir() {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    for dir in pending.into_iter().rev() {
        let parent = dir.parent().ok_or(io::ErrorKind::NotFound)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            if private {
                builder.mode(0o700);
            }
            match builder.create(dir) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => (),
                result => result?,
            }
            File::open(parent)?.sync_all()?;
        }
        #[cfg(windows)]
        {
            let _ = private;
            // Publish the directory name with MOVEFILE_WRITE_THROUGH too.
            let mut staging = tempfile::tempdir_in(parent)?;
            match atomicwrites::move_atomic(staging.path(), dir) {
                Ok(()) => staging.disable_cleanup(true),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir() => (),
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

pub(crate) fn write_file(
    path: &Path,
    overwrite: atomicwrites::OverwriteBehavior,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // Sync file contents, then rename with directory fsync on Unix or
    // MOVEFILE_WRITE_THROUGH on Windows. Preserve no-clobber for new files.
    atomicwrites::AtomicFile::new(path, overwrite)
        .write_with_options(write, options)
        .map_err(io::Error::from)
}
