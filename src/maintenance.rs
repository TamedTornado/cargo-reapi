use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Serialize;
use walkdir::WalkDir;

const MAINTENANCE_LOCK: &str = ".maintenance.lock";

pub struct CacheSharedGuard {
    lock: File,
}

impl Drop for CacheSharedGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock);
    }
}

pub fn acquire_shared(cache_root: &Path) -> Result<CacheSharedGuard> {
    fs::create_dir_all(cache_root)?;
    let lock = open_lock(cache_root)?;
    FileExt::lock_shared(&lock).context("acquiring shared cargo-reapi cache maintenance lock")?;
    Ok(CacheSharedGuard { lock })
}

fn open_lock(cache_root: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cache_root.join(MAINTENANCE_LOCK))
        .context("opening cargo-reapi cache maintenance lock")
}

#[derive(Clone, Copy)]
pub enum AccessKind {
    Action,
    Gate,
}

impl AccessKind {
    fn directory(self) -> &'static str {
        match self {
            Self::Action => "actions",
            Self::Gate => "gates",
        }
    }
}

pub fn record_access(cache_root: &Path, kind: AccessKind, key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("cache access key contains unsupported characters");
    }
    let directory = cache_root.join("access").join(kind.directory());
    fs::create_dir_all(&directory)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    fs::write(directory.join(key), format!("{timestamp}\n"))?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct CacheStats {
    pub schema_version: u32,
    pub cache_root: PathBuf,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub action_entries: usize,
    pub gate_entries: usize,
    pub blob_entries: usize,
}

