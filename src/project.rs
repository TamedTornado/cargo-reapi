//! Where Cargo puts things, as Cargo reports it.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// `path`, absolute, with every symlink resolved. A path that does not
/// exist yet, such as a target directory Cargo has not created, resolves
/// its deepest existing ancestor and keeps the rest, which may not climb
/// with `..`.
pub fn canonical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let mut missing = Vec::new();
    let mut existing = absolute.as_path();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("resolving {}", existing.display()));
            }
        }

        let mut components = existing.components();
        match components.next_back() {
            Some(Component::Normal(name)) => missing.push(name.to_owned()),
            Some(Component::CurDir) => {}
            _ => bail!(
                "cargo-reapi cannot resolve {} exactly: it climbs out of a directory that does not \
                 exist",
                absolute.display()
            ),
        }
        existing = components.as_path();
    }
}

/// The target directory `cargo metadata --no-deps` reports for the selected
/// project, or `None` where Cargo cannot describe one, as for commands that
/// run outside a project.
pub fn target_directory(cwd: &Path, metadata_arguments: &[OsString]) -> Option<PathBuf> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .args(metadata_arguments)
        .current_dir(cwd)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    metadata["target_directory"].as_str().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::canonical;

    #[cfg(unix)]
    #[test]
    fn a_missing_path_resolves_its_deepest_existing_ancestor() {
        let root = tempfile::tempdir().expect("root");
        let real = root.path().join("real");
        fs::create_dir(&real).expect("real directory");
        std::os::unix::fs::symlink(&real, root.path().join("link")).expect("link");

        let resolved = canonical(&root.path().join("link/artifacts/new")).expect("resolved");

        let expected = real.canonicalize().expect("real").join("artifacts/new");
        assert_eq!(resolved, expected);
        assert!(canonical(&root.path().join("missing/../real")).is_err());
    }
}
