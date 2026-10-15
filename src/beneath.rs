//! Writes the driver makes inside the target after untrusted code has run.
//!
//! The target is writable by build scripts and proc macros, so any path in it
//! may become a symlink while a build runs. Everything the driver writes
//! there afterwards, outside the sandbox, goes through a directory handle
//! opened on the canonical target and one `openat` per path component with
//! `O_NOFOLLOW`, so no symlink anywhere on the path can send a write
//! elsewhere.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

/// A directory the driver writes beneath, held open.
pub struct Beneath {
    root: OwnedFd,
}

impl Beneath {
    /// Opens `root`, which must be a real directory: a symlink at `root`
    /// itself is refused. Callers pass the canonical target.
    pub fn open(root: &Path) -> Result<Self> {
        let fd = open_at(
            libc::AT_FDCWD,
            root.as_os_str(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
        .with_context(|| format!("opening {} as a real directory", root.display()))?;
        Ok(Self { root: fd })
    }

    /// Writes `bytes` to `relative`, replacing whatever stands there, even a
    /// symlink, with a new regular file. Missing directories are created.
    pub fn write_file(&self, relative: &Path, bytes: &[u8]) -> Result<()> {
        let (parent, name) = self.parent_of(relative, true)?;
        // A link at the name is removed, never followed; `O_EXCL` then
        // refuses one planted between the two calls.
        unlink_at(&parent, name)?;
        let fd = open_at(
            parent.as_raw_fd(),
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o644,
        )
        .with_context(|| format!("creating {}", relative.display()))?;
        let mut file = File::from(fd);
        file.write_all(bytes)
            .with_context(|| format!("writing {}", relative.display()))
    }

    /// Appends `line` and a newline to `relative`, under an exclusive lock.
    /// A symlink at the name, or in any directory on the way, is an error.
    /// Only a regular file nothing else is linked to is appended to: a file
    /// with a second hard link, or any other kind, may be one a build made to
    /// point at a source, so it is replaced by a new file and never written.
    pub fn append_line(&self, relative: &Path, line: &str) -> Result<()> {
        let (parent, name) = self.parent_of(relative, true)?;
        let mut file = open_owned_for_append(&parent, name)
            .with_context(|| format!("opening {} without following links", relative.display()))?;
        fs2::FileExt::lock_exclusive(&file)?;
        let written = writeln!(file, "{line}");
        fs2::FileExt::unlock(&file)?;
        written.with_context(|| format!("appending to {}", relative.display()))
    }

    /// Removes the file or link at `relative`, if there is one. A directory
    /// on the way that is missing or is a link means nothing of ours is
    /// there, so there is nothing to remove.
    pub fn remove_file(&self, relative: &Path) -> Result<()> {
        match self.parent_of(relative, false) {
            Ok((parent, name)) => unlink_at(&parent, name),
            Err(_) => Ok(()),
        }
    }

    /// The directory holding `relative`, opened without following links,
    /// and its final component.
    fn parent_of<'a>(&self, relative: &'a Path, create: bool) -> Result<(OwnedFd, &'a OsStr)> {
        let mut components = relative.components().peekable();
        let mut directory = dup(self.root.as_raw_fd())?;
        loop {
            let Some(Component::Normal(name)) = components.next() else {
                bail!("{} is not a path beneath the target", relative.display());
            };
            if components.peek().is_none() {
                return Ok((directory, name));
            }
            directory = match open_directory(&directory, name) {
                Ok(next) => next,
                Err(error) if create && error.raw_os_error() == Some(libc::ENOENT) => {
                    make_directory(&directory, name)?;
                    open_directory(&directory, name)
                        .with_context(|| format!("opening {}", relative.display()))?
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "{} is not a real directory path beneath the target",
                            relative.display()
                        )
                    });
                }
            };
        }
    }
}

/// Opens `name` in `parent` for appending, creating it when missing, and
/// only when it is a regular file with a single link; otherwise the entry is
/// removed and a new file is created.
fn open_owned_for_append(parent: &OwnedFd, name: &OsStr) -> Result<File> {
    let append = libc::O_WRONLY | libc::O_APPEND | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let existing = open_at(parent.as_raw_fd(), name, append | libc::O_CREAT, 0o644);
    if let Ok(fd) = existing {
        let file = File::from(fd);
        let metadata = file.metadata()?;
        if metadata.is_file() && std::os::unix::fs::MetadataExt::nlink(&metadata) == 1 {
            return Ok(file);
        }
    }
    // A link at the name (ELOOP), another kind of file, or a second hard
    // link: replace the name. Unlinking never touches what it was linked to.
    unlink_at(parent, name)?;
    let fd = open_at(
        parent.as_raw_fd(),
        name,
        append | libc::O_CREAT | libc::O_EXCL,
        0o644,
    )?;
    Ok(File::from(fd))
}

