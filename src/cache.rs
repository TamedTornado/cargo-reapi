use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capture::{CaptureOptions, PreparedInvocation, prepare_invocation, record_invocation};
use crate::invocation::RustcInvocation;
use crate::relocation::{
    materialize_artifact_slots, normalize_artifact_slots, record_logical_digest,
    restored_logical_digest,
};
use crate::resource::ResourceLease;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct CacheOptions {
    root: PathBuf,
}

impl CacheOptions {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ActionManifest {
    schema_version: u32,
    action_key: String,
    outputs: Vec<CachedOutput>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CachedOutput {
    logical_path: String,
    sha256: String,
    size_bytes: u64,
    path_rewritten: bool,
    #[cfg(unix)]
    mode: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct LinkDiscovery {
    schema_version: u32,
    base_action_key: String,
    inputs: Vec<LinkInput>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LinkInput {
    logical_path: String,
    actual_path: PathBuf,
    sha256: String,
    size_bytes: u64,
    modified_unix_ns: u128,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct FileDigestMemo {
    schema_version: u32,
    actual_path: PathBuf,
    sha256: String,
    size_bytes: u64,
    modified_unix_ns: u128,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct LinkerCaptureRecord {
    pub schema_version: u32,
    pub arguments: Vec<String>,
    pub traced_inputs: Vec<String>,
}

pub fn execute_cached(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    cache_options: &CacheOptions,
) -> Result<i32> {
    let prepared = prepare_invocation(invocation)?;
    if !prepared.cache_eligibility.eligible {
        let exit_code = invocation.execute()?;
        record_invocation(capture_options, &prepared, "local-ineligible", exit_code)?;
        return Ok(exit_code);
    }

    if invocation.requires_native_linker() {
        return execute_cached_link(invocation, capture_options, cache_options, prepared);
    }

    execute_cached_regular(invocation, capture_options, cache_options, &prepared)
}

fn execute_cached_regular(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    cache_options: &CacheOptions,
    prepared: &PreparedInvocation,
) -> Result<i32> {
    let (lock, coalesced) = lock_action(cache_options, &prepared.action_key)?;

    let result = finish_cached_execution(
        invocation,
        capture_options,
        cache_options,
        prepared,
        coalesced,
    )?;

    FileExt::unlock(&lock).context("unlocking action cache entry")?;
    Ok(result)
}

fn lock_action(options: &CacheOptions, action_key: &str) -> Result<(File, bool)> {
    fs::create_dir_all(options.root.join("locks")).context("creating action lock directory")?;
    let lock_path = options
        .root
        .join("locks")
        .join(format!("{action_key}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening action lock {}", lock_path.display()))?;
    let coalesced = match lock.try_lock_exclusive() {
        Ok(()) => false,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            lock.lock_exclusive()
                .with_context(|| format!("coalescing on action {action_key}"))?;
            true
        }
        Err(error) => {
            return Err(error).with_context(|| format!("locking action {action_key}"));
        }
    };

    Ok((lock, coalesced))
}

fn finish_cached_execution(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    cache_options: &CacheOptions,
    prepared: &PreparedInvocation,
    coalesced: bool,
) -> Result<i32> {
    let result = if restore(cache_options, prepared)? {
        record_invocation(
            capture_options,
            prepared,
            if coalesced {
                "coalesced-hit"
            } else {
                "cache-hit"
            },
            0,
        )?;
        0
    } else {
        let _lease = ResourceLease::acquire(false)?;
        let exit_code = invocation.execute()?;
        let execution = if exit_code == 0 && publish(cache_options, prepared)? {
            "local-cache-miss"
        } else if exit_code == 0 {
            "local-output-incomplete"
        } else {
            "local-failed"
        };
        record_invocation(capture_options, prepared, execution, exit_code)?;
        exit_code
    };
    Ok(result)
}

fn execute_cached_link(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    cache_options: &CacheOptions,
    mut prepared: PreparedInvocation,
) -> Result<i32> {
    let base_action_key = prepared.action_key.clone();
    let (lock, coalesced) = lock_action(cache_options, &base_action_key)?;

    if let Some(discovery) = load_link_discovery(cache_options, &base_action_key, &prepared)? {
        prepared.action_key = discovered_action_key(&base_action_key, &discovery)?;
        if restore(cache_options, &prepared)? {
            record_invocation(
                capture_options,
                &prepared,
                if coalesced {
                    "coalesced-hit"
                } else {
                    "cache-hit"
                },
                0,
            )?;
            FileExt::unlock(&lock).context("unlocking linked action cache entry")?;
            return Ok(0);
        }
    }

    let capture_path = cache_options.root.join("link-captures").join(format!(
        "{}-{}-{}.jsonl",
        base_action_key,
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let wrapper = std::env::current_exe().context("locating cargo-reapi linker wrapper")?;
    let real_linker = resolve_real_linker(invocation)?;
    let _lease = ResourceLease::acquire(true)?;
    let exit_code =
        invocation.execute_with_linker_capture(&wrapper, &capture_path, &real_linker)?;
    let result = if exit_code == 0 {
        match discover_link_inputs(
            invocation,
            &prepared,
            cache_options,
            &capture_path,
            &real_linker,
        ) {
            Ok(discovery) => {
                prepared.action_key = discovered_action_key(&base_action_key, &discovery)?;
                publish_link_discovery(cache_options, &discovery)?;
                if publish(cache_options, &prepared)? {
                    record_invocation(capture_options, &prepared, "local-cache-miss", exit_code)?;
                } else {
                    record_invocation(
                        capture_options,
                        &prepared,
                        "local-output-incomplete",
                        exit_code,
                    )?;
                }
                exit_code
            }
            Err(error) => {
                prepared.cache_eligibility.eligible = false;
                prepared
                    .cache_eligibility
                    .reasons
                    .push(format!("link discovery failed closed: {error:#}"));
                record_invocation(capture_options, &prepared, "local-ineligible", exit_code)?;
                exit_code
            }
        }
    } else {
        record_invocation(capture_options, &prepared, "local-failed", exit_code)?;
        exit_code
    };
    fs::remove_file(&capture_path).ok();
    FileExt::unlock(&lock).context("unlocking linked action cache entry")?;
    Ok(result)
}

fn resolve_real_linker(invocation: &RustcInvocation) -> Result<PathBuf> {
    if let Some(candidate) = invocation.configured_linker() {
        return resolve_linker_candidate(&candidate);
    }
    if let Some(linker) = std::env::var_os("CARGO_REAPI_DEFAULT_LINKER") {
        return Ok(PathBuf::from(linker));
    }
    default_linker_for_sandbox()
}

pub fn default_linker_for_sandbox() -> Result<PathBuf> {
    let candidate = std::env::var_os("RUSTC_LINKER")
        .map_or_else(|| PathBuf::from("/usr/bin/cc"), PathBuf::from);
    resolve_linker_candidate(&candidate)
}

fn resolve_linker_candidate(candidate: &Path) -> Result<PathBuf> {
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        let output = Command::new("which")
            .arg(candidate)
            .output()
            .with_context(|| format!("resolving linker {}", candidate.display()))?;
        if !output.status.success() {
            anyhow::bail!("configured linker was not found: {}", candidate.display());
        }
        PathBuf::from(
            String::from_utf8(output.stdout)
                .context("linker path is not UTF-8")?
                .trim(),
        )
    };
    fs::canonicalize(&resolved).with_context(|| {
        format!(
            "resolving linker {} through filesystem aliases from {}",
            candidate.display(),
            resolved.display()
        )
    })
}

fn discover_link_inputs(
    invocation: &RustcInvocation,
    prepared: &PreparedInvocation,
    cache_options: &CacheOptions,
    capture_path: &Path,
    real_linker: &Path,
) -> Result<LinkDiscovery> {
    let encoded = fs::read_to_string(capture_path)
        .with_context(|| format!("reading linker capture {}", capture_path.display()))?;
    let invocations = encoded
        .lines()
        .map(serde_json::from_str::<LinkerCaptureRecord>)
        .collect::<Result<Vec<_>, _>>()
        .context("parsing linker capture")?;
    if invocations.is_empty() {
        anyhow::bail!("rustc completed without invoking the captured linker");
    }
    if invocations
        .iter()
        .any(|capture| capture.schema_version != 1)
    {
        anyhow::bail!("linker capture has an unsupported schema version");
    }

    let already_declared = prepared
        .inputs
        .iter()
        .map(|input| {
            fs::canonicalize(&input.actual_path).unwrap_or_else(|_| input.actual_path.clone())
        })
        .collect::<BTreeSet<_>>();
    let declared_outputs = prepared
        .output_files
        .iter()
        .map(|output| {
            fs::canonicalize(&output.actual_path).unwrap_or_else(|_| output.actual_path.clone())
        })
        .collect::<BTreeSet<_>>();
    let mut paths = BTreeSet::new();
    paths.insert(real_linker.to_path_buf());
    for captured in &invocations {
        for traced in &captured.traced_inputs {
            let path = absolute_path(Path::new(traced), &invocation.cwd);
            if path.is_file() {
                paths.insert(path);
            }
        }
        collect_link_argument_inputs(
            &captured.arguments,
            &invocation.cwd,
            real_linker,
            &mut paths,
        )?;
    }
    collect_apple_platform_inputs(&mut paths)?;

    let mut inputs = Vec::new();
    for path in paths {
        let path = fs::canonicalize(&path).unwrap_or(path);
        if already_declared.contains(&path)
            || declared_outputs.contains(&path)
            || is_rustc_generated_link_input(&path, &prepared.path_mappings)
            || !path.is_file()
        {
            continue;
        }
        let logical_path = logical_link_path(&path, &prepared.path_mappings);
        let content_addressed_root = logical_path.starts_with("target/")
            || logical_path.starts_with("package/")
            || logical_path.starts_with("workspace/");
        let memo = if content_addressed_root {
            digest_relocatable_file(&path, &prepared.path_mappings)?
        } else {
            digest_file_memoized(cache_options, &path)?
        };
        inputs.push(LinkInput {
            logical_path,
            actual_path: path,
            sha256: memo.sha256,
            size_bytes: memo.size_bytes,
            modified_unix_ns: memo.modified_unix_ns,
            #[cfg(unix)]
            device: memo.device,
            #[cfg(unix)]
            inode: memo.inode,
        });
    }
    inputs.sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    inputs.dedup_by(|left, right| left.logical_path == right.logical_path);
    Ok(LinkDiscovery {
        schema_version: 1,
        base_action_key: prepared.action_key.clone(),
        inputs,
    })
}

fn digest_relocatable_file(path: &Path, mappings: &[(String, String)]) -> Result<FileDigestMemo> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading relocatable linker input {}", path.display()))?;
    if let Some((sha256, size_bytes)) = restored_logical_digest(path)? {
        return Ok(FileDigestMemo {
            schema_version: 1,
            actual_path: path.to_path_buf(),
            sha256,
            size_bytes,
            modified_unix_ns: modified_unix_ns(&metadata)?,
            #[cfg(unix)]
            device: unix_device(&metadata),
            #[cfg(unix)]
            inode: unix_inode(&metadata),
        });
    }
    let bytes = fs::read(path)
        .with_context(|| format!("reading relocatable linker input {}", path.display()))?;
    let bytes = normalize_artifact_slots(bytes, mappings)?;
    Ok(FileDigestMemo {
        schema_version: 1,
        actual_path: path.to_path_buf(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        size_bytes: u64::try_from(bytes.len()).context("linker input is too large")?,
        modified_unix_ns: modified_unix_ns(&metadata)?,
        #[cfg(unix)]
        device: unix_device(&metadata),
        #[cfg(unix)]
        inode: unix_inode(&metadata),
    })
}

fn digest_file_memoized(options: &CacheOptions, path: &Path) -> Result<FileDigestMemo> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("reading linker input metadata {}", path.display()))?;
    let path_key = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
    let memo_path = options
        .root
        .join("file-digests")
        .join(format!("{path_key}.json"));
    if let Ok(encoded) = fs::read(&memo_path)
        && let Ok(memo) = serde_json::from_slice::<FileDigestMemo>(&encoded)
        && memo.schema_version == 1
        && memo.actual_path == path
        && memo.size_bytes == metadata.len()
        && memo.modified_unix_ns == modified_unix_ns(&metadata)?
        && memo_identity_matches(&memo, &metadata)
    {
        return Ok(memo);
    }
    let bytes = fs::read(path)
        .with_context(|| format!("reading discovered linker input {}", path.display()))?;
    let memo = FileDigestMemo {
        schema_version: 1,
        actual_path: path.to_path_buf(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        size_bytes: u64::try_from(bytes.len()).context("linker input is too large")?,
        modified_unix_ns: modified_unix_ns(&metadata)?,
        #[cfg(unix)]
        device: unix_device(&metadata),
        #[cfg(unix)]
        inode: unix_inode(&metadata),
    };
    write_atomic(
        &memo_path,
        &serde_json::to_vec(&memo).context("serializing file digest memo")?,
    )?;
    Ok(memo)
}

fn memo_identity_matches(memo: &FileDigestMemo, metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        memo.device == unix_device(metadata) && memo.inode == unix_inode(metadata)
    }
    #[cfg(not(unix))]
    {
        let _ = (memo, metadata);
        true
    }
}

fn is_rustc_generated_link_input(path: &Path, mappings: &[(String, String)]) -> bool {
    if path.extension().is_some_and(|extension| extension == "o")
        && mappings
            .iter()
            .any(|(label, root)| label == "target" && path.starts_with(root))
    {
        return true;
    }
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| value.starts_with("rustc") && value.len() > 5)
    })
}

fn collect_link_argument_inputs(
    arguments: &[String],
    cwd: &Path,
    real_linker: &Path,
    paths: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let mut library_paths = Vec::new();
    let mut framework_paths = Vec::new();
    let mut libraries = Vec::new();
    let mut frameworks = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if let Some(response) = argument.strip_prefix('@') {
            let response = absolute_path(Path::new(response), cwd);
            if !response.is_file() {
                anyhow::bail!(
                    "linker response file disappeared before capture: {}",
                    response.display()
                );
            }
            paths.insert(response);
        } else if argument == "-L" || argument == "-F" || argument == "-framework" {
            let value = arguments
                .get(index + 1)
                .with_context(|| format!("linker option {argument} is missing its value"))?;
            match argument.as_str() {
                "-L" => library_paths.push(absolute_path(Path::new(value), cwd)),
                "-F" => framework_paths.push(absolute_path(Path::new(value), cwd)),
                "-framework" => frameworks.push(value.clone()),
                _ => unreachable!(),
            }
            index += 1;
        } else if let Some(value) = argument.strip_prefix("-L") {
            library_paths.push(absolute_path(Path::new(value), cwd));
        } else if let Some(value) = argument.strip_prefix("-F") {
            framework_paths.push(absolute_path(Path::new(value), cwd));
        } else if let Some(value) = argument.strip_prefix("-l") {
            if !value.is_empty() {
                libraries.push(value.to_owned());
            }
        } else {
            let path = absolute_path(Path::new(argument), cwd);
            if path.is_file() {
                paths.insert(path);
            }
        }
        index += 1;
    }

