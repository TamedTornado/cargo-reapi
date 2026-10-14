//! The project a strict Cargo invocation builds, from Cargo's own full
//! metadata, and the checks that keep its target directory clear of every
//! source.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// The selected project, with every path canonical: symlinks resolved, so
/// that two spellings of one directory compare equal.
pub struct Project {
    pub workspace_root: PathBuf,
    /// Cargo's own target directory for the project: beside a
    /// `--manifest-path` workspace, or wherever `build.target-dir` and
    /// `CARGO_TARGET_DIR` put it.
    pub target_directory: PathBuf,
    pub packages: Vec<Package>,
}

/// A package in the project's resolve.
pub struct Package {
    pub id: String,
    pub root: PathBuf,
    /// A package built from a local path rather than a registry or Git.
    pub local: bool,
    /// The directories holding the package's target sources (`src`,
    /// `src/bin`, `examples`, `tests`, `benches` and any other), other than
    /// the package root itself.
    pub source_directories: Vec<PathBuf>,
}

impl Project {
    /// Runs Cargo's full metadata for what the invocation selects. It runs
    /// outside the strict sandbox, so it also acquires the project's
    /// dependency sources and settles its lockfile; see
    /// [`crate::query::cargo_metadata_output`].
    pub fn resolve(cwd: &Path, cache: &Path, metadata_arguments: &[OsString]) -> Result<Self> {
        let output = crate::query::cargo_metadata_output(cwd, cache, metadata_arguments)
            .context("running cargo metadata for the selected project")?;
        if !output.status.success() {
            bail!(
                "cargo metadata failed for the selected project: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("reading cargo metadata for the selected project")?;
        Self::from_metadata(&metadata)
    }

    fn from_metadata(metadata: &serde_json::Value) -> Result<Self> {
        let path = |field: &str| -> Result<PathBuf> {
            let text = metadata[field]
                .as_str()
                .with_context(|| format!("cargo metadata has no {field}"))?;
            canonical(Path::new(text))
        };

        let packages = metadata["packages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(Package::from_metadata)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            workspace_root: path("workspace_root")?,
            target_directory: path("target_directory")?,
            packages,
        })
    }

    /// Refuses a target directory that overlaps a source. Cargo treats the
    /// target as disposable output, the gate leaves it out of the inputs it
    /// hashes, and a gate hit restores it whole, so a target holding or
    /// inside a source would let a hit overwrite edited source.
    ///
    /// `target` is canonical; see [`canonical`]. It may not contain the
    /// working directory, the selected workspace, any package (path
    /// dependencies included) or a declared input, and may not lie inside a
    /// directory holding target sources or inside a declared input.
    pub fn validate_target(
        &self,
        cwd: &Path,
        target: &Path,
        declared_inputs: &[PathBuf],
    ) -> Result<()> {
        let mut roots = vec![canonical(cwd)?, self.workspace_root.clone()];
        let mut containers = Vec::new();
        for package in &self.packages {
            roots.push(package.root.clone());
            roots.extend(package.source_directories.iter().cloned());
            containers.extend(package.source_directories.iter().cloned());
        }
        for input in declared_inputs {
            let input = canonical(&cwd.join(input))?;
            roots.push(input.clone());
            containers.push(input);
        }

        if let Some(source) = roots.iter().find(|root| root.starts_with(target)) {
            bail!(
                "Cargo's target directory {} contains source {}; cargo-reapi treats the target as \
                 disposable output it may restore whole, so choose a target directory outside \
                 every package and input",
                target.display(),
                source.display()
            );
        }
        if let Some(source) = containers
            .iter()
            .find(|container| target.starts_with(container))
        {
            bail!(
                "Cargo's target directory {} lies inside source {}; cargo-reapi treats the target \
                 as disposable output it may restore whole, so choose a target directory outside \
                 every source directory and input",
                target.display(),
                source.display()
            );
        }
        Ok(())
    }
}

impl Package {
    fn from_metadata(package: &serde_json::Value) -> Result<Self> {
        let manifest = package["manifest_path"]
            .as_str()
            .context("cargo metadata lists a package without a manifest")?;
        let root = canonical(
            Path::new(manifest)
                .parent()
                .context("a package manifest has no directory")?,
        )?;

        let mut source_directories = Vec::new();
        for target in package["targets"].as_array().into_iter().flatten() {
            let Some(source) = target["src_path"].as_str() else {
                continue;
            };
            let Some(directory) = Path::new(source).parent() else {
                continue;
            };
            let directory = canonical(directory)?;
            if directory != root && !source_directories.contains(&directory) {
                source_directories.push(directory);
            }
        }

        Ok(Self {
            id: package["id"].as_str().unwrap_or(manifest).to_owned(),
            root,
            local: package["source"].is_null(),
            source_directories,
        })
    }
}

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
/// run outside a project. Only for invocations that do not use the strict
/// sandbox, which resolve the full [`Project`] instead.
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