fn open_directory(parent: &OwnedFd, name: &OsStr) -> std::io::Result<OwnedFd> {
    open_at(
        parent.as_raw_fd(),
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

fn make_directory(parent: &OwnedFd, name: &OsStr) -> Result<()> {
    let name = c_string(name)?;
    // SAFETY: `name` is a valid NUL-terminated string and `parent` an open
    // directory descriptor.
    let status = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("creating a directory");
    }
    Ok(())
}

fn unlink_at(parent: &OwnedFd, name: &OsStr) -> Result<()> {
    let name = c_string(name)?;
    // SAFETY: as in `make_directory`.
    let status = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error).context("removing a file");
        }
    }
    Ok(())
}

fn open_at(directory: i32, path: &OsStr, flags: i32, mode: u32) -> std::io::Result<OwnedFd> {
    let path = CString::new(path.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `path` is a valid NUL-terminated string; the descriptor
    // returned is owned by the `OwnedFd` that takes it.
    let fd = unsafe { libc::openat(directory, path.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by `openat` and is owned by no one else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn dup(fd: i32) -> Result<OwnedFd> {
    // SAFETY: `fd` is an open descriptor owned by the caller.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error()).context("duplicating a directory handle");
    }
    // SAFETY: `duplicate` is a new descriptor owned by no one else.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

fn c_string(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).context("a path component holds a NUL byte")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Beneath;

    #[test]
    fn writes_never_follow_a_link_in_the_file_or_a_directory_on_the_way() {
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("target");
        let source = root.path().join("source");
        fs::create_dir_all(&target).expect("target");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("lib.rs"), "source").expect("source file");
        let beneath = Beneath::open(&target).expect("handle");

        // A link at the file: replaced, and the source it named is untouched.
        fs::create_dir_all(target.join("admin")).expect("admin");
        std::os::unix::fs::symlink(source.join("lib.rs"), target.join("admin/marker"))
            .expect("link");
        beneath
            .write_file("admin/marker".as_ref(), b"marker")
            .expect("write");
        assert_eq!(fs::read(source.join("lib.rs")).expect("source"), b"source");
        assert_eq!(
            fs::read(target.join("admin/marker")).expect("marker"),
            b"marker"
        );

        // A link as the directory: refused, and nothing lands in the source.
        fs::remove_dir_all(target.join("admin")).expect("remove admin");
        std::os::unix::fs::symlink(&source, target.join("admin")).expect("link");
        assert!(
            beneath
                .write_file("admin/marker".as_ref(), b"marker")
                .is_err()
        );
        assert!(beneath.append_line("admin/log".as_ref(), "line").is_err());
        beneath
            .remove_file("admin/marker".as_ref())
            .expect("remove");
        assert!(!source.join("marker").exists());
        assert!(!source.join("log").exists());
    }

    #[test]
    fn an_append_never_reaches_a_file_with_a_second_hard_link() {
        let root = tempfile::tempdir().expect("root");
        let target = root.path().join("target");
        fs::create_dir_all(&target).expect("target");
        fs::write(root.path().join("lock"), "source").expect("source");
        fs::hard_link(root.path().join("lock"), target.join("log")).expect("hard link");
        let beneath = Beneath::open(&target).expect("handle");

        beneath.append_line("log".as_ref(), "line").expect("append");

        assert_eq!(
            fs::read(root.path().join("lock")).expect("source"),
            b"source"
        );
        assert_eq!(
            fs::read_to_string(target.join("log")).expect("log"),
            "line\n"
        );
        // A link is replaced too, and a file of its own is appended to.
        fs::remove_file(target.join("log")).expect("remove");
        std::os::unix::fs::symlink(root.path().join("lock"), target.join("log")).expect("link");
        beneath.append_line("log".as_ref(), "one").expect("append");
        beneath.append_line("log".as_ref(), "two").expect("append");
        assert_eq!(
            fs::read(root.path().join("lock")).expect("source"),
            b"source"
        );
        assert_eq!(
            fs::read_to_string(target.join("log")).expect("log"),
            "one\ntwo\n"
        );
    }

    #[test]
    fn a_linked_root_is_refused_and_appends_accumulate() {
        let root = tempfile::tempdir().expect("root");
        let real = root.path().join("real");
        fs::create_dir_all(&real).expect("real");
        std::os::unix::fs::symlink(&real, root.path().join("link")).expect("link");
        assert!(Beneath::open(&root.path().join("link")).is_err());

        let beneath = Beneath::open(&real).expect("handle");
        beneath
            .append_line("a/b/log".as_ref(), "one")
            .expect("append");
        beneath
            .append_line("a/b/log".as_ref(), "two")
            .expect("append");
        assert_eq!(
            fs::read_to_string(real.join("a/b/log")).expect("log"),
            "one\ntwo\n"
        );
    }
}
