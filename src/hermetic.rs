use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

pub const SRT_REQUIRED_PACKAGE: &str = "@anthropic-ai/sandbox-runtime";
pub const SRT_REQUIRED_VERSION: &str = "0.0.66";

// srt injects ephemeral proxy ports and credentials even when the policy's
// domain allowlist is empty. They are implementation plumbing, unavailable for
// useful network access, and must not leak nondeterminism into proc macros or
// build scripts. Execute Cargo through `env -u` inside the sandbox so strict
// builds observe a stable, networkless environment.
const SRT_EPHEMERAL_ENVIRONMENT: &[&str] = &[
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "GRPC_PROXY",
    "grpc_proxy",
    "GIT_CONFIG_PARAMETERS",
    "GIT_SSH_COMMAND",
    "FTP_PROXY",
    "ftp_proxy",
    "RSYNC_PROXY",
    "DOCKER_HTTP_PROXY",
    "DOCKER_HTTPS_PROXY",
    "CLOUDSDK_PROXY_TYPE",
    "CLOUDSDK_PROXY_ADDRESS",
    "CLOUDSDK_PROXY_PORT",
    "CLOUDSDK_PROXY_USERNAME",
    "CLOUDSDK_PROXY_PASSWORD",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotPolicy {
    Strict,
    Off,
}

pub struct HermeticCargo {
    pub command: Command,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SrtPolicy {
    network: SrtNetworkPolicy,
    filesystem: SrtFilesystemPolicy,
    enable_weaker_nested_sandbox: bool,
    enable_weaker_network_isolation: bool,
    allow_apple_events: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SrtNetworkPolicy {
    allowed_domains: Vec<String>,
    denied_domains: Vec<String>,
    allow_local_binding: bool,
    allow_unix_sockets: Vec<String>,
    allow_all_unix_sockets: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SrtFilesystemPolicy {
    deny_read: Vec<String>,
    allow_read: Vec<String>,
    allow_write: Vec<String>,
    deny_write: Vec<String>,
}

#[derive(Debug)]
struct SrtInstallation {
    executable: PathBuf,
    package_root: PathBuf,
}

pub fn cargo_command(
    policy: SnapshotPolicy,
    workspace: &Path,
    target: &Path,
    cache: &Path,
    action_log: &Path,
    explicit_inputs: &[PathBuf],
) -> Result<HermeticCargo> {
    match policy {
        SnapshotPolicy::Off => Ok(HermeticCargo {
            command: Command::new("cargo"),
        }),
        SnapshotPolicy::Strict => {
            strict_cargo_command(workspace, target, cache, action_log, explicit_inputs)
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn strict_cargo_command(
    workspace: &Path,
    target: &Path,
    cache: &Path,
    action_log: &Path,
    explicit_inputs: &[PathBuf],
) -> Result<HermeticCargo> {
    let workspace = canonical_or_absolute(workspace)?;
    let target = create_and_canonicalize(target, "target")?;
    let cache = create_and_canonicalize(cache, "cache")?;
    let action_log = create_file_and_canonicalize(action_log, "action log")?;
    let temporary = create_and_canonicalize(
        &target.join("cargo-reapi/hermetic-tmp"),
        "hermetic temporary directory",
    )?;
    let control_temporary = srt_control_directory()?;
    let installation = resolve_srt_installation()?;
    let policy = build_policy(
        &workspace,
        &target,
        &cache,
        &action_log,
        &temporary,
        &control_temporary,
        explicit_inputs,
    )?;
    let policy_path = target.join("cargo-reapi/srt-policy-v1.json");
    fs::write(&policy_path, canonical_policy_bytes(&policy)?)
        .with_context(|| format!("writing srt policy {}", policy_path.display()))?;

    let cargo = resolve_toolchain_executable("cargo")
        .context("resolving the active toolchain Cargo for strict execution")?;
    let rustc = resolve_toolchain_executable("rustc")
        .context("resolving the active toolchain rustc for strict execution")?;
    let rustdoc = resolve_toolchain_executable("rustdoc")
        .context("resolving the active toolchain rustdoc for strict execution")?;
    let toolchain_bin = cargo
        .parent()
        .context("active toolchain Cargo has no bin directory")?;
    let mut command_path = vec![toolchain_bin.to_path_buf()];
    if let Some(path) = env::var_os("PATH") {
        command_path.extend(env::split_paths(&path).filter(|entry| entry != toolchain_bin));
    }
    let command_path =
        env::join_paths(command_path).context("constructing strict toolchain PATH")?;
    let env_executable = resolve_executable("env")
        .context("resolving env used to remove srt's ephemeral proxy environment")?;
    let mut command = Command::new(installation.executable);
    command
        .arg("--settings")
        .arg(policy_path)
        .arg("--")
        .arg(env_executable);
    for name in SRT_EPHEMERAL_ENVIRONMENT {
        command.arg("-u").arg(name);
    }
    command
        .arg(cargo)
        .env("CARGO_NET_OFFLINE", "true")
        .env("PATH", command_path)
        // Keep srt's private mux socket out of the snapshotted, stable child
        // TMPDIR. A per-driver directory also prevents a crashed/reused srt PID
        // from colliding with stale provider plumbing.
        .env("TMPDIR", control_temporary)
        // srt deliberately replaces TMPDIR for sandboxed children. Point its
        // documented override at our declared, writable target directory.
        .env("CLAUDE_CODE_TMPDIR", temporary);
    if env::var_os("RUSTDOC").is_none() {
        command.env("RUSTDOC", rustdoc);
    }
    if env::var_os("RUSTC").is_none() {
        command.env("RUSTC", rustc);
    }
    Ok(HermeticCargo { command })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn strict_cargo_command(
    _workspace: &Path,
    _target: &Path,
    _cache: &Path,
    _action_log: &Path,
    _explicit_inputs: &[PathBuf],
) -> Result<HermeticCargo> {
    bail!(
        "strict whole-gate snapshots support macOS and Linux through pinned Anthropic Sandbox Runtime {SRT_REQUIRED_VERSION}; use --snapshot-policy off to run without whole-gate restoration"
    )
}

pub fn provider_identity_digest() -> Result<String> {
    let installation = resolve_srt_installation()?;
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, SRT_REQUIRED_PACKAGE.as_bytes());
    hash_field(&mut hasher, SRT_REQUIRED_VERSION.as_bytes());
    hash_node_package_closure(&mut hasher, &installation.package_root)?;
    let node = resolve_executable("node").context("resolving Node.js used by srt")?;
    hash_executable(&mut hasher, "node", &node)?;
    let ripgrep = resolve_executable("rg")
        .context("strict snapshots require ripgrep for srt's mandatory-deny traversal")?;
    hash_executable(&mut hasher, "rg", &ripgrep)?;
    #[cfg(target_os = "macos")]
    {
        let sandbox_exec = PathBuf::from("/usr/bin/sandbox-exec");
        if !sandbox_exec.is_file() {
            bail!("strict snapshots require macOS sandbox-exec");
        }
        hash_executable(&mut hasher, "sandbox-exec", &sandbox_exec)?;
    }
    #[cfg(target_os = "linux")]
    {
        for tool in ["bwrap", "socat"] {
            let executable = resolve_executable(tool)
                .with_context(|| format!("strict snapshots require {tool} for srt on Linux"))?;
            hash_executable(&mut hasher, tool, &executable)?;
        }
        let seccomp_arch = match env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            architecture => bail!(
                "strict snapshots require srt Unix-socket seccomp support; unsupported Linux architecture {architecture}"
            ),
        };
        let seccomp = installation
            .package_root
            .join("vendor/seccomp")
            .join(seccomp_arch)
            .join("apply-seccomp");
        if !seccomp.is_file() {
            bail!(
                "strict snapshots require srt's bundled apply-seccomp helper at {}",
                seccomp.display()
            );
        }
        hash_executable(&mut hasher, "apply-seccomp", &seccomp)?;
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn policy_identity_bytes(
    workspace: &Path,
    target: &Path,
    cache: &Path,
    action_log: &Path,
    explicit_inputs: &[PathBuf],
) -> Result<Vec<u8>> {
    let workspace = canonical_or_absolute(workspace)?;
    let target = absolute(target)?;
    let cache = canonical_or_absolute(cache)?;
    let action_log = absolute(action_log)?;
    let temporary = target.join("cargo-reapi/hermetic-tmp");
    let control_temporary = srt_control_directory()?;
    let policy = build_policy(
        &workspace,
        &target,
        &cache,
        &action_log,
        &temporary,
        &control_temporary,
        explicit_inputs,
    )?;
    normalize_policy_identity_bytes(
        canonical_policy_bytes(&policy)?,
        &control_temporary,
        env::var_os("CARGO_REAPI_RUSTC_TRACE")
            .map(PathBuf::from)
            .as_deref(),
        &workspace,
        &target,
        &cache,
        &action_log,
    )
}

fn normalize_policy_identity_bytes(
    bytes: Vec<u8>,
    control_temporary: &Path,
    rustc_trace: Option<&Path>,
    workspace: &Path,
    target: &Path,
    cache: &Path,
    action_log: &Path,
) -> Result<Vec<u8>> {
    let mut text = String::from_utf8(bytes)?;
    let mut mappings = vec![
        (control_temporary.to_path_buf(), "<srt-control>"),
        (workspace.to_path_buf(), "<workspace>"),
        (target.to_path_buf(), "<target>"),
        (cache.to_path_buf(), "<cache>"),
        (action_log.to_path_buf(), "<action-log>"),
    ];
    if let Some(trace) = rustc_trace {
        mappings.push((canonical_or_absolute(trace)?, "<rustc-trace>"));
    }
    mappings.sort_by_key(|(path, _)| std::cmp::Reverse(path.as_os_str().len()));
    for (path, replacement) in mappings {
        text = text.replace(&path.to_string_lossy().to_string(), replacement);
    }
    let mut policy: serde_json::Value = serde_json::from_str(&text)?;
    sort_string_arrays(&mut policy);
    Ok(serde_json::to_vec_pretty(&policy)?)
}

fn sort_string_arrays(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values.iter_mut() {
                sort_string_arrays(value);
            }
            if values.iter().all(serde_json::Value::is_string) {
                values.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
            }
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values_mut() {
                sort_string_arrays(value);
            }
        }
        _ => {}
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn build_policy(
    workspace: &Path,
    target: &Path,
    cache: &Path,
    action_log: &Path,
    temporary: &Path,
    control_temporary: &Path,
    explicit_inputs: &[PathBuf],
) -> Result<SrtPolicy> {
    let mut readable = package_roots(workspace, cache)?;
    #[cfg(target_os = "linux")]
    readable.extend(workspace_inputs_excluding_target(workspace, target)?);
    #[cfg(not(target_os = "linux"))]
    readable.insert(workspace.to_path_buf());
    readable.extend([
        target.to_path_buf(),
        cache.to_path_buf(),
        action_log.to_path_buf(),
        temporary.to_path_buf(),
    ]);
    readable.extend(cargo_configuration_files(workspace));

    for system_root in ["/System", "/usr", "/bin", "/sbin", "/dev"] {
        let path = PathBuf::from(system_root);
        if path.exists() {
            readable.insert(path);
        }
    }
    #[cfg(target_os = "macos")]
    for system_input in [
        "/Library/Developer",
        "/private/etc/ssl/openssl.cnf",
        "/private/var/select/sh",
        // Seatbelt evaluates the symlink vnode before the canonical /private
        // target. These entries permit traversal of the link itself; the
        // destination remains governed by its separately declared paths.
        "/tmp",
        "/var",
    ] {
        let path = PathBuf::from(system_input);
        if path.exists() {
            readable.insert(path);
        }
    }
    #[cfg(target_os = "macos")]
    {
        // Apple's xcrun/cc frontends insist on consulting this cache even when
        // TMPDIR is redirected. Limit the exception to xcrun's cache basename;
        // the rest of the host temporary directory remains unreadable.
        let host_temporary = env::temp_dir();
        readable.insert(host_temporary.join("xcrun_db"));
        readable.insert(host_temporary.join("xcrun_db-*"));
        if let Ok(canonical) = host_temporary.canonicalize() {
            readable.insert(canonical.join("xcrun_db"));
            readable.insert(canonical.join("xcrun_db-*"));
        }
    }

    if let Some(cargo_home) = cargo_home() {
        let cargo_home = canonical_or_absolute(&cargo_home)?;
        for relative in ["registry", "git", "config", "config.toml"] {
            let path = cargo_home.join(relative);
            if path.exists() {
                readable.insert(path);
            }
        }
    }
    for tool in ["cargo", "rustc", "rustdoc"] {
        if let Some(executable) = resolve_toolchain_executable(tool) {
            readable.insert(executable.clone());
            if let Some(toolchain) = executable.parent().and_then(Path::parent) {
                readable.insert(toolchain.to_path_buf());
            }
        }
    }
    for variable in ["RUSTC", "RUSTDOC"] {
        if let Some(configured_tool) = env::var_os(variable).map(PathBuf::from) {
            let configured_tool = if configured_tool.is_absolute() {
                configured_tool
            } else {
                workspace.join(configured_tool)
            };
            if configured_tool.is_file() {
                readable.insert(canonical_or_absolute(&configured_tool)?);
            }
        }
    }
    if let Ok(executable) = env::current_exe() {
        readable.insert(executable);
    }
    if let Some(trace) = env::var_os("CARGO_REAPI_RUSTC_TRACE").map(PathBuf::from) {
        readable.insert(canonical_or_absolute(&trace)?);
    }
    for input in explicit_inputs {
        let input = if input.is_absolute() {
            input.clone()
        } else {
            workspace.join(input)
        };
        if !input.exists() {
            bail!("declared input does not exist: {}", input.display());
        }
        readable.insert(canonical_or_absolute(&input)?);
    }

    let mut writable = BTreeSet::from([
        target.to_path_buf(),
        cache.to_path_buf(),
        action_log.to_path_buf(),
        temporary.to_path_buf(),
    ]);
    #[cfg(target_os = "macos")]
    {
        let host_temporary = env::temp_dir();
        writable.insert(host_temporary.join("xcrun_db"));
        writable.insert(host_temporary.join("xcrun_db-*"));
        if let Ok(canonical) = host_temporary.canonicalize() {
            writable.insert(canonical.join("xcrun_db"));
            writable.insert(canonical.join("xcrun_db-*"));
        }
    }
    if let Some(trace) = env::var_os("CARGO_REAPI_RUSTC_TRACE").map(PathBuf::from) {
        writable.insert(canonical_or_absolute(&trace)?);
    }
    if let Some(cargo_home) = cargo_home() {
        let cargo_home = canonical_or_absolute(&cargo_home)?;
        writable.insert(cargo_home.join(".package-cache"));
        writable.insert(cargo_home.join(".package-cache-mutate"));
    }

    Ok(SrtPolicy {
        network: SrtNetworkPolicy {
            allowed_domains: Vec::new(),
            denied_domains: Vec::new(),
            allow_local_binding: false,
            // srt's network-deny mux uses a private Unix socket beneath its
            // TMPDIR. No host or service socket is exposed to the build.
            allow_unix_sockets: vec![control_temporary.to_string_lossy().into_owned()],
            // Linux bubblewrap creates a private network namespace and the
            // filesystem policy hides host service sockets. Avoid srt's
            // second, capability-bearing user namespace: it cannot be nested
            // after bubblewrap's mandatory `--cap-drop ALL` on stock kernels.
            // The qualification container adds an independent `--network
            // none` boundary and exposes no host Unix sockets.
            #[cfg(target_os = "linux")]
            allow_all_unix_sockets: true,
            #[cfg(not(target_os = "linux"))]
            allow_all_unix_sockets: false,
        },
        filesystem: SrtFilesystemPolicy {
            deny_read: vec!["/".to_owned()],
            allow_read: paths_to_strings(readable),
            allow_write: paths_to_strings(writable),
            deny_write: Vec::new(),
        },
        enable_weaker_nested_sandbox: false,
        enable_weaker_network_isolation: false,
        allow_apple_events: false,
    })
}

#[cfg(target_os = "linux")]
fn workspace_inputs_excluding_target(workspace: &Path, target: &Path) -> Result<BTreeSet<PathBuf>> {
    let Ok(relative_target) = target.strip_prefix(workspace) else {
        return Ok(BTreeSet::from([workspace.to_path_buf()]));
    };
    if relative_target.as_os_str().is_empty() {
        return Ok(BTreeSet::new());
    }

    // SRT emits writable binds before read-only binds on Linux. A read-only
    // bind of the workspace would therefore shadow a writable target nested
    // beneath it. Bind every sibling on the route to the target read-only,
    // leaving the target itself to the writable binding.
    let mut readable = BTreeSet::new();
    let mut current = workspace.to_path_buf();
    for component in relative_target.components() {
        let next = current.join(component.as_os_str());
        for entry in fs::read_dir(&current)
            .with_context(|| format!("enumerating workspace input {}", current.display()))?
        {
            let path = entry?.path();
            if path != next {
                readable.insert(path);
            }
        }
        current = next;
    }
    Ok(readable)
}

fn paths_to_strings(paths: BTreeSet<PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn canonical_policy_bytes(policy: &SrtPolicy) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(policy)?)
}

fn resolve_srt_installation() -> Result<SrtInstallation> {
    let executable = env::var_os("CARGO_REAPI_SRT")
        .map(PathBuf::from)
        .or_else(|| resolve_executable("srt"))
        .context(
            "strict snapshots require Anthropic Sandbox Runtime; install @anthropic-ai/sandbox-runtime@0.0.66 or set CARGO_REAPI_SRT",
        )?;
    let executable = fs::canonicalize(&executable)
        .with_context(|| format!("resolving srt executable {}", executable.display()))?;
    let package_root = executable
        .ancestors()
        .find(|directory| {
            fs::read(directory.join("package.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|package| {
                    package["name"] == SRT_REQUIRED_PACKAGE
                        && package["version"] == SRT_REQUIRED_VERSION
                })
        })
        .map(Path::to_path_buf)
        .with_context(|| {
            format!(
                "{} is not {SRT_REQUIRED_PACKAGE}@{SRT_REQUIRED_VERSION}; refusing an unpinned sandbox provider",
                executable.display()
            )
        })?;
    Ok(SrtInstallation {
        executable,
        package_root,
    })
}

fn hash_tree(hasher: &mut Sha256, root: &Path) -> Result<()> {
    let mut entries = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.path().to_path_buf());
    for entry in entries {
        if entry.path() == root || entry.file_type().is_dir() {
            continue;
        }
        let relative = entry.path().strip_prefix(root)?;
        hash_field(hasher, relative.to_string_lossy().as_bytes());
        if entry.file_type().is_symlink() {
            hash_field(
                hasher,
                fs::read_link(entry.path())?.to_string_lossy().as_bytes(),
            );
        } else if entry.file_type().is_file() {
            hash_field(hasher, &fs::read(entry.path())?);
        }
    }
    Ok(())
}

fn hash_node_package_closure(hasher: &mut Sha256, root: &Path) -> Result<()> {
    fn visit(hasher: &mut Sha256, root: &Path, visited: &mut BTreeSet<PathBuf>) -> Result<()> {
        let root = root.canonicalize()?;
        if !visited.insert(root.clone()) {
            return Ok(());
        }
        let package_bytes = fs::read(root.join("package.json"))
            .with_context(|| format!("reading Node package identity {}", root.display()))?;
        let package: serde_json::Value = serde_json::from_slice(&package_bytes)?;
        let name = package["name"]
            .as_str()
            .context("Node dependency has no package name")?;
        let version = package["version"]
            .as_str()
            .context("Node dependency has no package version")?;
        hash_field(hasher, name.as_bytes());
        hash_field(hasher, version.as_bytes());
        hash_tree(hasher, &root)?;

        let mut dependencies = BTreeSet::new();
        for field in ["dependencies", "optionalDependencies"] {
            if let Some(values) = package[field].as_object() {
                dependencies.extend(values.keys().cloned());
            }
        }
        for dependency in dependencies {
            let dependency_root =
                resolve_node_dependency(&root, &dependency).with_context(|| {
                    format!("resolving runtime dependency {dependency} of {name}@{version}")
                })?;
            visit(hasher, &dependency_root, visited)?;
        }
        Ok(())
    }

    visit(hasher, root, &mut BTreeSet::new())
}

fn resolve_node_dependency(package_root: &Path, name: &str) -> Option<PathBuf> {
    let nested = package_root.join("node_modules").join(name);
    if nested.join("package.json").is_file() {
        return Some(nested);
    }
    package_root
        .ancestors()
        .filter(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|part| part == "node_modules")
        })
        .map(|node_modules| node_modules.join(name))
        .find(|candidate| candidate.join("package.json").is_file())
}

fn hash_executable(hasher: &mut Sha256, label: &str, executable: &Path) -> Result<()> {
    hash_field(hasher, label.as_bytes());
    hash_field(
        hasher,
        &fs::read(executable)
            .with_context(|| format!("hashing {label} executable {}", executable.display()))?,
    );
    Ok(())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_le_bytes());
    hasher.update(value);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn package_roots(workspace: &Path, cache: &Path) -> Result<BTreeSet<PathBuf>> {
    let output = crate::query::cargo_metadata_output(workspace, cache, &["--offline"])
        .context("running cargo metadata for the strict snapshot policy")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed for the strict snapshot policy: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let mut roots = BTreeSet::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        if let Some(manifest) = package["manifest_path"].as_str()
            && let Some(parent) = Path::new(manifest).parent()
        {
            roots.insert(canonical_or_absolute(parent)?);
        }
    }
    Ok(roots)
}

fn cargo_home() -> Option<PathBuf> {
    env::var_os("CARGO_HOME").map(PathBuf::from).or_else(|| {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".cargo"))
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn cargo_configuration_files(workspace: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    for directory in workspace.ancestors() {
        for name in ["config", "config.toml"] {
            let path = directory.join(".cargo").join(name);
            if path.is_file() {
                files.insert(path);
            }
        }
    }
    if let Some(home) = cargo_home() {
        for name in ["config", "config.toml"] {
            let path = home.join(name);
            if path.is_file() {
                files.insert(path);
            }
        }
    }
    files
}

fn resolve_toolchain_executable(name: &str) -> Option<PathBuf> {
    let candidate = resolve_executable(name)?;
    if candidate.file_name().is_some_and(|file| file == "rustup") {
        let output = Command::new(&candidate)
            .args(["which", name])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        return PathBuf::from(String::from_utf8(output.stdout).ok()?.trim())
            .canonicalize()
            .ok();
    }
    Some(candidate)
}

fn resolve_executable(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 && candidate.is_file() {
        return fs::canonicalize(candidate).ok();
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|root| root.join(name))
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| fs::canonicalize(candidate).ok())
    })
}

fn create_and_canonicalize(path: &Path, label: &str) -> Result<PathBuf> {
    let path = absolute(path)?;
    fs::create_dir_all(&path).with_context(|| format!("creating {label} {}", path.display()))?;
    canonical_or_absolute(&path)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn srt_control_directory() -> Result<PathBuf> {
    // Unix-domain socket paths are short (104 bytes on macOS). A worktree
    // target can exceed that before srt appends its PID suffix, causing two
    // different proxy sockets to truncate to the same kernel address.
    // Provider-only plumbing therefore uses a fixed short directory while the
    // sandboxed child retains its target-local hermetic TMPDIR.
    let path = PathBuf::from("/tmp/cargo-reapi-srt");
    if path.exists() && fs::symlink_metadata(&path)?.file_type().is_symlink() {
        bail!(
            "refusing symlinked srt control directory {}",
            path.display()
        );
    }
    create_and_canonicalize(&path, "srt control temporary directory")
}

fn create_file_and_canonicalize(path: &Path, label: &str) -> Result<PathBuf> {
    let path = absolute(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {label} parent {}", parent.display()))?;
    }
    if !path.exists() {
        fs::write(&path, b"").with_context(|| format!("creating {label} {}", path.display()))?;
    }
    canonical_or_absolute(&path)
}

fn canonical_or_absolute(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", path.display()));
    }
    absolute(path)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::workspace_inputs_excluding_target;
    use super::{
        SrtFilesystemPolicy, SrtNetworkPolicy, SrtPolicy, canonical_policy_bytes,
        normalize_policy_identity_bytes,
    };
    use std::path::Path;

    #[test]
    fn strict_policy_is_fail_closed() {
        let policy = SrtPolicy {
            network: SrtNetworkPolicy {
                allowed_domains: Vec::new(),
                denied_domains: Vec::new(),
                allow_local_binding: false,
                allow_unix_sockets: Vec::new(),
                allow_all_unix_sockets: false,
            },
            filesystem: SrtFilesystemPolicy {
                deny_read: vec!["/".to_owned()],
                allow_read: vec!["/workspace".to_owned()],
                allow_write: vec!["/workspace/target".to_owned()],
                deny_write: Vec::new(),
            },
            enable_weaker_nested_sandbox: false,
            enable_weaker_network_isolation: false,
            allow_apple_events: false,
        };
        let value: serde_json::Value =
            serde_json::from_slice(&canonical_policy_bytes(&policy).expect("policy JSON"))
                .expect("valid JSON");
        assert_eq!(value["filesystem"]["denyRead"][0], "/");
        assert_eq!(value["network"]["allowedDomains"], serde_json::json!([]));
        assert_eq!(value["filesystem"]["allowWrite"][0], "/workspace/target");
        assert_eq!(value["enableWeakerNestedSandbox"], false);
        assert_eq!(value["enableWeakerNetworkIsolation"], false);
        assert_eq!(value["allowAppleEvents"], false);
    }

    #[test]
    fn policy_identity_ignores_external_observer_output_location() {
        let first = normalize_policy_identity_bytes(
            br#"{"allowWrite":["/tmp/trace-one","/tmp/workspace/target","/tmp/workspace-evidence/actions"]}"#.to_vec(),
            Path::new("/tmp/control"),
            Some(Path::new("/tmp/trace-one")),
            Path::new("/tmp/workspace"),
            Path::new("/tmp/workspace/target"),
            Path::new("/tmp/cache"),
            Path::new("/tmp/workspace-evidence/actions"),
        )
        .unwrap();
        let second = normalize_policy_identity_bytes(
            br#"{"allowWrite":["/tmp/evidence/actions","/tmp/workspace-longer/target","/tmp/trace-two"]}"#.to_vec(),
            Path::new("/tmp/control"),
            Some(Path::new("/tmp/trace-two")),
            Path::new("/tmp/workspace-longer"),
            Path::new("/tmp/workspace-longer/target"),
            Path::new("/tmp/cache"),
            Path::new("/tmp/evidence/actions"),
        )
        .unwrap();
        assert_eq!(first, second);
        assert!(String::from_utf8(first).unwrap().contains("<rustc-trace>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_workspace_inputs_do_not_shadow_nested_writable_target() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let target = workspace.join("target");
        std::fs::create_dir_all(workspace.join("crates/leaf")).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(workspace.join("Cargo.toml"), "[workspace]\n").unwrap();

        let readable = workspace_inputs_excluding_target(&workspace, &target).unwrap();
        assert!(readable.contains(&workspace.join("Cargo.toml")));
        assert!(readable.contains(&workspace.join("crates")));
        assert!(!readable.contains(&workspace));
        assert!(!readable.iter().any(|path| target.starts_with(path)));
    }
}