    if cfg!(target_os = "macos") {
        let sdk = command_path("xcrun", &["--show-sdk-path"])?;
        library_paths.push(sdk.join("usr/lib"));
        framework_paths.push(sdk.join("System/Library/Frameworks"));
        libraries.push("System".to_owned());
    }
    for library in libraries {
        let traced = paths
            .iter()
            .find(|path| traced_path_matches_library(path, &library))
            .cloned();
        let explicit = library_paths
            .iter()
            .flat_map(|root| {
                ["so", "a", "dylib", "tbd"]
                    .map(|extension| root.join(format!("lib{library}.{extension}")))
            })
            .find(|path| path.is_file());
        let driver_reported = if traced.is_none() && explicit.is_none() {
            linker_reported_library(real_linker, &library, cwd)?
        } else {
            None
        };
        let resolved = traced
            .or(explicit)
            .or(driver_reported)
            .with_context(|| format!("resolving native library -l{library}"))?;
        paths.insert(resolved);
    }
    for framework in frameworks {
        let resolved = framework_paths
            .iter()
            .flat_map(|root| {
                let directory = root.join(format!("{framework}.framework"));
                [
                    directory.join(&framework),
                    directory.join(format!("{framework}.tbd")),
                ]
            })
            .find(|path| path.is_file())
            .with_context(|| format!("resolving framework {framework}"))?;
        paths.insert(resolved);
    }
    Ok(())
}

