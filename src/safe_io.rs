//! The one way cargo-reapi writes files that a build can also reach.
//!
//! A build may plant, anywhere it can write, a symlink or a hard link to a
//! file it cannot write, such as `Cargo.lock`. cargo-reapi's own writes, in the
//! driver outside the sandbox and in the compiler wrapper inside it, must
//! never land there. The policy, for every such write:
//!
//! - a link is never followed;
//! - a file is changed in place only when it is a regular file with a single
//!   link, and then only by appending a line under a lock;
//! - anything else is replaced: a new file is written beside it and renamed
//!   over its name, so an inode something else is linked to is never modified;
//! - a lock file is opened without following links, and only ever locked.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

/// Opens `path` as a lock file, creating it when it is missing. A symlink at
/// the path is an error and is never followed; the file is only locked.
pub fn open_lock(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path)
}

/// Writes `bytes` as the whole contents of `path` by writing a new file in
/// its directory and renaming it over the name. A link at `path` is replaced
/// by the file, and what it was linked to is untouched.
pub fn write_replacing(path: &Path, bytes: &[u8]) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let directory = path.parent().context("the path has no directory")?;
    let name = path.file_name().context("the path has no name")?;
    let temporary = directory.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    let written = file.write_all(bytes);
    drop(file);
    let renamed = written.and_then(|()| std::fs::rename(&temporary, path));
    if renamed.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    renamed.with_context(|| format!("writing {}", path.display()))
}

/// Appends a line to `path` under an exclusive lock, when `path` is a
/// regular file with a single link; any other file there is replaced first.
pub fn append_line(path: &Path, line: &str) -> Result<()> {
    let directory = path.parent().context("the path has no directory")?;
    let name = path.file_name().context("the path has no name")?;
    #[cfg(unix)]
    {
        crate::beneath::Beneath::open(directory)?.append_line(Path::new(name), line)
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, name);
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        writeln!(file, "{line}")?;
        fs2::FileExt::unlock(&file)?;
        Ok(())
    }
}
