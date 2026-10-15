use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::action::{
    ActionInput, DeterministicAction, PlatformIdentity, RemoteEligibility, ToolchainIdentity,
    action_key,
};
use crate::environment::CompileEnvironment;
use crate::invocation::RustcInvocation;
use crate::relocation::{
    logical_digest, logical_text, record_output_mappings, record_path_mappings,
};
use crate::resource::ResourceCapacity;

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
    cache_eligibility: RemoteEligibility,
    working_directory: String,
    crate_name: Option<String>,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    keyed_environment: BTreeMap<String, String>,
    stripped_environment: Vec<String>,
    inputs: Vec<ActionInput>,
    output_directory: Option<String>,
    output_files: Vec<String>,
    execution: String,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_capacity: Option<ResourceCapacity>,
}

#[derive(Debug)]
pub struct PreparedInvocation {
    pub action_key: String,
    pub remote_eligibility: RemoteEligibility,
    pub cache_eligibility: RemoteEligibility,
    pub output_files: Vec<PreparedOutput>,
    /// Roots that name inputs and outputs by logical path.
    pub path_mappings: Vec<(String, String)>,
    /// Roots that artifacts are relocated against.
    pub relocation_mappings: Vec<(String, String)>,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub keyed_environment: BTreeMap<String, String>,
    pub stripped_environment: Vec<String>,
    pub dep_info: Option<PreparedOutput>,
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

#[derive(Clone, Debug)]
pub struct PreparedOutput {
    pub logical_path: String,
    pub actual_path: PathBuf,
}

struct PreparedPaths {
    output_files: Vec<PathBuf>,
    outputs: Vec<String>,
    working_directory: String,
}

#[derive(Debug, Serialize)]
struct CaptureRoots {
    workspace: PathBuf,
    package: Option<PathBuf>,
    target: PathBuf,
    toolchain: PathBuf,
}

/// One file rustc's own dep-info lists as read by a compile.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, Serialize)]
pub struct DepInput {
    /// The path as rustc named it, under its root's label and with no `..`
    /// collapsed: resolving it again follows the links on the way as rustc
    /// did, so a link that moved is noticed.
    pub raw_logical_path: String,
    /// The file it resolved to, by logical path.
    pub logical_path: String,
    pub sha256: String,
}

/// Every file the compile just read, as rustc's own dep-info says, with its
/// digest. The action key covers the command line, the package directory and
/// `OUT_DIR`, but `include!`, `include_str!` and `#[path]` can read anywhere,
/// so a cached entry is a hit only while all of these still resolve to the
/// same files with the same content. Fails when the dep-info is missing, or
/// lists a file outside the declared roots or one that cannot be read, so the
/// entry is not published.
pub fn read_dep_inputs(
    invocation: &RustcInvocation,
    prepared: &PreparedInvocation,
) -> Result<Vec<DepInput>> {
    let dep_info = prepared
        .dep_info
        .as_ref()
        .context("the compile writes no dep-info, so its inputs are unknown")?;
    let text = fs::read_to_string(&dep_info.actual_path)
        .with_context(|| format!("reading dep-info {}", dep_info.actual_path.display()))?;
    let roots = CaptureRoots::from_env(invocation)?;
    let mut inputs = BTreeMap::new();
    for listed in dep_info_paths(&text) {
        let unresolved = invocation.cwd.join(&listed);
        let raw_logical_path = roots.label_unresolved(&unresolved)?;
        // The file rustc read: links resolved component by component, `..`
        // after a link meaning the link target's parent, as for the kernel.
        let resolved = unresolved
            .canonicalize()
            .with_context(|| format!("resolving dep-info input {}", unresolved.display()))?;
        let logical_path = roots.normalize(&resolved, &invocation.cwd)?;
        let (sha256, _) = logical_digest(&resolved, &prepared.relocation_mappings)
            .with_context(|| format!("digesting dep-info input {}", resolved.display()))?;
        inputs.insert(raw_logical_path.clone(), (logical_path, sha256));
    }
    Ok(inputs
        .into_iter()
        .map(|(raw_logical_path, (logical_path, sha256))| DepInput {
            raw_logical_path,
            logical_path,
            sha256,
        })
        .collect())
}

/// Whether every recorded dep-info input still resolves to the file it did,
/// with the content it had.
pub fn dep_inputs_match(
    invocation: &RustcInvocation,
    recorded: &[DepInput],
    mappings: &[(String, String)],
) -> Result<bool> {
    let roots = CaptureRoots::from_env(invocation)?;
    for input in recorded {
        let Some(unresolved) = roots.denormalize(&input.raw_logical_path) else {
            return Ok(false);
        };
        let Ok(resolved) = unresolved.canonicalize() else {
            return Ok(false);
        };
        if roots.normalize(&resolved, &invocation.cwd).ok().as_deref()
            != Some(input.logical_path.as_str())
        {
            return Ok(false);
        }
        match logical_digest(&resolved, mappings) {
            Ok((sha256, _)) if sha256 == input.sha256 => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// The prerequisites a dependency file lists. rustc escapes only spaces, with
/// a backslash; every other backslash is part of the name.
pub(crate) fn dep_info_paths(text: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((target, prerequisites)) = split_rule(line) else {
            continue;
        };
        if prerequisites.trim().is_empty() {
            paths.insert(unescape_spaces(target));
            continue;
        }
        let mut current = String::new();
        let mut characters = prerequisites.chars().peekable();
        while let Some(character) = characters.next() {
            match character {
                '\\' if characters.peek() == Some(&' ') => {
                    characters.next();
                    current.push(' ');
                }
                ' ' => {
                    if !current.is_empty() {
                        paths.insert(std::mem::take(&mut current));
                    }
                }
                other => current.push(other),
            }
        }
        if !current.is_empty() {
            paths.insert(current);
        }
    }
    paths
}

/// A rule line's target and prerequisites, split at the first `:` that is
/// followed by a space or the end of the line and is not an escaped one.
fn split_rule(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let separator = (0..bytes.len()).find(|&index| {
        bytes[index] == b':' && bytes.get(index + 1).is_none_or(|next| *next == b' ')
    })?;
    Some((&line[..separator], &line[separator + 1..]))
}

fn unescape_spaces(text: &str) -> String {
    text.replace("\\ ", " ")
}

/// Why a compile is never cached for want of knowing its native inputs.
///
/// A link step (a binary, test, proc macro or other dynamic library) lets
/// the linker pick libraries, start files and itself from places cargo-reapi
/// does not key, so it always runs. So does a compile whose `-L` or `-l`
/// arguments reach a file it cannot show to lie in the workspace, the build
/// output or the compiler's own sysroot, such as a static library an rlib
/// bundles. Whatever cannot be told cheaply counts as outside.
fn native_input_reason(invocation: &RustcInvocation, roots: &CaptureRoots) -> Option<String> {
    if invocation.requires_native_linker() {
        return Some(
            "a link step reads libraries and a linker that are not keyed, so it always runs"
                .to_owned(),
        );
    }
    let inside = |path: &Path| {
        let path = absolute(path, &invocation.cwd);
        [
            Some(&roots.workspace),
            Some(&roots.target),
            Some(&roots.toolchain),
            roots.package.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|root| path.starts_with(root))
    };
    let mut search_directories = Vec::new();
    let mut libraries = Vec::new();
    let mut arguments = invocation
        .args
        .iter()
        .map(|argument| argument.to_string_lossy());
    while let Some(argument) = arguments.next() {
        let value = if argument == "-L" || argument == "-l" {
            arguments.next().map(|value| value.into_owned())
        } else {
            argument
                .strip_prefix("-L")
                .or_else(|| argument.strip_prefix("-l"))
                .map(str::to_owned)
        };
        let Some(value) = value else {
            continue;
        };
        if argument.starts_with("-L") {
            let (kind, path) = value.split_once('=').unwrap_or(("all", value.as_str()));
            if kind == "dependency" || kind == "crate" {
                continue;
            }
            if !inside(Path::new(path)) {
                return Some(format!(
                    "-L {value} reaches outside the workspace and toolchain"
                ));
            }
            search_directories.push(PathBuf::from(path));
        } else if argument.starts_with("-l") {
            libraries.push(value);
        }
    }
    for library in libraries {
        let (kind, name) = library
            .split_once('=')
            .unwrap_or(("dylib", library.as_str()));
        let name = name.split(':').next().unwrap_or(name);
        let found = kind != "framework"
            && search_directories.iter().any(|directory| {
                ["a", "so", "dylib", "lib"].iter().any(|extension| {
                    absolute(directory, &invocation.cwd)
                        .join(format!("lib{name}.{extension}"))
                        .exists()
                        || absolute(directory, &invocation.cwd)
                            .join(format!("{name}.{extension}"))
                            .exists()
                })
            });
        if !found {
            return Some(format!(
                "-l {library} is not found beside a -L directory inside the workspace, so it \
                 may be a system library"
            ));
        }
    }
    None
}

/// Records the action and runs the compiler with the caller's whole
/// environment. The recorded key covers the environment a cached compile
/// would start with.
pub fn capture_invocation(invocation: &RustcInvocation, options: &CaptureOptions) -> Result<i32> {
    let environment = CompileEnvironment::for_compile(invocation, None);
    let mut prepared = prepare_invocation(invocation, &environment)?;
    // Capture observes Cargo unchanged: nothing is stripped from the compile.
    prepared.stripped_environment.clear();
    let exit_code = invocation.execute()?;
    record_invocation(options, &prepared, "local-capture", exit_code)?;
    Ok(exit_code)
}

pub fn prepare_invocation(
    invocation: &RustcInvocation,
    compile_environment: &CompileEnvironment,
) -> Result<PreparedInvocation> {
    let roots = CaptureRoots::from_env(invocation)?;
    // Cargo probes rustc through the wrapper with control invocations such as
    // `rustc -vV`. Those invocations produce no artifact and may run while the
    // outer target is deliberately mounted read-only by the sandbox. Path
    // mappings are restore metadata, so only artifact-producing compilations
    // need to persist them.
    if should_record_path_mappings(invocation) {
        record_path_mappings(&roots.path_mappings())?;
    }
    let (inputs, mut eligibility_reasons) = discover_inputs(invocation, &roots)?;
    eligibility_reasons.extend(native_input_reason(invocation, &roots));
    let paths = prepare_paths(invocation, &roots, &mut eligibility_reasons)?;
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
    let key_environment = compile_environment.keyed_values(|value| roots.normalize_text(value));
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
        working_directory: paths.working_directory.clone(),
        arguments: arguments.clone(),
        environment: key_environment.clone(),
        inputs: inputs
            .iter()
            .map(|input| ActionInput {
                path: input.logical_path.clone(),
                sha256: input.sha256.clone(),
                size_bytes: input.size_bytes,
            })
            .collect(),
        outputs: paths.outputs.clone(),
    };
    let key = action_key(&deterministic_action)?;
    let cache_eligibility = RemoteEligibility::from_reasons(eligibility_reasons.clone());
    if invocation.requires_native_linker() {
        eligibility_reasons.push(
            "link action input discovery is incomplete; native libraries, linker binaries, response files, and platform SDK inputs must be declared"
                .to_owned(),
        );
    }

    let relocation_mappings = roots.relocation_mappings();
    let output_files = prepare_outputs(
        invocation,
        paths.outputs,
        paths.output_files,
        &relocation_mappings,
    )?;

    Ok(PreparedInvocation {
        action_key: key,
        toolchain_sha256: toolchain.sha256.clone(),
        platform_os: platform.os,
        platform_arch: platform.arch,
        compiler: display(&invocation.compiler),
        toolchain,
        platform,
        remote_eligibility: RemoteEligibility::from_reasons(eligibility_reasons),
        cache_eligibility,
        working_directory: paths.working_directory,
        crate_name: invocation.crate_name().map(lossy),
        arguments,
        environment,
        keyed_environment: key_environment,
        stripped_environment: compile_environment.stripped(),
        dep_info: invocation.dep_info_file().and_then(|path| {
            let actual_path = absolute(&path, &invocation.cwd);
            roots
                .normalize(&actual_path, &invocation.cwd)
                .ok()
                .map(|logical_path| PreparedOutput {
                    logical_path,
                    actual_path,
                })
        }),
        inputs,
        output_directory: invocation
            .out_dir()
            .as_deref()
            .map(|path| roots.normalize_text(&display(path))),
        output_files,
        path_mappings: roots.path_mappings(),
        relocation_mappings,
    })
}

fn prepare_paths(
    invocation: &RustcInvocation,
    roots: &CaptureRoots,
    eligibility_reasons: &mut Vec<String>,
) -> Result<PreparedPaths> {
    let output_files = invocation.output_files()?;
    let outputs = output_files
        .iter()
        .map(|path| roots.normalize(path, &invocation.cwd))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|error| {
            eligibility_reasons.push(error.to_string());
            Vec::new()
        });
    if outputs.is_empty() {
        eligibility_reasons.push("action has no declared outputs".to_owned());
    }
    let working_directory = roots
        .normalize(&invocation.cwd, &invocation.cwd)
        .unwrap_or_else(|error| {
            eligibility_reasons.push(error.to_string());
            display(&invocation.cwd)
        });
    Ok(PreparedPaths {
        output_files,
        outputs,
        working_directory,
    })
}

/// Pairs each output with its logical path and records the mappings it will
/// be published with. Cargo starts a pipelined dependent as soon as the
/// compiler writes this compile's metadata, before it is published, so a
/// reader must find those mappings before the compiler starts.
fn prepare_outputs(
    invocation: &RustcInvocation,
    logical_paths: Vec<String>,
    actual_paths: Vec<PathBuf>,
    path_mappings: &[(String, String)],
) -> Result<Vec<PreparedOutput>> {
    let outputs: Vec<PreparedOutput> = logical_paths
        .into_iter()
        .zip(actual_paths)
        .map(|(logical_path, actual_path)| PreparedOutput {
            logical_path,
            actual_path: absolute(&actual_path, &invocation.cwd),
        })
        .collect();

    if should_record_path_mappings(invocation) {
        for output in &outputs {
            record_output_mappings(&output.actual_path, path_mappings)?;
        }
    }
    Ok(outputs)
}

fn should_record_path_mappings(invocation: &RustcInvocation) -> bool {
    invocation.crate_name().is_some() && invocation.out_dir().is_some()
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
        cache_eligibility: prepared.cache_eligibility.clone(),
        working_directory: prepared.working_directory.clone(),
        crate_name: prepared.crate_name.clone(),
        arguments: prepared.arguments.clone(),
        environment: prepared.environment.clone(),
        keyed_environment: prepared.keyed_environment.clone(),
        stripped_environment: prepared.stripped_environment.clone(),
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
        resource_capacity: if env::var_os("CARGO_REAPI_RESOURCE_LEDGER").is_some() {
            Some(ResourceCapacity::from_env()?)
        } else {
            None
        },
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
        let compiler_for_roots = resolved_identity_compiler(&invocation.compiler, &invocation.cwd)?;
        let toolchain = compiler_for_roots
            .parent()
            .and_then(Path::parent)
            .context("compiler path has no toolchain root")?
            .to_path_buf();
        let package = env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .map(|path| absolute(&path, &invocation.cwd))
            .filter(|package| {
                invocation.args.iter().any(|argument| {
                    let path = PathBuf::from(argument);
                    (path.is_absolute() || path.components().count() > 1)
                        && absolute(&path, &invocation.cwd).starts_with(package)
                })
            });
        Ok(Self {
            workspace: absolute(&workspace, &invocation.cwd),
            package,
            target: absolute(&target, &invocation.cwd),
            toolchain: absolute(&toolchain, &invocation.cwd),
        })
    }

