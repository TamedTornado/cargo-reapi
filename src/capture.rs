use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::action::{
    ActionInput, DeterministicAction, PlatformIdentity, RemoteEligibility, ToolchainIdentity,
    action_key,
};
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
    action_key: String,
    toolchain: ToolchainIdentity,
    platform: PlatformIdentity,
    remote_eligibility: RemoteEligibility,
    working_directory: String,
    crate_name: Option<String>,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    inputs: Vec<ActionInput>,
    output_directory: Option<String>,
    output_files: Vec<String>,
    execution: String,
    exit_code: i32,
}

#[derive(Debug)]
pub struct PreparedInvocation {
    pub action_key: String,
    pub remote_eligibility: RemoteEligibility,
    pub output_files: Vec<PreparedOutput>,
    pub path_mappings: Vec<(String, String)>,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub inputs: Vec<PreparedInput>,
    pub toolchain_sha256: String,
    pub platform_os: &'static str,
    pub platform_arch: &'static str,
    compiler: String,
    toolchain: ToolchainIdentity,
    platform: PlatformIdentity,
    working_directory: String,
    crate_name: Option<String>,
    output_directory: Option<String>,
}

#[derive(Debug)]
pub struct PreparedInput {
    pub logical_path: String,
    pub actual_path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug)]
pub struct PreparedOutput {
    pub logical_path: String,
    pub actual_path: PathBuf,
}

#[derive(Debug, Serialize)]
struct CaptureRoots {
    workspace: PathBuf,
    package: Option<PathBuf>,
    target: PathBuf,
    toolchain: PathBuf,
}

pub fn capture_invocation(invocation: &RustcInvocation, options: &CaptureOptions) -> Result<i32> {
    let prepared = prepare_invocation(invocation)?;
    let exit_code = invocation.execute()?;
    record_invocation(options, &prepared, "local-capture", exit_code)?;
    Ok(exit_code)
}

pub fn prepare_invocation(invocation: &RustcInvocation) -> Result<PreparedInvocation> {
    let roots = CaptureRoots::from_env(invocation)?;
    let (inputs, mut eligibility_reasons) = discover_inputs(invocation, &roots)?;
    let output_files = invocation.output_files()?;
    let normalized_outputs = output_files
        .iter()
        .map(|path| roots.normalize(path, &invocation.cwd))
        .collect::<Result<Vec<_>, _>>();
    let outputs = match normalized_outputs {
        Ok(outputs) => outputs,
        Err(error) => {
            eligibility_reasons.push(error.to_string());
            Vec::new()
        }
    };
    if outputs.is_empty() {
        eligibility_reasons.push("action has no declared outputs".to_owned());
    }
    if invocation.is_link_action() {
        eligibility_reasons.push(
            "link action input discovery is incomplete; native libraries, linker binaries, response files, and platform SDK inputs must be declared"
                .to_owned(),
        );
    }
    let working_directory = match roots.normalize(&invocation.cwd, &invocation.cwd) {
        Ok(path) => path,
        Err(error) => {
            eligibility_reasons.push(error.to_string());
            display(&invocation.cwd)
        }
    };
    let toolchain = toolchain_identity(&invocation.compiler)?;
    let platform = PlatformIdentity {
        os: env::consts::OS,
        arch: env::consts::ARCH,
    };
    let arguments: Vec<String> = invocation
        .args
        .iter()
        .map(|value| roots.normalize_text(&lossy(value)))
        .collect();
    let environment: BTreeMap<String, String> = captured_environment()
        .into_iter()
        .map(|(name, value)| (name, roots.normalize_text(&value)))
        .collect();
    let deterministic_action = DeterministicAction {
        compiler: ToolchainIdentity {
            sha256: toolchain.sha256.clone(),
            size_bytes: toolchain.size_bytes,
            version: toolchain.version.clone(),
        },
        platform: PlatformIdentity {
            os: platform.os,
            arch: platform.arch,
        },
        working_directory: working_directory.clone(),
        arguments: arguments.clone(),
        environment: environment.clone(),
        inputs: inputs
            .iter()
            .map(|input| ActionInput {
                path: input.logical_path.clone(),
                sha256: input.sha256.clone(),
                size_bytes: input.size_bytes,
            })
            .collect(),
        outputs: outputs.clone(),
    };
    let key = action_key(&deterministic_action)?;
    Ok(PreparedInvocation {
        action_key: key,
        toolchain_sha256: toolchain.sha256.clone(),
        platform_os: platform.os,
        platform_arch: platform.arch,
        compiler: display(&invocation.compiler),
        toolchain,
        platform,
        remote_eligibility: RemoteEligibility::from_reasons(eligibility_reasons),
        working_directory,
        crate_name: invocation.crate_name().map(lossy),
        arguments,
        environment,
        inputs,
        output_directory: invocation
            .out_dir()
            .as_deref()
            .map(|path| roots.normalize_text(&display(path))),
        output_files: outputs
            .into_iter()
            .zip(output_files)
            .map(|(logical_path, actual_path)| PreparedOutput {
                logical_path,
                actual_path: absolute(&actual_path, &invocation.cwd),
            })
            .collect(),
        path_mappings: roots.path_mappings(),
    })
}

