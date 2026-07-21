use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// A fixed-width, path-shaped relocation slot lets cached Rust metadata and
// linked binaries retain valid file offsets while still being materialized for
// worktrees whose absolute paths have different lengths.
pub const RELOCATION_SLOT_BYTES: usize = 256;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RecordedPathMapping {
    pub label: String,
    pub actual: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct LogicalDigestReceipt {
    schema_version: u32,
    actual_path: PathBuf,
    actual_size_bytes: u64,
    modified_unix_ns: u128,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    actual_sha256: String,
    logical_sha256: String,
    logical_size_bytes: u64,
}

pub fn execution_slot(actual: &str) -> Result<String> {
    pad_path(actual)
}

pub fn cache_slot(label: &str) -> Result<String> {
    pad_path(&format!("/__cargo_reapi_cache_slot__/{label}"))
}

fn pad_path(path: &str) -> Result<String> {
    if path.len() > RELOCATION_SLOT_BYTES {
        bail!("path is too long for the {RELOCATION_SLOT_BYTES}-byte relocation slot: {path}");
    }
    let mut padded = String::with_capacity(RELOCATION_SLOT_BYTES);
    padded.push_str(path.trim_end_matches('/'));
    while padded.len() < RELOCATION_SLOT_BYTES {
        padded.push('/');
    }
    Ok(padded)
}

pub fn normalize_artifact_slots(
    mut bytes: Vec<u8>,
    mappings: &[(String, String)],
) -> Result<Vec<u8>> {
    for (label, actual) in mappings {
        bytes = replace_bytes(
            &bytes,
            execution_slot(actual)?.as_bytes(),
            cache_slot(label)?.as_bytes(),
        );
    }
    Ok(bytes)
}

pub fn materialize_artifact_slots(
    mut bytes: Vec<u8>,
    mappings: &[(String, String)],
) -> Result<Vec<u8>> {
    for (label, actual) in mappings.iter().rev() {
        bytes = replace_bytes(
            &bytes,
            cache_slot(label)?.as_bytes(),
            execution_slot(actual)?.as_bytes(),
        );
    }
    Ok(bytes)
}

pub fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    debug_assert_eq!(needle.len(), replacement.len());
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut result = Vec::with_capacity(haystack.len());
    let mut offset = 0;
    while let Some(found) = memchr::memmem::find(&haystack[offset..], needle) {
        let position = offset + found;
        result.extend_from_slice(&haystack[offset..position]);
        result.extend_from_slice(replacement);
        offset = position + needle.len();
    }
    result.extend_from_slice(&haystack[offset..]);
    result
}

pub fn record_logical_digest(
    actual_path: &Path,
    logical_sha256: &str,
    logical_size_bytes: u64,
) -> Result<()> {
    let Some(target) = std::env::var_os("CARGO_REAPI_TARGET_ROOT").map(PathBuf::from) else {
        return Ok(());
    };
    record_logical_digest_for_target(&target, actual_path, logical_sha256, logical_size_bytes)
}

fn record_logical_digest_for_target(
    target: &Path,
    actual_path: &Path,
    logical_sha256: &str,
    logical_size_bytes: u64,
) -> Result<()> {
    let Some(receipt_path) = receipt_path(target, actual_path) else {
        return Ok(());
    };
    let metadata = fs::metadata(actual_path)
        .with_context(|| format!("reading restored output {}", actual_path.display()))?;
    if !requires_logical_digest_receipt(&metadata) {
        return Ok(());
    }
    let actual_bytes = fs::read(actual_path)
        .with_context(|| format!("hashing restored output {}", actual_path.display()))?;
    let receipt = LogicalDigestReceipt {
        schema_version: 1,
        actual_path: actual_path.to_path_buf(),
        actual_size_bytes: metadata.len(),
        modified_unix_ns: modified_unix_ns(&metadata)?,
        #[cfg(unix)]
        device: unix_device(&metadata),
        #[cfg(unix)]
        inode: unix_inode(&metadata),
        actual_sha256: format!("{:x}", Sha256::digest(actual_bytes)),
        logical_sha256: logical_sha256.to_owned(),
        logical_size_bytes,
    };
    if let Some(parent) = receipt_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        &receipt_path,
        serde_json::to_vec(&receipt).context("serializing logical digest receipt")?,
    )
    .with_context(|| format!("writing logical digest receipt {}", receipt_path.display()))
}