fn linker_reported_library(linker: &Path, library: &str, cwd: &Path) -> Result<Option<PathBuf>> {
    if !cfg!(target_os = "linux") {
        return Ok(None);
    }
    for extension in ["so", "a"] {
        let filename = format!("lib{library}.{extension}");
        let output = Command::new(linker)
            .arg(format!("-print-file-name={filename}"))
            .output()
            .with_context(|| {
                format!(
                    "asking linker driver {} to resolve {filename}",
                    linker.display()
                )
            })?;
        if !output.status.success() {
            continue;
        }
        let reported = String::from_utf8(output.stdout)
            .context("linker driver returned a non-UTF-8 library path")?;
        let reported = reported.trim();
        if reported.is_empty() || reported == filename {
            continue;
        }
        let path = absolute_path(Path::new(reported), cwd);
        if path.is_file() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn traced_path_matches_library(path: &Path, library: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let prefix = format!("lib{library}.");
            name.starts_with(&prefix)
        })
}

fn collect_apple_platform_inputs(paths: &mut BTreeSet<PathBuf>) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    for tool in ["clang", "ld"] {
        paths.insert(command_path("xcrun", &["--find", tool])?);
    }
    let sdk = command_path("xcrun", &["--show-sdk-path"])?;
    for relative in [
        "SDKSettings.json",
        "SDKSettings.plist",
        "usr/lib/libSystem.tbd",
    ] {
        let path = sdk.join(relative);
        if path.is_file() {
            paths.insert(path);
        }
    }
    Ok(())
}