pub fn cache_stats(cache_root: &Path) -> Result<CacheStats> {
    fs::create_dir_all(cache_root)?;
    Ok(CacheStats {
        schema_version: 1,
        cache_root: cache_root.to_path_buf(),
        total_bytes: directory_size(cache_root)?,
        available_bytes: fs2::available_space(cache_root)?,
        action_entries: entry_count(&cache_root.join("actions"), false)?,
        gate_entries: entry_count(&cache_root.join("gate-snapshots/objects"), true)?,
        blob_entries: entry_count(&cache_root.join("blobs"), false)?,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct GcOptions {
    pub max_bytes: u64,
    pub min_free_bytes: u64,
    pub target_free_bytes: u64,
    pub dry_run: bool,
}

#[derive(Debug, Serialize)]
pub struct GcReport {
    pub schema_version: u32,
    pub dry_run: bool,
    pub before: CacheStats,
    pub after: CacheStats,
    pub removed_action_entries: usize,
    pub removed_gate_entries: usize,
    pub removed_auxiliary_entries: usize,
    pub removed_blob_entries: usize,
    pub estimated_freed_bytes: u64,
    pub capacity_satisfied: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandidateKind {
    Action,
    Gate,
    Auxiliary,
}

#[derive(Debug)]
struct Candidate {
    kind: CandidateKind,
    key: String,
    path: PathBuf,
    last_used: SystemTime,
}

pub fn collect_garbage(cache_root: &Path, options: GcOptions) -> Result<GcReport> {
    if options.target_free_bytes < options.min_free_bytes {
        bail!("target free bytes must be greater than or equal to minimum free bytes");
    }
    fs::create_dir_all(cache_root)?;
    let lock = open_lock(cache_root)?;
    FileExt::lock_exclusive(&lock)
        .context("acquiring exclusive cargo-reapi cache maintenance lock")?;
    let before = cache_stats(cache_root)?;
    let recovering_free_space = before.available_bytes < options.min_free_bytes;
    let mut candidates = candidates(cache_root)?;
    candidates.sort_by_key(|candidate| candidate.last_used);
    let mut projected_bytes = before.total_bytes;
    let mut projected_free = before.available_bytes;
    let mut removed_action_entries = 0;
    let mut removed_gate_entries = 0;
    let mut removed_auxiliary_entries = 0;
    let mut removed_blob_entries = 0;

    for candidate in candidates {
        let within_limit = projected_bytes <= options.max_bytes;
        let free_recovered = !recovering_free_space || projected_free >= options.target_free_bytes;
        if within_limit && free_recovered {
            break;
        }
        let entry_bytes = path_size(&candidate.path)?;
        if !options.dry_run {
            remove_path(&candidate.path)?;
            remove_access_marker(cache_root, &candidate);
        }
        projected_bytes = projected_bytes.saturating_sub(entry_bytes);
        projected_free = projected_free.saturating_add(entry_bytes);
        match candidate.kind {
            CandidateKind::Action => removed_action_entries += 1,
            CandidateKind::Gate => removed_gate_entries += 1,
            CandidateKind::Auxiliary => removed_auxiliary_entries += 1,
        }
    }

    if !options.dry_run {
        removed_blob_entries = sweep_unreferenced_blobs(cache_root)?;
    }
    let after = if options.dry_run {
        CacheStats {
            schema_version: before.schema_version,
            cache_root: before.cache_root.clone(),
            total_bytes: projected_bytes,
            available_bytes: projected_free,
            action_entries: before.action_entries.saturating_sub(removed_action_entries),
            gate_entries: before.gate_entries.saturating_sub(removed_gate_entries),
            blob_entries: before.blob_entries,
        }
    } else {
        cache_stats(cache_root)?
    };
    FileExt::unlock(&lock).context("unlocking cargo-reapi cache maintenance lock")?;
    let capacity_satisfied = after.total_bytes <= options.max_bytes
        && (!recovering_free_space || after.available_bytes >= options.target_free_bytes);
    Ok(GcReport {
        schema_version: 1,
        dry_run: options.dry_run,
        estimated_freed_bytes: before.total_bytes.saturating_sub(after.total_bytes),
        before,
        after,
        removed_action_entries,
        removed_gate_entries,
        removed_auxiliary_entries,
        removed_blob_entries,
        capacity_satisfied,
    })
}

fn candidates(cache_root: &Path) -> Result<Vec<Candidate>> {
    let mut result = Vec::new();
    add_file_candidates(
        cache_root,
        &cache_root.join("actions"),
        CandidateKind::Action,
        Some(AccessKind::Action),
        &mut result,
    )?;
    add_directory_candidates(
        cache_root,
        &cache_root.join("gate-snapshots/objects"),
        CandidateKind::Gate,
        Some(AccessKind::Gate),
        &mut result,
    )?;
    for auxiliary in ["discoveries", "link-captures", "file-digests"] {
        add_file_candidates(
            cache_root,
            &cache_root.join(auxiliary),
            CandidateKind::Auxiliary,
            None,
            &mut result,
        )?;
    }
    Ok(result)
}

fn add_file_candidates(
    cache_root: &Path,
    directory: &Path,
    kind: CandidateKind,
    access_kind: Option<AccessKind>,
    result: &mut Vec<Candidate>,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let key = entry
            .path()
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_owned();
        result.push(Candidate {
            last_used: last_used(cache_root, access_kind, &key, &entry.path())?,
            kind,
            key,
            path: entry.path(),
        });
    }
    Ok(())
}

fn add_directory_candidates(
    cache_root: &Path,
    directory: &Path,
    kind: CandidateKind,
    access_kind: Option<AccessKind>,
    result: &mut Vec<Candidate>,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let key = entry.file_name().to_string_lossy().into_owned();
        result.push(Candidate {
            last_used: last_used(cache_root, access_kind, &key, &entry.path())?,
            kind,
            key,
            path: entry.path(),
        });
    }
    Ok(())
}

fn last_used(
    cache_root: &Path,
    kind: Option<AccessKind>,
    key: &str,
    fallback: &Path,
) -> Result<SystemTime> {
    if let Some(kind) = kind {
        let marker = cache_root.join("access").join(kind.directory()).join(key);
        if let Ok(metadata) = fs::metadata(marker) {
            return metadata
                .modified()
                .context("reading cache access timestamp");
        }
    }
    fs::metadata(fallback)?
        .modified()
        .context("reading cache entry timestamp")
}

fn remove_access_marker(cache_root: &Path, candidate: &Candidate) {
    let kind = match candidate.kind {
        CandidateKind::Action => Some(AccessKind::Action),
        CandidateKind::Gate => Some(AccessKind::Gate),
        CandidateKind::Auxiliary => None,
    };
    if let Some(kind) = kind {
        let _ = fs::remove_file(
            cache_root
                .join("access")
                .join(kind.directory())
                .join(&candidate.key),
        );
    }
}

fn sweep_unreferenced_blobs(cache_root: &Path) -> Result<usize> {
    let mut referenced = BTreeSet::new();
    if let Ok(actions) = fs::read_dir(cache_root.join("actions")) {
        for action in actions {
            let action = action?;
            if !action.file_type()?.is_file() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_slice(&fs::read(action.path())?)?;
            let outputs = value
                .get("outputs")
                .and_then(serde_json::Value::as_array)
                .context("action manifest has no outputs array")?;
            for output in outputs {
                let digest = output
                    .get("sha256")
                    .and_then(serde_json::Value::as_str)
                    .context("action manifest output has no sha256")?;
                referenced.insert(digest.to_owned());
            }
        }
    }
    let mut removed = 0;
    if let Ok(blobs) = fs::read_dir(cache_root.join("blobs")) {
        for blob in blobs {
            let blob = blob?;
            if !blob.file_type()?.is_file() {
                continue;
            }
            if !referenced.contains(&blob.file_name().to_string_lossy().into_owned()) {
                fs::remove_file(blob.path())?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

fn remove_path(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn entry_count(directory: &Path, directories: bool) -> Result<usize> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(0);
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if (directories && file_type.is_dir()) || (!directories && file_type.is_file()) {
            count += 1;
        }
    }
    Ok(count)
}

fn directory_size(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut size = 0_u64;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            size = size.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(size)
}

fn path_size(path: &Path) -> Result<u64> {
    if path.is_dir() {
        directory_size(path)
    } else {
        Ok(fs::metadata(path)?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn gc_retains_blobs_referenced_by_surviving_actions() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("actions")).unwrap();
        fs::create_dir_all(root.path().join("blobs")).unwrap();
        fs::write(root.path().join("blobs/keep"), b"kept").unwrap();
        fs::write(root.path().join("blobs/drop"), b"dropped").unwrap();
        fs::write(
            root.path().join("actions/action.json"),
            br#"{"schema_version":1,"action_key":"action","outputs":[{"sha256":"keep"}]}"#,
        )
        .unwrap();
        let report = collect_garbage(
            root.path(),
            GcOptions {
                max_bytes: u64::MAX,
                min_free_bytes: 0,
                target_free_bytes: 0,
                dry_run: false,
            },
        )
        .unwrap();
        assert!(root.path().join("blobs/keep").exists());
        assert!(!root.path().join("blobs/drop").exists());
        assert_eq!(report.removed_blob_entries, 1);
    }

    #[test]
    fn dry_run_reports_eviction_without_mutating_entries() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("gate-snapshots/objects/gate")).unwrap();
        fs::write(
            root.path().join("gate-snapshots/objects/gate/data"),
            vec![0_u8; 1024],
        )
        .unwrap();
        let report = collect_garbage(
            root.path(),
            GcOptions {
                max_bytes: 0,
                min_free_bytes: 0,
                target_free_bytes: 0,
                dry_run: true,
            },
        )
        .unwrap();
        assert_eq!(report.removed_gate_entries, 1);
        assert!(root.path().join("gate-snapshots/objects/gate").exists());
    }

    #[test]
    fn gc_waits_for_active_cache_operations() {
        let root = tempdir().unwrap();
        let guard = acquire_shared(root.path()).unwrap();
        let cache_root = root.path().to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = collect_garbage(
                &cache_root,
                GcOptions {
                    max_bytes: u64::MAX,
                    min_free_bytes: 0,
                    target_free_bytes: 0,
                    dry_run: false,
                },
            );
            sender.send(result).unwrap();
        });

        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
        drop(guard);
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("GC should proceed after the active cache operation ends")
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn gc_evicts_least_recently_used_entries_first() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("actions")).unwrap();
        fs::write(
            root.path().join("actions/older.json"),
            format!(r#"{{"outputs":[]}}{}"#, " ".repeat(1024)),
        )
        .unwrap();
        fs::write(
            root.path().join("actions/newer.json"),
            format!(r#"{{"outputs":[]}}{}"#, " ".repeat(1024)),
        )
        .unwrap();
        record_access(root.path(), AccessKind::Action, "older").unwrap();
        thread::sleep(Duration::from_millis(10));
        record_access(root.path(), AccessKind::Action, "newer").unwrap();
        let before = cache_stats(root.path()).unwrap();
        let report = collect_garbage(
            root.path(),
            GcOptions {
                max_bytes: before.total_bytes - 1024,
                min_free_bytes: 0,
                target_free_bytes: 0,
                dry_run: false,
            },
        )
        .unwrap();

        assert_eq!(report.removed_action_entries, 1);
        assert!(!root.path().join("actions/older.json").exists());
        assert!(root.path().join("actions/newer.json").exists());
    }
}