pub fn record_path_mappings(mappings: &[(String, String)]) -> Result<()> {
    let Some(target) = std::env::var_os("CARGO_REAPI_TARGET_ROOT").map(PathBuf::from) else {
        return Ok(());
    };
    let root = target.join("cargo-reapi").join("path-mappings");
    fs::create_dir_all(&root)
        .with_context(|| format!("creating path mapping directory {}", root.display()))?;
    for (label, actual) in mappings {
        let mapping = RecordedPathMapping {
            label: label.clone(),
            actual: actual.clone(),
        };
        let encoded = serde_json::to_vec(&mapping).context("serializing path mapping")?;
        let key = format!("{:x}", Sha256::digest(&encoded));
        let path = root.join(format!("{key}.json"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => file
                .write_all(&encoded)
                .with_context(|| format!("recording path mapping {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating path mapping {}", path.display()));
            }
        }
    }
    Ok(())
}

pub fn restored_logical_digest(actual_path: &Path) -> Result<Option<(String, u64)>> {
    let Some(target) = std::env::var_os("CARGO_REAPI_TARGET_ROOT").map(PathBuf::from) else {
        return Ok(None);
    };
    restored_logical_digest_for_target(&target, actual_path)
}

fn restored_logical_digest_for_target(
    target: &Path,
    actual_path: &Path,
) -> Result<Option<(String, u64)>> {
    let Some(receipt_path) = receipt_path(target, actual_path) else {
        return Ok(None);
    };
    let Ok(encoded) = fs::read(&receipt_path) else {
        return Ok(None);
    };
    let Ok(receipt) = serde_json::from_slice::<LogicalDigestReceipt>(&encoded) else {
        return Ok(None);
    };
    let Ok(metadata) = fs::metadata(actual_path) else {
        return Ok(None);
    };
    if !requires_logical_digest_receipt(&metadata) {
        return Ok(None);
    }
    if receipt.schema_version != 1
        || receipt.actual_path != actual_path
        || receipt.actual_size_bytes != metadata.len()
    {
        return Ok(None);
    }
    let actual_bytes = fs::read(actual_path)
        .with_context(|| format!("verifying restored output {}", actual_path.display()))?;
    if format!("{:x}", Sha256::digest(actual_bytes)) != receipt.actual_sha256 {
        return Ok(None);
    }
    Ok(Some((receipt.logical_sha256, receipt.logical_size_bytes)))
}

fn receipt_path(target: &Path, actual_path: &Path) -> Option<PathBuf> {
    if !actual_path.starts_with(target) {
        return None;
    }
    let key = format!(
        "{:x}",
        Sha256::digest(actual_path.to_string_lossy().as_bytes())
    );
    Some(
        target
            .join("cargo-reapi")
            .join("logical-digests")
            .join(format!("{key}.json")),
    )
}

fn modified_unix_ns(metadata: &fs::Metadata) -> Result<u128> {
    Ok(metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .context("output modification time predates Unix epoch")?
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

fn requires_logical_digest_receipt(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_fixed_width_and_execution_slots_remain_equivalent_paths() {
        let fixture = tempfile::tempdir().expect("fixture path");
        let actual = fixture.path().to_string_lossy();
        let execution = execution_slot(&actual).expect("execution slot");
        let cached = cache_slot("package").expect("cache slot");
        assert_eq!(execution.len(), RELOCATION_SLOT_BYTES);
        assert_eq!(cached.len(), RELOCATION_SLOT_BYTES);
        assert_eq!(
            std::fs::canonicalize(&execution).ok(),
            std::fs::canonicalize(fixture.path()).ok()
        );
    }

    #[test]
    fn artifact_slot_round_trip_preserves_length_and_uses_the_consumer_path() {
        let producer = vec![("package".to_owned(), "/tmp/producer/package".to_owned())];
        let consumer = vec![(
            "package".to_owned(),
            "/tmp/a-much-longer-consumer/package".to_owned(),
        )];
        let original = format!("before:{}:after", execution_slot(&producer[0].1).unwrap());
        let cached = normalize_artifact_slots(original.into_bytes(), &producer).unwrap();
        let restored = materialize_artifact_slots(cached, &consumer).unwrap();
        let restored = String::from_utf8(restored).unwrap();
        assert!(restored.contains(&execution_slot(&consumer[0].1).unwrap()));
    }

    #[test]
    fn logical_receipt_for_non_executable_survives_touch_but_rejects_mutation() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target");
        let artifact = target.join("debug/deps/libfixture.rlib");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, b"relocated artifact").unwrap();
        record_logical_digest_for_target(&target, &artifact, "logical-digest", 17).unwrap();

        let touched = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        OpenOptions::new()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(touched))
            .unwrap();
        assert_eq!(
            restored_logical_digest_for_target(&target, &artifact).unwrap(),
            Some(("logical-digest".to_owned(), 17))
        );

        fs::write(&artifact, b"poisoned artifact!").unwrap();
        assert_eq!(
            restored_logical_digest_for_target(&target, &artifact).unwrap(),
            None
        );
    }
}