fn command_path(command: &str, arguments: &[&str]) -> Result<PathBuf> {
    let output = Command::new(command)
        .args(arguments)
        .output()
        .with_context(|| format!("running {command} {}", arguments.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!("{command} {} failed", arguments.join(" "));
    }
    Ok(PathBuf::from(
        String::from_utf8(output.stdout)
            .context("platform tool returned a non-UTF-8 path")?
            .trim(),
    ))
}

fn absolute_path(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn platform_logical_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(relative) = text.strip_prefix("/Library/Developer/") {
        format!("platform/developer/{relative}")
    } else if let Some(relative) = text.strip_prefix("/Users/") {
        let tail = relative.split('/').skip(1).collect::<Vec<_>>().join("/");
        format!("platform/user/{tail}")
    } else {
        format!("platform/absolute/{}", text.trim_start_matches('/'))
    }
}

fn logical_link_path(path: &Path, mappings: &[(String, String)]) -> String {
    for (label, actual) in mappings {
        if let Ok(relative) = path.strip_prefix(actual) {
            if relative.as_os_str().is_empty() {
                return label.clone();
            }
            return format!("{label}/{}", relative.to_string_lossy());
        }
    }
    platform_logical_path(path)
}

fn load_link_discovery(
    options: &CacheOptions,
    base_key: &str,
    prepared: &PreparedInvocation,
) -> Result<Option<LinkDiscovery>> {
    let path = link_discovery_path(options, base_key);
    let Ok(encoded) = fs::read(&path) else {
        return Ok(None);
    };
    let discovery: LinkDiscovery = serde_json::from_slice(&encoded)
        .with_context(|| format!("reading link discovery {}", path.display()))?;
    if discovery.schema_version != 1 || discovery.base_action_key != base_key {
        return Ok(None);
    }
    for input in &discovery.inputs {
        if let Some(declared) = prepared
            .inputs
            .iter()
            .find(|declared| declared.logical_path == input.logical_path)
        {
            if declared.sha256 != input.sha256 || declared.size_bytes != input.size_bytes {
                return Ok(None);
            }
            continue;
        }
        let actual = resolve_link_input(input, &prepared.path_mappings);
        let content_addressed_root = input.logical_path.starts_with("target/")
            || input.logical_path.starts_with("package/")
            || input.logical_path.starts_with("workspace/");
        if content_addressed_root {
            if !relocatable_file_matches(
                &actual,
                &input.sha256,
                input.size_bytes,
                &prepared.path_mappings,
            )? {
                return Ok(None);
            }
        } else if !link_input_matches(input, &actual)? {
            return Ok(None);
        }
    }
    Ok(Some(discovery))
}

fn relocatable_file_matches(
    path: &Path,
    digest: &str,
    size: u64,
    mappings: &[(String, String)],
) -> Result<bool> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(false);
    };
    if metadata.len() != size {
        return Ok(false);
    }
    if let Some((logical_sha256, logical_size)) = restored_logical_digest(path)? {
        return Ok(logical_size == size && logical_sha256 == digest);
    }
    let bytes = fs::read(path)
        .with_context(|| format!("validating relocatable input {}", path.display()))?;
    let bytes = normalize_artifact_slots(bytes, mappings)?;
    Ok(format!("{:x}", Sha256::digest(bytes)) == digest)
}