    /// The actual path a logical path names, the inverse of [`Self::normalize`].
    fn denormalize(&self, logical: &str) -> Option<PathBuf> {
        let (label, relative) = logical.split_once('/').unwrap_or((logical, ""));
        let root = match label {
            "target" => Some(&self.target),
            "package" => self.package.as_ref(),
            "workspace" => Some(&self.workspace),
            "toolchain" => Some(&self.toolchain),
            _ => None,
        }?;
        Some(if relative.is_empty() {
            root.clone()
        } else {
            root.join(relative)
        })
    }

    /// `path` under its root's label with nothing resolved: `..` and links
    /// stay as written.
    fn label_unresolved(&self, path: &Path) -> Result<String> {
        for (root, label) in [
            (Some(&self.target), "target"),
            (self.package.as_ref(), "package"),
            (Some(&self.workspace), "workspace"),
            (Some(&self.toolchain), "toolchain"),
        ] {
            if let Some(root) = root
                && let Ok(relative) = path.strip_prefix(root)
            {
                return Ok(logical_path(label, relative));
            }
        }
        anyhow::bail!(
            "path is outside declared package, workspace, target, and toolchain roots: {}",
            path.display()
        )
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

    /// Arguments and environment values as keyed: paths beneath the target,
    /// workspace and toolchain roots become those roots' labels, exactly as
    /// artifacts are relocated. A package outside the workspace keeps its
    /// absolute path, because its artifacts keep it too.
    fn normalize_text(&self, value: &str) -> String {
        logical_text(value, &self.relocation_mappings())
    }

    /// The roots artifacts are relocated against. The compile's own package
    /// is not among them: inside the workspace it is a path beneath the
    /// workspace root, like every workspace dependency whose paths reach
    /// this compile's artifacts, and outside it, it is the same path in
    /// every worktree.
    fn relocation_mappings(&self) -> Vec<(String, String)> {
        [
            (&self.target, "target"),
            (&self.workspace, "workspace"),
            (&self.toolchain, "toolchain"),
        ]
        .into_iter()
        .filter_map(|(root, label)| {
            root.to_str()
                .map(|root| (label.to_owned(), root.to_owned()))
        })
        .collect()
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
    let audit_observer = env::var_os("CARGO_REAPI_REAL_RUSTC")
        .and_then(|_| env::var_os("RUSTC"))
        .map(PathBuf::from);
    for (index, arg) in invocation.args.iter().enumerate() {
        let path = PathBuf::from(arg);
        if path.is_file() && audit_observer.as_ref() != Some(&path) {
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

    let mut input_roots = Vec::new();
    if let Some(package) = &roots.package {
        input_roots.push(("CARGO_MANIFEST_DIR", package.clone()));
    }
    if let Some(out_dir) = env::var_os("OUT_DIR").map(PathBuf::from) {
        input_roots.push(("OUT_DIR", out_dir));
    }
    for (name, input_root) in input_roots {
        for entry in WalkDir::new(input_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| name == "OUT_DIR" || !is_ignored(entry.path(), &roots.target))
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
            Ok(logical_path) => inputs.push(digest_file(
                &path,
                logical_path,
                &invocation.cwd,
                &roots.relocation_mappings(),
            )?),
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

/// Whether the walk over a package's files skips `path`: Cargo's target
/// directory itself, which is output, and any entry named `.git`, which the
/// strict sandbox denies reads of. No other name is skipped.
fn is_ignored(path: &Path, target: &Path) -> bool {
    path.starts_with(target) || path.file_name().is_some_and(|name| name == ".git")
}

fn digest_file(
    path: &Path,
    logical_path: String,
    cwd: &Path,
    mappings: &[(String, String)],
) -> Result<PreparedInput> {
    let actual_path = absolute(path, cwd);
    let (sha256, size_bytes) = logical_digest(&actual_path, mappings)?;
    Ok(PreparedInput {
        logical_path,
        actual_path,
        sha256,
        size_bytes,
    })
}

fn toolchain_identity(compiler: &Path) -> Result<ToolchainIdentity> {
    let identity_compiler = resolved_identity_compiler(compiler, &env::current_dir()?)?;
    let bytes = fs::read(&identity_compiler).with_context(|| {
        format!(
            "reading compiler executable {}",
            identity_compiler.display()
        )
    })?;
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

fn resolved_identity_compiler(compiler: &Path, cwd: &Path) -> Result<PathBuf> {
    if let Some(real) = env::var_os("CARGO_REAPI_REAL_RUSTC").map(PathBuf::from) {
        return real
            .canonicalize()
            .with_context(|| format!("resolving real compiler {}", real.display()));
    }
    let candidate = if compiler.is_absolute() || compiler.components().count() > 1 {
        absolute(compiler, cwd)
    } else {
        env::var_os("PATH")
            .and_then(|path| {
                env::split_paths(&path)
                    .map(|directory| directory.join(compiler))
                    .find(|candidate| candidate.is_file())
            })
            .with_context(|| format!("resolving compiler {} through PATH", compiler.display()))?
    };
    let candidate = candidate
        .canonicalize()
        .with_context(|| format!("canonicalizing compiler {}", candidate.display()))?;
    if candidate.file_name().is_some_and(|name| name == "rustup") {
        let output = Command::new(&candidate)
            .args(["which", "rustc"])
            .output()
            .context("resolving the active rustc through rustup")?;
        if !output.status.success() {
            anyhow::bail!(
                "rustup could not resolve the active rustc: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let rustc = PathBuf::from(String::from_utf8(output.stdout)?.trim());
        return rustc
            .canonicalize()
            .with_context(|| format!("canonicalizing rustup compiler {}", rustc.display()));
    }
    Ok(candidate)
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
        || name.starts_with("CARGO_BIN_EXE_")
        || name.starts_with("CARGO_FEATURE_")
        || name.starts_with("CARGO_PKG_")
        || name.starts_with("CARGO_TARGET_")
        || matches!(
            name,
            "CARGO"
                | "CARGO_BIN_NAME"
                | "CARGO_CRATE_NAME"
                | "CARGO_ENCODED_RUSTFLAGS"
                | "CARGO_MANIFEST_DIR"
                | "CARGO_MANIFEST_PATH"
                | "CARGO_PRIMARY_PACKAGE"
                | "CARGO_TARGET_TMPDIR"
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
    crate::safe_io::append_line(path, &line)
        .with_context(|| format!("writing action log {}", path.display()))
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn lossy(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{dep_info_paths, is_compiler_environment, should_record_path_mappings};
    use crate::invocation::RustcInvocation;

    fn invocation(arguments: &[&str]) -> RustcInvocation {
        RustcInvocation {
            compiler: PathBuf::from("rustc"),
            args: arguments.iter().map(Into::into).collect(),
            cwd: PathBuf::from("/workspace"),
        }
    }

    #[test]
    fn records_restore_metadata_only_for_artifact_producing_compilations() {
        assert!(!should_record_path_mappings(&invocation(&["-vV"])));
        assert!(!should_record_path_mappings(&invocation(&[
            "--crate-name",
            "probe"
        ])));
        assert!(should_record_path_mappings(&invocation(&[
            "--crate-name",
            "leaf",
            "--out-dir",
            "/workspace/target/debug/deps",
        ])));
    }

    #[test]
    fn dep_info_escapes_only_spaces() {
        let text = "out/a.d: src/lib.rs src/with\\ space.rs shared/a\\b.txt\n\
                    \n\
                    src/lib.rs:\n\
                    src/with\\ space.rs:\n\
                    shared/a\\b.txt:\n\
                    # env-dep:NAME=value\n";
        let paths = dep_info_paths(text).into_iter().collect::<Vec<_>>();
        assert_eq!(
            paths,
            ["shared/a\\b.txt", "src/lib.rs", "src/with space.rs"]
        );
    }

    #[test]
    fn captures_cargo_compiler_contract() {
        assert!(is_compiler_environment("CARGO_PKG_VERSION"));
        assert!(is_compiler_environment("CARGO_CFG_TARGET_OS"));
        assert!(is_compiler_environment(
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER"
        ));
        assert!(is_compiler_environment("CARGO_BIN_EXE_companion"));
        assert!(is_compiler_environment("CARGO_TARGET_TMPDIR"));
        assert!(is_compiler_environment("CARGO"));
        assert!(is_compiler_environment("OUT_DIR"));
    }

    #[test]
    fn never_captures_registry_credentials() {
        assert!(!is_compiler_environment("CARGO_REGISTRY_TOKEN"));
        assert!(!is_compiler_environment("CARGO_REGISTRIES_PRIVATE_TOKEN"));
        assert!(!is_compiler_environment("AWS_SECRET_ACCESS_KEY"));
    }
}
