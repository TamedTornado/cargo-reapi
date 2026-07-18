use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::invocation::RustcInvocation;

pub struct CaptureOptions {
    action_log: PathBuf,
}

impl CaptureOptions {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            action_log: env::var_os("CARGO_REAPI_ACTION_LOG")
                .map(PathBuf::from)
                .context("CARGO_REAPI_ACTION_LOG is required in capture mode")?,
        })
    }
}

#[derive(Debug, Serialize)]
struct ActionCapture {
    schema_version: u32,
    captured_at_unix_ms: u128,
    compiler: String,
    working_directory: String,
    crate_name: Option<String>,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    inputs: Vec<InputDigest>,
    output_directory: Option<String>,
    output_files: Vec<String>,
    exit_code: i32,
}

#[derive(Debug, Serialize)]
struct InputDigest {
    path: String,
    sha256: String,
    size_bytes: u64,
}

pub fn capture_invocation(invocation: &RustcInvocation, options: &CaptureOptions) -> Result<i32> {
    let inputs = discover_inputs(invocation)?;
    let output_files = invocation.output_files()?;
    let exit_code = invocation.execute()?;
    let capture = ActionCapture {
        schema_version: 1,
        captured_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before Unix epoch")?
            .as_millis(),
        compiler: display(&invocation.compiler),
        working_directory: display(&invocation.cwd),
        crate_name: invocation.crate_name().map(lossy),
        arguments: invocation.args.iter().map(|value| lossy(value)).collect(),
        environment: captured_environment(),
        inputs,
        output_directory: invocation.out_dir().as_deref().map(display),
        output_files: output_files.iter().map(|path| display(path)).collect(),
        exit_code,
    };
    append_capture(&options.action_log, &capture)?;
    Ok(exit_code)
}

fn discover_inputs(invocation: &RustcInvocation) -> Result<Vec<InputDigest>> {
    let mut candidates = BTreeSet::new();
    for (index, arg) in invocation.args.iter().enumerate() {
        let path = PathBuf::from(arg);
        if path.is_file() {
            candidates.insert(path);
        }
        if let Some(response_path) = arg.to_string_lossy().strip_prefix('@') {
            let response_path = PathBuf::from(response_path);
            if response_path.is_file() {
                candidates.insert(response_path);
            }
        }
        if arg == "--extern"
            && let Some(value) = invocation.args.get(index + 1).and_then(extern_path)
        {
            candidates.insert(value);
        }
    }

    if let Some(manifest_dir) = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) {
        for entry in WalkDir::new(manifest_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !is_ignored(entry.path()))
        {
            let entry = entry.context("walking Cargo package inputs")?;
            if entry.file_type().is_file() {
                candidates.insert(entry.into_path());
            }
        }
    }

    candidates.iter().map(|path| digest_file(path)).collect()
}

fn extern_path(value: &OsString) -> Option<PathBuf> {
    let text = value.to_string_lossy();
    text.split_once('=').map(|(_, path)| PathBuf::from(path))
}

fn is_ignored(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component.as_os_str().to_str(), Some(".git" | "target")))
}

fn digest_file(path: &Path) -> Result<InputDigest> {
    let bytes = fs::read(path).with_context(|| format!("reading input {}", path.display()))?;
    let size_bytes = u64::try_from(bytes.len()).context("input file is too large")?;
    Ok(InputDigest {
        path: display(path),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        size_bytes,
    })
}

fn captured_environment() -> BTreeMap<String, String> {
    env::vars()
        .filter(|(name, _)| is_compiler_environment(name))
        .collect()
}

fn is_compiler_environment(name: &str) -> bool {
    name.starts_with("CARGO_CFG_")
        || name.starts_with("CARGO_FEATURE_")
        || name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_TARGET_")
        || matches!(
            name,
            "CARGO_CRATE_NAME"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "CARGO_MANIFEST_DIR"
                | "CARGO_MANIFEST_PATH"
                | "CARGO_PRIMARY_PACKAGE"
                | "DEBUG"
                | "HOST"
                | "NUM_JOBS"
                | "OPT_LEVEL"
                | "OUT_DIR"
                | "PROFILE"
                | "RUSTC"
                | "RUSTC_LINKER"
                | "RUSTDOCFLAGS"
                | "RUSTFLAGS"
                | "TARGET"
        )
}

fn append_capture(path: &Path, capture: &ActionCapture) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating action log directory {}", parent.display()))?;
    }
    let line = serde_json::to_string(capture).context("serializing action capture")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening action log {}", path.display()))?;
    file.lock_exclusive().context("locking action log")?;
    writeln!(file, "{line}").context("writing action capture")?;
    FileExt::unlock(&file).context("unlocking action log")
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn lossy(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::is_compiler_environment;

    #[test]
    fn captures_cargo_compiler_contract() {
        assert!(is_compiler_environment("CARGO_PKG_VERSION"));
        assert!(is_compiler_environment("CARGO_CFG_TARGET_OS"));
        assert!(is_compiler_environment(
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER"
        ));
        assert!(is_compiler_environment("OUT_DIR"));
    }

    #[test]
    fn never_captures_registry_credentials() {
        assert!(!is_compiler_environment("CARGO_REGISTRY_TOKEN"));
        assert!(!is_compiler_environment("CARGO_REGISTRIES_PRIVATE_TOKEN"));
        assert!(!is_compiler_environment("AWS_SECRET_ACCESS_KEY"));
    }
}