fn resolve_link_input(input: &LinkInput, mappings: &[(String, String)]) -> PathBuf {
    for (label, actual) in mappings {
        if input.logical_path == *label {
            return PathBuf::from(actual);
        }
        if let Some(relative) = input.logical_path.strip_prefix(&format!("{label}/")) {
            return PathBuf::from(actual).join(relative);
        }
    }
    input.actual_path.clone()
}

fn link_input_matches(input: &LinkInput, actual_path: &Path) -> Result<bool> {
    let Ok(metadata) = fs::metadata(actual_path) else {
        return Ok(false);
    };
    if metadata.len() != input.size_bytes || modified_unix_ns(&metadata)? != input.modified_unix_ns
    {
        return Ok(false);
    }
    #[cfg(unix)]
    if unix_device(&metadata) != input.device || unix_inode(&metadata) != input.inode {
        return Ok(false);
    }
    Ok(true)
}

fn modified_unix_ns(metadata: &fs::Metadata) -> Result<u128> {
    Ok(metadata
        .modified()
        .context("reading input modification time")?
        .duration_since(std::time::UNIX_EPOCH)
        .context("input modification time is before the Unix epoch")?
        .as_nanos())
}

#[cfg(unix)]
fn unix_device(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(unix)]
fn unix_inode(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

fn publish_link_discovery(options: &CacheOptions, discovery: &LinkDiscovery) -> Result<()> {
    write_atomic(
        &link_discovery_path(options, &discovery.base_action_key),
        &serde_json::to_vec(discovery).context("serializing link discovery")?,
    )
}

fn discovered_action_key(base_key: &str, discovery: &LinkDiscovery) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(base_key.as_bytes());
    let stable_inputs = discovery
        .inputs
        .iter()
        .map(|input| (&input.logical_path, &input.sha256, input.size_bytes))
        .collect::<Vec<_>>();
    digest.update(serde_json::to_vec(&stable_inputs).context("serializing link inputs")?);
    Ok(format!("{:x}", digest.finalize()))
}

