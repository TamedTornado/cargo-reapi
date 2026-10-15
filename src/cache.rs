use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capture::{
    CaptureOptions, DepInput, PreparedInvocation, bundles_nothing_unkeyed, prepare_invocation,
    record_invocation,
};
use crate::environment::{
    CompileEnvironment, EnvironmentHints, is_scheduling_environment, tracked_environment,
};
use crate::invocation::RustcInvocation;
use crate::maintenance::{acquire_shared, record_access};
use crate::relocation::{
    logical_bytes, materialize_artifact_slots, record_logical_digest, restore_cached_paths,
};
use crate::resource::ResourceLease;

pub struct CacheOptions {
    root: PathBuf,
}

impl CacheOptions {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// Only the version of an action manifest, which every layout carries.
#[derive(Deserialize)]
struct ManifestVersion {
    schema_version: u32,
}

/// The manifest in `encoded` when it is of the current version and
/// readable, and `None` for anything else: an unsupported version, or a
/// damaged entry, is a cache miss.
fn read_action_manifest(encoded: &[u8]) -> Option<ActionManifest> {
    let version: ManifestVersion = serde_json::from_slice(encoded).ok()?;
    if version.schema_version != ACTION_MANIFEST_VERSION {
        return None;
    }
    serde_json::from_slice(encoded).ok()
}

/// The version of the action manifest, which records dep-info inputs.
/// Raised whenever the rules for what may be stored tighten, so an entry an
/// older cargo-reapi stored under looser rules is a miss: version 4 is the
/// first stored only after its inputs were re-checked once the compile
/// finished, and with dep-info roots that need no escaping.
const ACTION_MANIFEST_VERSION: u32 = 4;

#[derive(Debug, Deserialize, Serialize)]
struct ActionManifest {
    schema_version: u32,
    action_key: String,
    outputs: Vec<CachedOutput>,
    /// Every file rustc's own dep-info listed when this entry was made, with
    /// its digest: the entry is a hit only while all of them still match.
    dep_inputs: Vec<DepInput>,
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

/// How one attempt at a cached compile ended.
enum Resolution {
    Finished(i32),
    /// The compile tracked variables the caller set but its keyed environment
    /// withheld. When `rerun` is false the compile already succeeded and Cargo
    /// may have used its artifacts, so it cannot be repeated invisibly.
    Withheld {
        names: BTreeSet<String>,
        rerun: bool,
    },
}

pub fn execute_cached(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    cache_options: &CacheOptions,
) -> Result<i32> {
    let _maintenance = acquire_shared(&cache_options.root)?;
    let hints = EnvironmentHints::new(&cache_options.root, invocation);
    let mut environment = CompileEnvironment::for_compile(invocation, hints.as_ref());

    loop {
        let prepared = prepare_invocation(invocation, &environment)?;
        if !prepared.cache_eligibility.eligible {
            let exit_code = invocation.execute()?;
            record_invocation(capture_options, &prepared, "local-ineligible", exit_code)?;
            return Ok(exit_code);
        }
        environment.report(prepared_crate_name(invocation).as_deref());

        let resolution = execute_cached_regular(
            invocation,
            capture_options,
            cache_options,
            &prepared,
            &environment,
        )?;

        match resolution {
            Resolution::Finished(exit_code) => return Ok(exit_code),
            Resolution::Withheld { names, rerun } => {
                if let Some(hints) = &hints {
                    hints.record(&names)?;
                }
                if !rerun {
                    report_withheld_after_success(invocation, &names);
                    return Ok(1);
                }
                // `withheld` names only variables this environment can pass,
                // so every retry runs the compiler with more of them.
                anyhow::ensure!(
                    environment.pass(&names),
                    "cargo-reapi: retrying a compile without changing its environment"
                );
            }
        }
    }
}

fn prepared_crate_name(invocation: &RustcInvocation) -> Option<String> {
    invocation
        .crate_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn report_withheld_after_success(invocation: &RustcInvocation, names: &BTreeSet<String>) {
    let crate_name = prepared_crate_name(invocation).unwrap_or_else(|| "a compile".to_owned());
    let names = names.iter().cloned().collect::<Vec<_>>().join(", ");
    eprintln!(
        "cargo-reapi: {crate_name} read {names} at compile time, but its keyed compile \
         environment withheld them, so its artifacts would not match plain Cargo. cargo-reapi \
         now passes them to {crate_name}; run the build again, or list them in \
         CARGO_REAPI_DECLARED_ENV."
    );
}

fn execute_cached_regular(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    cache_options: &CacheOptions,
    prepared: &PreparedInvocation,
    environment: &CompileEnvironment,
) -> Result<Resolution> {
    let (lock, coalesced) = lock_action(cache_options, &prepared.action_key)?;
    #[cfg(feature = "test-hooks")]
    hold_test_producer_lock(coalesced);

    let result = finish_cached_execution(
        invocation,
        capture_options,
        cache_options,
        prepared,
        environment,
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
    let lock = crate::safe_io::open_lock(&lock_path)
        .with_context(|| format!("opening action lock {}", lock_path.display()))?;
    let coalesced = match lock.try_lock_exclusive() {
        Ok(()) => false,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            #[cfg(feature = "test-hooks")]
            if let Some(signal) = test_coalesce_signal() {
                fs::write(&signal, b"")
                    .with_context(|| format!("signalling {}", signal.display()))?;
            }
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

/// Test synchronization for coalescing, compiled only with the
/// `test-hooks` feature, which only the integration tests enable: a file
/// named by `CARGO_REAPI_ACTION_CACHE_TEST_COALESCE_SIGNAL` that a process
/// finding an action's lock held creates before it waits, and that the
/// action's producer waits for before it executes. Two identical builds
/// then coalesce however their processes are scheduled.
#[cfg(feature = "test-hooks")]
fn test_coalesce_signal() -> Option<PathBuf> {
    std::env::var_os("CARGO_REAPI_ACTION_CACHE_TEST_COALESCE_SIGNAL").map(PathBuf::from)
}

/// Holds a producer's action lock until another process waits on it; see
/// [`test_coalesce_signal`].
#[cfg(feature = "test-hooks")]
fn hold_test_producer_lock(coalesced: bool) {
    if coalesced {
        return;
    }
    let Some(signal) = test_coalesce_signal() else {
        return;
    };
    while !signal.exists() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn finish_cached_execution(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    cache_options: &CacheOptions,
    prepared: &PreparedInvocation,
    environment: &CompileEnvironment,
    coalesced: bool,
) -> Result<Resolution> {
    let withheld = withheld_by_cached_entry(cache_options, prepared, environment);
    if !withheld.is_empty() {
        return Ok(Resolution::Withheld {
            names: withheld,
            rerun: true,
        });
    }
    if restore(cache_options, invocation, prepared)? {
        record_access(&cache_options.root, &prepared.action_key)?;
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
        return Ok(Resolution::Finished(0));
    }

    let _lease = ResourceLease::acquire()?;
    remove_stale_dep_info(prepared)?;
    let started = SystemTime::now();
    let exit_code = invocation.execute_in(Some(environment))?;
    let withheld = withheld_by_execution(prepared, environment)?;
    if !withheld.is_empty() {
        record_invocation(
            capture_options,
            prepared,
            "local-environment-withheld",
            exit_code,
        )?;
        return Ok(Resolution::Withheld {
            names: withheld,
            rerun: exit_code != 0,
        });
    }

    let execution = if exit_code != 0 {
        "local-failed"
    } else if !bundles_nothing_unkeyed(prepared)? {
        // The rlib bundled a static library from an outside directory the
        // key does not cover.
        "local-ineligible"
    } else {
        match publish(cache_options, invocation, prepared, started)? {
            Publication::Stored => {
                record_access(&cache_options.root, &prepared.action_key)?;
                "local-cache-miss"
            }
            Publication::InputChanged => "local-input-changed",
            Publication::Incomplete => "local-output-incomplete",
        }
    };
    record_invocation(capture_options, prepared, execution, exit_code)?;
    Ok(Resolution::Finished(exit_code))
}

/// Tracked variables a cached entry's compile recorded that the caller set
/// but this environment withholds. Restoring that entry would hand Cargo an
/// artifact built without them.
fn withheld_by_cached_entry(
    options: &CacheOptions,
    prepared: &PreparedInvocation,
    environment: &CompileEnvironment,
) -> BTreeSet<String> {
    let Some(dep_info) = &prepared.dep_info else {
        return BTreeSet::new();
    };
    let Ok(encoded) = fs::read(action_manifest_path(options, &prepared.action_key)) else {
        return BTreeSet::new();
    };
    let Some(manifest) = read_action_manifest(&encoded) else {
        return BTreeSet::new();
    };
    let Some(cached) = manifest
        .outputs
        .iter()
        .find(|output| output.logical_path == dep_info.logical_path)
    else {
        return BTreeSet::new();
    };
    let Ok(bytes) = fs::read(blob_path(options, &cached.sha256)) else {
        return BTreeSet::new();
    };
    environment.withheld(&tracked_environment(&bytes))
}

/// Tracked variables the compile just recorded that the caller set but this
/// environment withheld.
fn withheld_by_execution(
    prepared: &PreparedInvocation,
    environment: &CompileEnvironment,
) -> Result<BTreeSet<String>> {
    let Some(dep_info) = &prepared.dep_info else {
        return Ok(BTreeSet::new());
    };
    match fs::read(&dep_info.actual_path) {
        Ok(bytes) => Ok(environment.withheld(&tracked_environment(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(error) => Err(error)
            .with_context(|| format!("reading dep-info {}", dep_info.actual_path.display())),
    }
}

/// A failed compile may stop before writing dep-info. Removing the previous
/// one keeps its tracked variables from being mistaken for this compile's.
fn remove_stale_dep_info(prepared: &PreparedInvocation) -> Result<()> {
    let Some(dep_info) = &prepared.dep_info else {
        return Ok(());
    };
    match fs::remove_file(&dep_info.actual_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "removing previous dep-info {}",
                dep_info.actual_path.display()
            )
        }),
    }
}

fn restore(
    options: &CacheOptions,
    invocation: &RustcInvocation,
    prepared: &PreparedInvocation,
) -> Result<bool> {
    let manifest_path = action_manifest_path(options, &prepared.action_key);
    let Ok(encoded) = fs::read(&manifest_path) else {
        return Ok(false);
    };
    // The version is read before the rest: an entry an older or newer
    // cargo-reapi wrote has another schema, and is a miss, never an error.
    let Some(manifest) = read_action_manifest(&encoded) else {
        return Ok(false);
    };
    if manifest.action_key != prepared.action_key {
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
    if !crate::capture::dep_inputs_match(
        invocation,
        &manifest.dep_inputs,
        &prepared.relocation_mappings,
    )? {
        return Ok(false);
    }
    for (cached, output) in manifest.outputs.iter().zip(&prepared.output_files) {
        materialize_blob(options, prepared, cached, &output.actual_path)?;
    }
    Ok(true)
}

/// What [`publish`] did with a successful compile's outputs.
enum Publication {
    Stored,
    /// An input changed while the compile ran, so its outputs may not match
    /// the content the entry would record.
    InputChanged,
    /// An output is missing, or the entry could not be checked on a hit.
    Incomplete,
}

fn publish(
    options: &CacheOptions,
    invocation: &RustcInvocation,
    prepared: &PreparedInvocation,
    started: SystemTime,
) -> Result<Publication> {
    if prepared.output_files.is_empty()
        || prepared
            .output_files
            .iter()
            .any(|output| !output.actual_path.is_file())
    {
        return Ok(Publication::Incomplete);
    }
    if reads_scheduling_environment(prepared) {
        return Ok(Publication::Incomplete);
    }
    if !crate::capture::keyed_inputs_unchanged(invocation, prepared, started)? {
        return Ok(Publication::InputChanged);
    }
    // Without every input rustc read there is nothing to check a hit
    // against, so the entry is not published.
    let dep_inputs = match crate::capture::read_dep_inputs(invocation, prepared, started) {
        Ok(Some(dep_inputs)) => dep_inputs,
        Ok(None) => return Ok(Publication::InputChanged),
        Err(_) => return Ok(Publication::Incomplete),
    };

    let mut outputs = Vec::with_capacity(prepared.output_files.len());
    for output in &prepared.output_files {
        let metadata = fs::metadata(&output.actual_path)
            .with_context(|| format!("reading output metadata {}", output.actual_path.display()))?;
        let original = fs::read(&output.actual_path)
            .with_context(|| format!("reading output {}", output.actual_path.display()))?;
        let bytes = logical_bytes(
            &output.actual_path,
            original.clone(),
            &prepared.relocation_mappings,
        )?;
        let path_rewritten = bytes != original;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        publish_blob(options, &sha256, &bytes)?;
        if path_rewritten {
            record_logical_digest(
                &output.actual_path,
                &prepared.relocation_mappings,
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
        schema_version: ACTION_MANIFEST_VERSION,
        action_key: prepared.action_key.clone(),
        outputs,
        dep_inputs,
    };
    let path = action_manifest_path(options, &prepared.action_key);
    write_atomic(
        &path,
        &serde_json::to_vec(&manifest).context("serializing action manifest")?,
    )?;
    Ok(Publication::Stored)
}

/// Whether the compile's dep-info shows its source read a variable the key
/// leaves out, or cannot be read. Its output could then differ under the
/// same key.
fn reads_scheduling_environment(prepared: &PreparedInvocation) -> bool {
    let Some(dep_info) = &prepared.dep_info else {
        return false;
    };
    let Ok(bytes) = fs::read(&dep_info.actual_path) else {
        return true;
    };
    tracked_environment(&bytes)
        .iter()
        .any(|name| is_scheduling_environment(name))
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
    let temporary = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
    if cached.path_rewritten {
        let bytes = fs::read(&blob)
            .with_context(|| format!("reading path-normalized blob {}", cached.sha256))?;
        let bytes = restore_cached_paths(bytes, &prepared.relocation_mappings);
        let bytes = materialize_artifact_slots(bytes, &prepared.relocation_mappings)?;
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
    fs::rename(&temporary, output)
        .with_context(|| format!("installing cached output {}", output.display()))?;
    if cached.path_rewritten {
        record_logical_digest(
            output,
            &prepared.relocation_mappings,
            &cached.sha256,
            cached.size_bytes,
        )?;
    }
    Ok(())
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

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("cache path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating cache directory {}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary cache file beside {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing temporary cache file beside {}", path.display()))?;
    file.as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary cache file beside {}", path.display()))?;
    let temporary = file.into_temp_path();
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

fn blob_path(options: &CacheOptions, digest: &str) -> PathBuf {
    options.root.join("blobs").join(digest)
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
    #[test]
    fn an_action_manifest_of_another_version_or_damaged_is_a_miss() {
        let old = br#"{"schema_version":1,"action_key":"k","outputs":[]}"#;
        assert!(super::read_action_manifest(old).is_none());
        assert!(super::read_action_manifest(b"not json").is_none());
        assert!(super::read_action_manifest(br#"{"schema_version":4}"#).is_none());
        let previous = br#"{"schema_version":3,"action_key":"k","outputs":[],"dep_inputs":[]}"#;
        assert!(super::read_action_manifest(previous).is_none());
        let current = br#"{"schema_version":4,"action_key":"k","outputs":[],"dep_inputs":[]}"#;
        assert!(super::read_action_manifest(current).is_some());
    }

    use super::write_atomic;

    #[test]
    fn failed_publication_removes_only_its_own_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        let occupied = root.path().join("destination");
        std::fs::create_dir(&occupied).unwrap();
        std::fs::write(occupied.join("retained"), b"retain").unwrap();

        assert!(write_atomic(&occupied, b"replacement").is_err());

        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read(occupied.join("retained")).unwrap(), b"retain");
    }
}