pub fn record_invocation(
    options: &CaptureOptions,
    prepared: &PreparedInvocation,
    execution: &str,
    exit_code: i32,
) -> Result<()> {
    let capture = ActionCapture {
        schema_version: 2,
        captured_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before Unix epoch")?
            .as_millis(),
        compiler: prepared.compiler.clone(),
        action_key: prepared.action_key.clone(),
        toolchain: prepared.toolchain.clone(),
        platform: prepared.platform.clone(),
        remote_eligibility: prepared.remote_eligibility.clone(),
        working_directory: prepared.working_directory.clone(),
        crate_name: prepared.crate_name.clone(),
        arguments: prepared.arguments.clone(),
        environment: prepared.environment.clone(),
        inputs: prepared
            .inputs
            .iter()
            .map(|input| ActionInput {
                path: input.logical_path.clone(),
                sha256: input.sha256.clone(),
                size_bytes: input.size_bytes,
            })
            .collect(),
        output_directory: prepared.output_directory.clone(),
        output_files: prepared
            .output_files
            .iter()
            .map(|output| output.logical_path.clone())
            .collect(),
        execution: execution.to_owned(),
        exit_code,
    };
    append_capture(&options.action_log, &capture)?;
    Ok(())
}

impl CaptureRoots {
    fn from_env(invocation: &RustcInvocation) -> Result<Self> {
        let workspace = env::var_os("CARGO_REAPI_WORKSPACE_ROOT")
            .map(PathBuf::from)
            .context("CARGO_REAPI_WORKSPACE_ROOT is required in capture mode")?;
        let target = env::var_os("CARGO_REAPI_TARGET_ROOT")
            .map(PathBuf::from)
            .context("CARGO_REAPI_TARGET_ROOT is required in capture mode")?;
        let toolchain = invocation
            .compiler
            .parent()
            .and_then(Path::parent)
            .context("compiler path has no toolchain root")?
            .to_path_buf();
        Ok(Self {
            workspace: absolute(&workspace, &invocation.cwd),
            package: env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .map(|path| absolute(&path, &invocation.cwd)),
            target: absolute(&target, &invocation.cwd),
            toolchain: absolute(&toolchain, &invocation.cwd),
        })
    }

