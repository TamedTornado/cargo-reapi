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

const OUTPUT_RECORD_SCHEMA_VERSION: u32 = 2;

/// What every reader of one compile output in a target needs to compute its
/// logical digest: the path mappings its producer normalizes it with. The
/// producer writes the record before the compiler runs, so a pipelined
/// consumer that reads the output early, before the producer has published
/// it, normalizes it exactly as publication will.
#[derive(Debug, Deserialize, Serialize)]
struct OutputRecord {
    schema_version: u32,
    actual_path: PathBuf,
    path_mappings: Vec<(String, String)>,
    /// The digest of the cached blob this file was published as or restored
    /// from, valid while the file still has exactly these bytes.
    verified: Option<VerifiedDigest>,
}

#[derive(Debug, Deserialize, Serialize)]
struct VerifiedDigest {
    actual_size_bytes: u64,
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

fn normalize_artifact_slots(mut bytes: Vec<u8>, mappings: &[(String, String)]) -> Result<Vec<u8>> {
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

/// The bytes a file in a worktree is cached and keyed as: its execution
/// slots become cache slots, and a dependency file's plain root paths become
/// root labels. `mappings` must be those of the compile that produced the
/// file; see [`logical_digest`].
pub fn logical_bytes(
    actual_path: &Path,
    bytes: Vec<u8>,
    mappings: &[(String, String)],
) -> Result<Vec<u8>> {
    let bytes = normalize_artifact_slots(bytes, mappings)?;
    if actual_path
        .extension()
        .is_some_and(|extension| extension == "d")
    {
        return Ok(rewrite_paths_for_cache(bytes, mappings));
    }
    Ok(bytes)
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

/// Reverses the root labels of [`logical_bytes`] for the consumer's roots.
pub fn restore_cached_paths(mut bytes: Vec<u8>, mappings: &[(String, String)]) -> Vec<u8> {
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
    while let Some(found) = memchr::memmem::find(&haystack[offset..], needle) {
        let position = offset + found;
        result.extend_from_slice(&haystack[offset..position]);
        result.extend_from_slice(replacement);
        offset = position + needle.len();
    }
    result.extend_from_slice(&haystack[offset..]);
    result
}

/// Records, before a compile runs, the path mappings its output at
/// `actual_path` is normalized with. Only outputs in the target can be read
/// by another compile, so only they are recorded.
pub fn record_output_mappings(actual_path: &Path, mappings: &[(String, String)]) -> Result<()> {
    let Some(target) = target_root() else {
        return Ok(());
    };
    record_output_mappings_in(&target, actual_path, mappings)
}

fn record_output_mappings_in(
    target: &Path,
    actual_path: &Path,
    mappings: &[(String, String)],
) -> Result<()> {
    let Some(record_path) = output_record_path(target, actual_path) else {
        return Ok(());
    };
    let record = OutputRecord {
        schema_version: OUTPUT_RECORD_SCHEMA_VERSION,
        actual_path: actual_path.to_path_buf(),
        path_mappings: mappings.to_vec(),
        verified: None,
    };
    write_output_record(&record_path, &record)
}

/// Records that the file at `actual_path` now holds the cached blob
/// `logical_sha256`, published or restored with `mappings`.
pub fn record_logical_digest(
    actual_path: &Path,
    mappings: &[(String, String)],
    logical_sha256: &str,
    logical_size_bytes: u64,
) -> Result<()> {
    let Some(target) = target_root() else {
        return Ok(());
    };
    record_logical_digest_in(
        &target,
        actual_path,
        mappings,
        logical_sha256,
        logical_size_bytes,
    )
}

fn record_logical_digest_in(
    target: &Path,
    actual_path: &Path,
    mappings: &[(String, String)],
    logical_sha256: &str,
    logical_size_bytes: u64,
) -> Result<()> {
    let Some(record_path) = output_record_path(target, actual_path) else {
        return Ok(());
    };
    let actual_bytes = fs::read(actual_path)
        .with_context(|| format!("hashing compile output {}", actual_path.display()))?;

    let record = OutputRecord {
        schema_version: OUTPUT_RECORD_SCHEMA_VERSION,
        actual_path: actual_path.to_path_buf(),
        path_mappings: mappings.to_vec(),
        verified: Some(VerifiedDigest {
            actual_size_bytes: u64::try_from(actual_bytes.len())
                .context("compile output is too large")?,
            actual_sha256: format!("{:x}", Sha256::digest(&actual_bytes)),
            logical_sha256: logical_sha256.to_owned(),
            logical_size_bytes,
        }),
    };
    write_output_record(&record_path, &record)
}

/// Readers may open the record at any moment, so it is replaced whole.
fn write_output_record(record_path: &Path, record: &OutputRecord) -> Result<()> {
    let parent = record_path
        .parent()
        .with_context(|| format!("output record has no parent: {}", record_path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating output record directory {}", parent.display()))?;

    let encoded = serde_json::to_vec(record).context("serializing output record")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating output record beside {}", record_path.display()))?;
    file.write_all(&encoded)
        .with_context(|| format!("writing output record {}", record_path.display()))?;
    file.persist(record_path)
        .with_context(|| format!("installing output record {}", record_path.display()))?;
    Ok(())
}

pub fn record_path_mappings(mappings: &[(String, String)]) -> Result<()> {
    let Some(target) = target_root() else {
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

/// The digest and size a file is keyed by.
///
/// A compile output in the target is normalized with its producer's path
/// mappings, so it has one logical digest whichever compile reads it and
/// whether or not its producer has published it yet. A pipelined consumer
/// reads a dependency's metadata while the dependency is still compiling;
/// with its own mappings it would leave the dependency's package slot as a
/// worktree path and key on a digest no other worktree reproduces. Every
/// other file is normalized with the reader's `mappings`.
pub fn logical_digest(actual_path: &Path, mappings: &[(String, String)]) -> Result<(String, u64)> {
    let record = target_root().and_then(|target| read_output_record(&target, actual_path));
    logical_digest_with(actual_path, record, mappings)
}

fn logical_digest_with(
    actual_path: &Path,
    record: Option<OutputRecord>,
    mappings: &[(String, String)],
) -> Result<(String, u64)> {
    let bytes = fs::read(actual_path)
        .with_context(|| format!("reading input {}", actual_path.display()))?;

    let producer_mappings = match record {
        Some(record) => {
            if let Some(verified) = &record.verified
                && verified.actual_size_bytes == u64::try_from(bytes.len())?
                && verified.actual_sha256 == format!("{:x}", Sha256::digest(&bytes))
            {
                return Ok((verified.logical_sha256.clone(), verified.logical_size_bytes));
            }
            record.path_mappings
        }
        None => mappings.to_vec(),
    };

    let logical = logical_bytes(actual_path, bytes, &producer_mappings)?;
    let size_bytes = u64::try_from(logical.len()).context("input file is too large")?;
    Ok((format!("{:x}", Sha256::digest(&logical)), size_bytes))
}

fn read_output_record(target: &Path, actual_path: &Path) -> Option<OutputRecord> {
    let encoded = fs::read(output_record_path(target, actual_path)?).ok()?;
    let record = serde_json::from_slice::<OutputRecord>(&encoded).ok()?;
    (record.schema_version == OUTPUT_RECORD_SCHEMA_VERSION && record.actual_path == actual_path)
        .then_some(record)
}

fn output_record_path(target: &Path, actual_path: &Path) -> Option<PathBuf> {
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
            .join("output-records")
            .join(format!("{key}.json")),
    )
}

fn target_root() -> Option<PathBuf> {
    std::env::var_os("CARGO_REAPI_TARGET_ROOT").map(PathBuf::from)
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

    fn digest_in(target: &Path, artifact: &Path, mappings: &[(String, String)]) -> (String, u64) {
        logical_digest_with(artifact, read_output_record(target, artifact), mappings).unwrap()
    }

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn verified_digest_survives_touch_but_rejects_mutation() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target");
        let artifact = target.join("debug/deps/libfixture.rlib");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, b"relocated artifact").unwrap();
        record_logical_digest_in(&target, &artifact, &[], "logical-digest", 17).unwrap();

        let touched = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        OpenOptions::new()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(touched))
            .unwrap();
        assert_eq!(
            digest_in(&target, &artifact, &[]),
            ("logical-digest".to_owned(), 17)
        );

        fs::write(&artifact, b"poisoned artifact!").unwrap();
        assert_eq!(
            digest_in(&target, &artifact, &[]),
            (sha256(b"poisoned artifact!"), 18)
        );
    }

    #[test]
    fn unpublished_output_is_digested_with_its_producers_mappings() {
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("target");
        let artifact = target.join("debug/deps/libproducer.rmeta");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();

        let producer = vec![
            ("target".to_owned(), target.to_string_lossy().into_owned()),
            ("package".to_owned(), "/worktree/producer".to_owned()),
        ];
        let consumer = vec![
            ("target".to_owned(), target.to_string_lossy().into_owned()),
            ("package".to_owned(), "/worktree/consumer".to_owned()),
        ];
        let metadata = format!(
            "source:{}/src/lib.rs",
            execution_slot("/worktree/producer").unwrap()
        );
        fs::write(&artifact, &metadata).unwrap();

        // What the producer will publish once it finishes.
        let published = logical_bytes(&artifact, metadata.clone().into_bytes(), &producer).unwrap();
        let published = (sha256(&published), published.len() as u64);

        // The producer records its mappings before the compiler runs, so a
        // pipelined consumer reading the output early agrees with it.
        record_output_mappings_in(&target, &artifact, &producer).unwrap();
        assert_eq!(digest_in(&target, &artifact, &consumer), published);

        record_logical_digest_in(&target, &artifact, &producer, &published.0, published.1).unwrap();
        assert_eq!(digest_in(&target, &artifact, &consumer), published);

        // Without the record the consumer's own mappings leave the
        // producer's package slot as a worktree path.
        fs::remove_dir_all(target.join("cargo-reapi")).unwrap();
        assert_ne!(digest_in(&target, &artifact, &consumer), published);
    }
}
