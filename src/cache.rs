use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capture::{CaptureOptions, PreparedInvocation, prepare_invocation, record_invocation};
use crate::invocation::RustcInvocation;

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

pub fn execute_cached(
    invocation: &RustcInvocation,
    capture_options: &CaptureOptions,
    cache_options: &CacheOptions,
) -> Result<i32> {
    let prepared = prepare_invocation(invocation)?;
    if !prepared.remote_eligibility.eligible {
        let exit_code = invocation.execute()?;
        record_invocation(capture_options, &prepared, "local-ineligible", exit_code)?;
        return Ok(exit_code);
    }

    fs::create_dir_all(cache_options.root.join("locks"))
        .context("creating action lock directory")?;
    let lock_path = cache_options
        .root
        .join("locks")
        .join(format!("{}.lock", prepared.action_key));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening action lock {}", lock_path.display()))?;
    lock.lock_exclusive()
        .with_context(|| format!("locking action {}", prepared.action_key))?;

    let result = if restore(cache_options, &prepared)? {
        record_invocation(capture_options, &prepared, "cache-hit", 0)?;
        0
    } else {
        let exit_code = invocation.execute()?;
        let execution = if exit_code == 0 && publish(cache_options, &prepared)? {
            "local-cache-miss"
        } else if exit_code == 0 {
            "local-output-incomplete"
        } else {
            "local-failed"
        };
        record_invocation(capture_options, &prepared, execution, exit_code)?;
        exit_code
    };

    FileExt::unlock(&lock).context("unlocking action cache entry")?;
    Ok(result)
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
        let mut bytes = fs::read(&output.actual_path)
            .with_context(|| format!("reading output {}", output.actual_path.display()))?;
        let path_rewritten = output
            .actual_path
            .extension()
            .is_some_and(|extension| extension == "d");
        if path_rewritten {
            bytes = rewrite_paths_for_cache(bytes, &prepared.path_mappings);
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        publish_blob(options, &sha256, &bytes)?;
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
        .with_context(|| format!("installing cached output {}", output.display()))
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