fn restore(options: &CacheOptions, prepared: &PreparedInvocation) -> Result<bool> {
    let manifest_path = action_manifest_path(options, &prepared.action_key);
    let Ok(encoded) = fs::read(&manifest_path) else {
        return Ok(false);
    };
    let manifest: ActionManifest = serde_json::from_slice(&encoded)
        .with_context(|| format!("reading action manifest {}", manifest_path.display()))?;
    if manifest.schema_version != 1 || manifest.action_key != prepared.action_key {
        return Ok(false);
    }
    if manifest.outputs.len() != prepared.output_files.len() {
        return Ok(false);
    }

    for (cached, output) in manifest.outputs.iter().zip(&prepared.output_files) {
        if cached.logical_path != output.logical_path {
            return Ok(false);
        }
        let blob = blob_path(options, &cached.sha256);
        if !blob_matches(&blob, &cached.sha256, cached.size_bytes)? {
            return Ok(false);
        }
    }
    for (cached, output) in manifest.outputs.iter().zip(&prepared.output_files) {
        materialize_blob(options, prepared, cached, &output.actual_path)?;
    }
    Ok(true)
}

fn publish(options: &CacheOptions, prepared: &PreparedInvocation) -> Result<bool> {
    if prepared.output_files.is_empty()
        || prepared
            .output_files
            .iter()
            .any(|output| !output.actual_path.is_file())
    {
        return Ok(false);
    }

    let mut outputs = Vec::with_capacity(prepared.output_files.len());
    for output in &prepared.output_files {
        let metadata = fs::metadata(&output.actual_path)
            .with_context(|| format!("reading output metadata {}", output.actual_path.display()))?;
        let original = fs::read(&output.actual_path)
            .with_context(|| format!("reading output {}", output.actual_path.display()))?;
        let mut bytes = normalize_artifact_slots(original.clone(), &prepared.path_mappings)?;
        let dependency_file = output
            .actual_path
            .extension()
            .is_some_and(|extension| extension == "d");
        if dependency_file {
            bytes = rewrite_paths_for_cache(bytes, &prepared.path_mappings);
        }
        let path_rewritten = bytes != original;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        publish_blob(options, &sha256, &bytes)?;
        if path_rewritten {
            record_logical_digest(
                &output.actual_path,
                &sha256,
                u64::try_from(bytes.len()).context("cached output is too large")?,
            )?;
        }
        outputs.push(CachedOutput {
            logical_path: output.logical_path.clone(),
            sha256,
            size_bytes: u64::try_from(bytes.len()).context("cached output is too large")?,
            path_rewritten,
            #[cfg(unix)]
            mode: unix_mode(&metadata),
        });
    }

    let manifest = ActionManifest {
        schema_version: 1,
        action_key: prepared.action_key.clone(),
        outputs,
    };
    let path = action_manifest_path(options, &prepared.action_key);
    write_atomic(
        &path,
        &serde_json::to_vec(&manifest).context("serializing action manifest")?,
    )?;
    Ok(true)
}