    fn normalize(&self, path: &Path, cwd: &Path) -> Result<String> {
        let absolute = absolute(path, cwd);
        if let Ok(relative) = absolute.strip_prefix(&self.target) {
            return Ok(logical_path("target", relative));
        }
        if let Some(package) = &self.package
            && let Ok(relative) = absolute.strip_prefix(package)
        {
            return Ok(logical_path("package", relative));
        }
        if let Ok(relative) = absolute.strip_prefix(&self.workspace) {
            return Ok(logical_path("workspace", relative));
        }
        if let Ok(relative) = absolute.strip_prefix(&self.toolchain) {
            return Ok(logical_path("toolchain", relative));
        }
        anyhow::bail!(
            "path is outside declared package, workspace, target, and toolchain roots: {}",
            absolute.display()
        )
    }

    fn normalize_text(&self, value: &str) -> String {
        let mut normalized = value.to_owned();
        let mut mappings = vec![(&self.target, "target")];
        if let Some(package) = &self.package {
            mappings.push((package, "package"));
        }
        mappings.push((&self.workspace, "workspace"));
        mappings.push((&self.toolchain, "toolchain"));
        for (root, label) in mappings {
            if let Some(root) = root.to_str() {
                normalized = normalized.replace(root, label);
            }
        }
        normalized
    }

    fn path_mappings(&self) -> Vec<(String, String)> {
        let mut roots = vec![(&self.target, "target")];
        if let Some(package) = &self.package {
            roots.push((package, "package"));
        }
        roots.push((&self.workspace, "workspace"));
        roots.push((&self.toolchain, "toolchain"));
        roots
            .into_iter()
            .filter_map(|(root, label)| {
                root.to_str()
                    .map(|root| (label.to_owned(), root.to_owned()))
            })
            .collect()
    }
}

fn discover_inputs(
    invocation: &RustcInvocation,
    roots: &CaptureRoots,
) -> Result<(Vec<PreparedInput>, Vec<String>)> {
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

    for input_root in ["CARGO_MANIFEST_DIR", "OUT_DIR"]
        .into_iter()
        .filter_map(|name| env::var_os(name).map(PathBuf::from))
    {
        for entry in WalkDir::new(input_root)
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

    let mut inputs = Vec::new();
    let mut reasons = Vec::new();
    for path in candidates {
        match roots.normalize(&path, &invocation.cwd) {
            Ok(logical_path) => inputs.push(digest_file(&path, logical_path, &invocation.cwd)?),
            Err(error) => reasons.push(error.to_string()),
        }
    }
    inputs.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    Ok((inputs, reasons))
}

fn extern_path(value: &OsString) -> Option<PathBuf> {
    let text = value.to_string_lossy();
    text.split_once('=').map(|(_, path)| PathBuf::from(path))
}

fn is_ignored(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component.as_os_str().to_str(), Some(".git" | "target")))
}

fn digest_file(path: &Path, logical_path: String, cwd: &Path) -> Result<PreparedInput> {
    let bytes = fs::read(path).with_context(|| format!("reading input {}", path.display()))?;
    let size_bytes = u64::try_from(bytes.len()).context("input file is too large")?;
    Ok(PreparedInput {
        logical_path,
        actual_path: absolute(path, cwd),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        size_bytes,
    })
}

fn toolchain_identity(compiler: &Path) -> Result<ToolchainIdentity> {
    let bytes = fs::read(compiler)
        .with_context(|| format!("reading compiler executable {}", compiler.display()))?;
    let version = Command::new(compiler)
        .arg("-vV")
        .output()
        .with_context(|| format!("reading compiler identity from {}", compiler.display()))?;
    if !version.status.success() {
        anyhow::bail!(
            "compiler identity command failed: {}",
            String::from_utf8_lossy(&version.stderr).trim()
        );
    }
    Ok(ToolchainIdentity {
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        size_bytes: u64::try_from(bytes.len()).context("compiler executable is too large")?,
        version: String::from_utf8(version.stdout)
            .context("compiler identity is not UTF-8")?
            .trim()
            .to_owned(),
    })
}

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn logical_path(root: &str, relative: &Path) -> String {
    if relative.as_os_str().is_empty() {
        root.to_owned()
    } else {
        format!("{root}/{}", relative.to_string_lossy())
    }
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