fn publish_blob(options: &CacheOptions, digest: &str, bytes: &[u8]) -> Result<()> {
    let path = blob_path(options, digest);
    if blob_matches(
        &path,
        digest,
        u64::try_from(bytes.len()).context("blob is too large")?,
    )? {
        return Ok(());
    }
    write_atomic(&path, bytes)
}

fn materialize_blob(
    options: &CacheOptions,
    prepared: &PreparedInvocation,
    cached: &CachedOutput,
    output: &Path,
) -> Result<()> {
    let blob = blob_path(options, &cached.sha256);
    let parent = output
        .parent()
        .with_context(|| format!("output has no parent: {}", output.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating output directory {}", parent.display()))?;
    let temporary = temporary_path(output);
    if cached.path_rewritten {
        let bytes = fs::read(&blob)
            .with_context(|| format!("reading path-normalized blob {}", cached.sha256))?;
        let bytes = restore_cached_paths(bytes, &prepared.path_mappings);
        let bytes = materialize_artifact_slots(bytes, &prepared.path_mappings)?;
        fs::write(&temporary, bytes).with_context(|| {
            format!(
                "materializing path-normalized blob {} to {}",
                cached.sha256,
                temporary.display()
            )
        })?;
    } else {
        fs::copy(&blob, &temporary).with_context(|| {
            format!(
                "materializing cached blob {} to {}",
                cached.sha256,
                temporary.display()
            )
        })?;
    }
    #[cfg(unix)]
    set_unix_mode(&temporary, cached.mode)?;
    #[cfg(target_os = "macos")]
    if cached.path_rewritten && cached.mode & 0o111 != 0 {
        let result = Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&temporary)
            .output()
            .with_context(|| format!("re-signing restored Mach-O output {}", output.display()))?;
        if !result.status.success() {
            anyhow::bail!(
                "codesign rejected restored output {}: {}",
                output.display(),
                String::from_utf8_lossy(&result.stderr).trim()
            );
        }
    }
    fs::rename(&temporary, output)
        .with_context(|| format!("installing cached output {}", output.display()))?;
    if cached.path_rewritten {
        record_logical_digest(output, &cached.sha256, cached.size_bytes)?;
    }
    Ok(())
}

fn rewrite_paths_for_cache(mut bytes: Vec<u8>, mappings: &[(String, String)]) -> Vec<u8> {
    for (label, actual) in mappings {
        bytes = replace_bytes(
            &bytes,
            actual.as_bytes(),
            format!("/__cargo_reapi__/{label}").as_bytes(),
        );
    }
    bytes
}

fn restore_cached_paths(mut bytes: Vec<u8>, mappings: &[(String, String)]) -> Vec<u8> {
    for (label, actual) in mappings.iter().rev() {
        bytes = replace_bytes(
            &bytes,
            format!("/__cargo_reapi__/{label}").as_bytes(),
            actual.as_bytes(),
        );
    }
    bytes
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut result = Vec::with_capacity(haystack.len());
    let mut offset = 0;
    while let Some(found) = haystack[offset..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let position = offset + found;
        result.extend_from_slice(&haystack[offset..position]);
        result.extend_from_slice(replacement);
        offset = position + needle.len();
    }
    result.extend_from_slice(&haystack[offset..]);
    result
}

fn blob_matches(path: &Path, digest: &str, size: u64) -> Result<bool> {
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(false);
    };
    if metadata.len() != size {
        return Ok(false);
    }
    let bytes =
        fs::read(path).with_context(|| format!("verifying cached blob {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)) == digest)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("cache path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating cache directory {}", parent.display()))?;
    let temporary = temporary_path(path);
    let mut file = File::create(&temporary)
        .with_context(|| format!("creating temporary cache file {}", temporary.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing temporary cache file {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing temporary cache file {}", temporary.display()))?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if path.exists() => {
            let published = fs::read(path).with_context(|| {
                format!(
                    "verifying concurrently published cache file {}",
                    path.display()
                )
            })?;
            fs::remove_file(&temporary).ok();
            if published == bytes {
                Ok(())
            } else {
                Err(error)
                    .with_context(|| format!("replacing corrupt cache file {}", path.display()))
            }
        }
        Err(error) => {
            Err(error).with_context(|| format!("publishing cache file {}", path.display()))
        }
    }
}

fn action_manifest_path(options: &CacheOptions, action_key: &str) -> PathBuf {
    options
        .root
        .join("actions")
        .join(format!("{action_key}.json"))
}

fn link_discovery_path(options: &CacheOptions, base_key: &str) -> PathBuf {
    options
        .root
        .join("discoveries")
        .join(format!("{base_key}.json"))
}

fn blob_path(options: &CacheOptions, digest: &str) -> PathBuf {
    options.root.join("blobs").join(digest)
}

fn temporary_path(path: &Path) -> PathBuf {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("tmp-{}-{sequence}", std::process::id()))
}

#[cfg(unix)]
fn unix_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("restoring cached output mode for {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        LinkDiscovery, LinkInput, discovered_action_key, resolve_linker_candidate,
        traced_path_matches_library,
    };
    use std::path::PathBuf;

    fn discovery(actual_path: &str, sha256: &str, modified: u128) -> LinkDiscovery {
        LinkDiscovery {
            schema_version: 1,
            base_action_key: "base".to_owned(),
            inputs: vec![LinkInput {
                logical_path: "target/debug/build/native/libdemo.a".to_owned(),
                actual_path: PathBuf::from(actual_path),
                sha256: sha256.to_owned(),
                size_bytes: 42,
                modified_unix_ns: modified,
                #[cfg(unix)]
                device: 1,
                #[cfg(unix)]
                inode: u64::try_from(modified).unwrap_or(u64::MAX),
            }],
        }
    }

    #[test]
    fn link_key_uses_logical_content_not_worktree_metadata() {
        let producer = discovery("/tmp/producer/target/libdemo.a", "content", 1);
        let consumer = discovery("/tmp/a-longer-consumer/target/libdemo.a", "content", 999);
        let changed = discovery("/tmp/producer/target/libdemo.a", "changed", 1);
        assert_eq!(
            discovered_action_key("base", &producer).expect("producer key"),
            discovered_action_key("base", &consumer).expect("consumer key")
        );
        assert_ne!(
            discovered_action_key("base", &producer).expect("producer key"),
            discovered_action_key("base", &changed).expect("changed key")
        );
    }

    #[cfg(unix)]
    #[test]
    fn linker_alias_is_resolved_before_entering_the_sandbox() {
        use std::os::unix::fs::symlink;

        let fixture = tempfile::tempdir().unwrap();
        let executable = fixture.path().join("actual-linker");
        let alias = fixture.path().join("cc");
        std::fs::write(&executable, b"linker").unwrap();
        symlink(&executable, &alias).unwrap();
        assert_eq!(
            resolve_linker_candidate(&alias).unwrap(),
            executable.canonicalize().unwrap()
        );
    }

    #[test]
    fn linux_link_trace_identifies_versioned_and_unversioned_libraries() {
        assert!(traced_path_matches_library(
            std::path::Path::new("/lib/x86_64-linux-gnu/libgcc_s.so.1"),
            "gcc_s"
        ));
        assert!(traced_path_matches_library(
            std::path::Path::new("/usr/lib/libc.a"),
            "c"
        ));
        assert!(!traced_path_matches_library(
            std::path::Path::new("/usr/lib/libgcc.a"),
            "gcc_s"
        ));
    }
}
