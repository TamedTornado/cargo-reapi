use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Serialize;
use walkdir::WalkDir;

use crate::footprint::{Footprint, FootprintBuilder, OwnerId};

const MAINTENANCE_LOCK: &str = ".maintenance.lock";
const MAINTENANCE_ADMISSION_LOCK: &str = ".maintenance-admission.lock";

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
    // A collector holds admission while draining existing readers. New readers
    // cannot repeatedly barge ahead of its exclusive maintenance request.
    let admission = open_lock(cache_root, MAINTENANCE_ADMISSION_LOCK)?;
    FileExt::lock_exclusive(&admission).context("entering cargo-reapi cache admission")?;
    let lock = open_lock(cache_root, MAINTENANCE_LOCK)?;
    FileExt::lock_shared(&lock).context("acquiring shared cargo-reapi cache maintenance lock")?;
    FileExt::unlock(&admission).context("leaving cargo-reapi cache admission")?;
    Ok(CacheSharedGuard { lock })
}

fn open_lock(cache_root: &Path, name: &str) -> Result<File> {
    crate::safe_io::open_lock(&cache_root.join(name))
        .context("opening cargo-reapi cache maintenance lock")
}

/// Records that the action `key` was used, so that collection evicts the
/// least recently used first.
pub fn record_access(cache_root: &Path, key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("cache access key contains unsupported characters");
    }
    let directory = cache_root.join("access/actions");
    fs::create_dir_all(&directory)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    crate::safe_io::write_replacing(&directory.join(key), format!("{timestamp}\n").as_bytes())?;
    Ok(())
}

/// Schema 2 reports the physical footprint as `total_bytes`; schema 1 summed
/// file lengths there.
const REPORT_SCHEMA_VERSION: u32 = 2;

/// Owner of every file no collection removes: locks, access markers, the
/// resource ledger and anything else outside the eviction candidates.
const PINNED: OwnerId = 0;

#[derive(Clone, Debug, Serialize)]
pub struct CacheStats {
    pub schema_version: u32,
    pub cache_root: PathBuf,
    /// Physical bytes the cache occupies, each shared block counted once.
    pub total_bytes: u64,
    /// Sum of file lengths.
    pub apparent_bytes: u64,
    pub available_bytes: u64,
    pub action_entries: usize,
    pub blob_entries: usize,
}

pub fn cache_stats(cache_root: &Path) -> Result<CacheStats> {
    fs::create_dir_all(cache_root)?;
    let footprint = measure_footprint(cache_root, &HashMap::new())?;
    stats(
        cache_root,
        footprint.total_bytes(),
        directory_size(cache_root)?,
    )
}

fn stats(cache_root: &Path, total_bytes: u64, apparent_bytes: u64) -> Result<CacheStats> {
    Ok(CacheStats {
        schema_version: REPORT_SCHEMA_VERSION,
        cache_root: cache_root.to_path_buf(),
        total_bytes,
        apparent_bytes,
        available_bytes: fs2::available_space(cache_root)?,
        action_entries: entry_count(&cache_root.join("actions"), false)?,
        blob_entries: entry_count(&cache_root.join("blobs"), false)?,
    })
}

/// Attributes each file to the removable unit containing it: the nearest
/// ancestor (or the file itself) registered in `owners`, else [`PINNED`].
fn measure_footprint(cache_root: &Path, owners: &HashMap<PathBuf, OwnerId>) -> Result<Footprint> {
    let mut builder = FootprintBuilder::default();
    for file in cache_files(cache_root)? {
        let owner = file
            .ancestors()
            .take_while(|ancestor| *ancestor != cache_root)
            .find_map(|ancestor| owners.get(ancestor))
            .copied()
            .unwrap_or(PINNED);
        builder.add_file(owner, &file)?;
    }
    Ok(builder.build())
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
    /// Directories earlier versions kept and this one does not use, removed
    /// whole (see [`OBSOLETE_DIRECTORIES`]).
    pub removed_obsolete_directories: usize,
    pub removed_blob_entries: usize,
    pub estimated_freed_bytes: u64,
    pub capacity_satisfied: bool,
}

#[derive(Debug)]
struct Candidate {
    key: String,
    path: PathBuf,
    last_used: SystemTime,
}

pub fn collect_garbage(cache_root: &Path, options: GcOptions) -> Result<GcReport> {
    if options.target_free_bytes < options.min_free_bytes {
        bail!("target free bytes must be greater than or equal to minimum free bytes");
    }
    fs::create_dir_all(cache_root)?;
    let admission = open_lock(cache_root, MAINTENANCE_ADMISSION_LOCK)?;
    FileExt::lock_exclusive(&admission).context("closing cargo-reapi cache admission for GC")?;
    let lock = open_lock(cache_root, MAINTENANCE_LOCK)?;
    FileExt::lock_exclusive(&lock)
        .context("acquiring exclusive cargo-reapi cache maintenance lock")?;

    let Units {
        obsolete,
        candidates,
        blobs: blob_owners,
        owners,
    } = removable_units(cache_root)?;
    let mut removal = Removal {
        footprint: measure_footprint(cache_root, &owners)?,
        apparent_bytes: directory_size(cache_root)?,
        dry_run: options.dry_run,
    };
    let before = stats(
        cache_root,
        removal.footprint.total_bytes(),
        removal.apparent_bytes,
    )?;

    let recovering_free_space = before.available_bytes < options.min_free_bytes;
    let mut projected_free = before.available_bytes;
    let mut removed_action_entries = 0;
    let mut removed_blob_entries = 0;

    // What earlier versions kept is never a reuse candidate, yet it occupies
    // the disk. Left in place it can exceed the budget on its own, and every
    // collection would then evict all reusable entries without satisfying it.
    let removed_obsolete_directories = obsolete.len();
    for (staging, owner) in &obsolete {
        projected_free = projected_free.saturating_add(removal.remove(staging, *owner)?);
    }

    // An action's real size is its output blobs. A blob is freed only once no
    // surviving action references it; unreferenced blobs go immediately.
    let mut references = BlobReferences::of(&candidates)?;
    for (digest, (owner, path)) in &blob_owners {
        if !references.is_referenced(digest) {
            projected_free = projected_free.saturating_add(removal.remove(path, *owner)?);
            removed_blob_entries += 1;
        }
    }

    for (index, (candidate, owner)) in candidates.iter().enumerate() {
        let within_limit = removal.footprint.total_bytes() <= options.max_bytes;
        let free_recovered = !recovering_free_space || projected_free >= options.target_free_bytes;
        if within_limit && free_recovered {
            break;
        }
        let mut freed = removal.remove(&candidate.path, *owner)?;
        if !options.dry_run {
            remove_access_marker(cache_root, candidate);
        }
        for digest in references.release(index) {
            if let Some((blob_owner, path)) = blob_owners.get(&digest) {
                freed = freed.saturating_add(removal.remove(path, *blob_owner)?);
                removed_blob_entries += 1;
            }
        }
        // Blocks shared outside the cache stay allocated when it lets go of
        // them, so a real collection measures recovered space directly.
        projected_free = if recovering_free_space && !options.dry_run {
            fs2::available_space(cache_root)?
        } else {
            projected_free.saturating_add(freed)
        };
        removed_action_entries += 1;
    }

    let after = if options.dry_run {
        CacheStats {
            total_bytes: removal.footprint.total_bytes(),
            apparent_bytes: removal.apparent_bytes,
            available_bytes: projected_free,
            // Entry counts skip dot-files; removals also cover stray temporaries.
            action_entries: before.action_entries.saturating_sub(removed_action_entries),
            blob_entries: before.blob_entries.saturating_sub(removed_blob_entries),
            ..before.clone()
        }
    } else {
        // Nothing may remain unreferenced, including blobs an action manifest
        // listed but this collection did not observe.
        removed_blob_entries += sweep_unreferenced_blobs(cache_root)?;
        cache_stats(cache_root)?
    };
    FileExt::unlock(&lock).context("unlocking cargo-reapi cache maintenance lock")?;
    FileExt::unlock(&admission).context("reopening cargo-reapi cache admission after GC")?;
    let capacity_satisfied = after.total_bytes <= options.max_bytes
        && (!recovering_free_space || after.available_bytes >= options.target_free_bytes);
    Ok(GcReport {
        schema_version: REPORT_SCHEMA_VERSION,
        dry_run: options.dry_run,
        estimated_freed_bytes: before.total_bytes.saturating_sub(after.total_bytes),
        before,
        after,
        removed_action_entries,
        removed_obsolete_directories,
        removed_blob_entries,
        capacity_satisfied,
    })
}

/// Everything a collection may remove. Each unit owns its files, so the
/// footprint knows exactly which physical bytes removing it frees.
struct Units {
    obsolete: Vec<(PathBuf, OwnerId)>,
    /// Least recently used first.
    candidates: Vec<(Candidate, OwnerId)>,
    blobs: BTreeMap<String, (OwnerId, PathBuf)>,
    owners: HashMap<PathBuf, OwnerId>,
}

/// Call only under the exclusive maintenance lock.
fn removable_units(cache_root: &Path) -> Result<Units> {
    let mut owners = HashMap::new();
    let mut register = |path: &Path| {
        let owner = PINNED + 1 + owners.len();
        owners.insert(path.to_path_buf(), owner);
        owner
    };

    let obsolete = obsolete_directories(cache_root)
        .into_iter()
        .into_iter()
        .map(|path| {
            let owner = register(&path);
            (path, owner)
        })
        .collect();
    let mut candidates = candidates(cache_root)?;
    candidates.sort_by_key(|candidate| candidate.last_used);
    let candidates = candidates
        .into_iter()
        .map(|candidate| {
            let owner = register(&candidate.path);
            (candidate, owner)
        })
        .collect();
    let blobs = blob_files(cache_root)?
        .into_iter()
        .map(|(digest, path)| (digest, (register(&path), path)))
        .collect();

    Ok(Units {
        obsolete,
        candidates,
        blobs,
        owners,
    })
}

/// Removes whole units under the exclusive lock and tracks what that frees.
struct Removal {
    footprint: Footprint,
    apparent_bytes: u64,
    dry_run: bool,
}

impl Removal {
    /// Removes one unit (unless this is a dry run) and returns the physical
    /// bytes no remaining unit references.
    fn remove(&mut self, path: &Path, owner: OwnerId) -> Result<u64> {
        self.apparent_bytes = self.apparent_bytes.saturating_sub(path_size(path)?);
        if !self.dry_run {
            remove_path(path)?;
        }
        Ok(self.footprint.release(owner))
    }
}

/// How many candidate actions still reference each blob.
struct BlobReferences {
    /// Output digests per candidate, in candidate order; empty for non-actions.
    outputs: Vec<Vec<String>>,
    counts: BTreeMap<String, usize>,
}

impl BlobReferences {
    fn of(candidates: &[(Candidate, OwnerId)]) -> Result<Self> {
        let outputs = candidates
            .iter()
            .map(|(candidate, _)| action_outputs(&candidate.path))
            .collect::<Result<Vec<_>>>()?;
        let mut counts = BTreeMap::<String, usize>::new();
        for digest in outputs.iter().flatten() {
            *counts.entry(digest.clone()).or_default() += 1;
        }
        Ok(Self { outputs, counts })
    }

    fn is_referenced(&self, digest: &str) -> bool {
        self.counts.contains_key(digest)
    }

    /// Drops one evicted candidate's references and returns the digests no
    /// remaining action references.
    fn release(&mut self, candidate: usize) -> Vec<String> {
        let mut unreferenced = Vec::new();
        for digest in std::mem::take(&mut self.outputs[candidate]) {
            let count = self
                .counts
                .get_mut(&digest)
                .expect("every output digest was counted");
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&digest);
                unreferenced.push(digest);
            }
        }
        unreferenced
    }
}

/// The output digests an action manifest references.
fn action_outputs(manifest: &Path) -> Result<Vec<String>> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(manifest)?)?;
    let outputs = value
        .get("outputs")
        .and_then(serde_json::Value::as_array)
        .context("action manifest has no outputs array")?;
    outputs
        .iter()
        .map(|output| {
            output
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .context("action manifest output has no sha256")
        })
        .collect()
}

fn blob_files(cache_root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let Ok(entries) = fs::read_dir(cache_root.join("blobs")) else {
        return Ok(Vec::new());
    };
    let mut blobs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            blobs.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            ));
        }
    }
    Ok(blobs)
}

/// Every regular file in the cache. Statistics observe a live shared cache, so
/// files another process removes during the traversal are skipped.
fn cache_files(cache_root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(cache_root).follow_links(false) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error)
                if error
                    .io_error()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    Ok(files)
}

/// Directories earlier versions kept in the cache and this one never reads:
/// whole-gate snapshots and their access records, the query cache, and the
/// link discoveries and digest memos of native-input caching. They are
/// removed whole by the first collection, so an upgraded cache does not keep
/// gigabytes nothing will use.
const OBSOLETE_DIRECTORIES: [&str; 6] = [
    "gate-snapshots",
    "access/gates",
    "rustc-query-cache-v1",
    "discoveries",
    "link-captures",
    "file-digests",
];

fn obsolete_directories(cache_root: &Path) -> Vec<PathBuf> {
    OBSOLETE_DIRECTORIES
        .iter()
        .map(|name| cache_root.join(name))
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .collect()
}

/// The action entries a collection may evict, with when each was last used.
fn candidates(cache_root: &Path) -> Result<Vec<Candidate>> {
    let Ok(entries) = fs::read_dir(cache_root.join("actions")) else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
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
        let marker = cache_root.join("access/actions").join(&key);
        let last_used = match fs::metadata(marker) {
            Ok(metadata) => metadata
                .modified()
                .context("reading cache access timestamp")?,
            Err(_) => fs::metadata(entry.path())?
                .modified()
                .context("reading cache entry timestamp")?,
        };
        result.push(Candidate {
            key,
            path: entry.path(),
            last_used,
        });
    }
    Ok(result)
}

fn remove_access_marker(cache_root: &Path, candidate: &Candidate) {
    let _ = fs::remove_file(cache_root.join("access/actions").join(&candidate.key));
}

fn sweep_unreferenced_blobs(cache_root: &Path) -> Result<usize> {
    let mut referenced = BTreeSet::new();
    if let Ok(actions) = fs::read_dir(cache_root.join("actions")) {
        for action in actions {
            let action = action?;
            if !action.file_type()?.is_file() {
                continue;
            }
            referenced.extend(action_outputs(&action.path())?);
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
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
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
        // Statistics observe a live shared cache: another process may publish
        // or remove its own temporary files while this traversal is underway.
        let entry = match entry {
            Ok(entry) => entry,
            Err(error)
                if error
                    .io_error()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if entry.file_type().is_file() {
            match entry.metadata() {
                Ok(metadata) => size = size.saturating_add(metadata.len()),
                Err(error)
                    if error
                        .io_error()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
                Err(error) => return Err(error.into()),
            }
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
    use std::fs::OpenOptions;
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
        write_action(root.path(), "action", &["action-blob"], 1024);
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
        assert_eq!(report.removed_action_entries, 1);
        assert!(root.path().join("actions/action.json").exists());
        assert!(root.path().join("blobs/action-blob").exists());
    }

    fn write_obsolete(root: &Path, name: &str, bytes: usize) -> PathBuf {
        let directory = root.join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("data"), vec![0_u8; bytes]).unwrap();
        directory
    }

    #[test]
    fn gc_removes_directories_earlier_versions_kept_before_evicting_reusable_entries() {
        let root = tempdir().unwrap();
        write_action(root.path(), "reusable", &["reusable-blob"], 1024);
        let snapshots = write_obsolete(root.path(), "gate-snapshots/objects", 64 * 1024);
        let gates = write_obsolete(root.path(), "access/gates", 16);
        let discoveries = write_obsolete(root.path(), "discoveries", 16);

        // The budget fits the reusable entry but not the obsolete snapshots.
        let report = collect_within(root.path(), 8 * 1024);

        for obsolete in [&snapshots, &gates, &discoveries] {
            assert!(!obsolete.exists(), "{obsolete:?} was kept");
        }
        assert!(root.path().join("actions/reusable.json").exists());
        assert_eq!(report.removed_obsolete_directories, 3);
        assert_eq!(report.removed_action_entries, 0);
        assert!(report.capacity_satisfied);
    }

    #[test]
    fn dry_run_reports_obsolete_directories_without_removing_them() {
        let root = tempdir().unwrap();
        let snapshots = write_obsolete(root.path(), "gate-snapshots", 1024);

        let report = collect_garbage(
            root.path(),
            GcOptions {
                max_bytes: u64::MAX,
                min_free_bytes: 0,
                target_free_bytes: 0,
                dry_run: true,
            },
        )
        .unwrap();

        assert_eq!(report.removed_obsolete_directories, 1);
        assert!(snapshots.exists());
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
    fn waiting_gc_closes_admission_to_new_readers() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("blobs")).unwrap();
        fs::write(root.path().join("blobs/unreferenced"), b"discardable").unwrap();
        let active = acquire_shared(root.path()).unwrap();
        let admission = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.path().join(".maintenance-admission.lock"))
            .unwrap();
        let cache_root = root.path().to_path_buf();
        let collector = thread::spawn(move || {
            collect_garbage(
                &cache_root,
                GcOptions {
                    max_bytes: u64::MAX,
                    min_free_bytes: 0,
                    target_free_bytes: 0,
                    dry_run: false,
                },
            )
            .unwrap()
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let admission_closed = loop {
            match FileExt::try_lock_shared(&admission) {
                Ok(()) => FileExt::unlock(&admission).unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break true,
                Err(error) => panic!("checking collector admission: {error}"),
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(1));
        };
        if !admission_closed {
            drop(active);
            collector.join().unwrap();
            panic!("waiting collector never closed admission to new cache readers");
        }

        let cache_root = root.path().to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            let _guard = acquire_shared(&cache_root).unwrap();
            sender
                .send(cache_root.join("blobs/unreferenced").exists())
                .unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(root.path().join("blobs/unreferenced").exists());

        drop(active);
        assert!(!receiver.recv_timeout(Duration::from_secs(2)).unwrap());
        assert_eq!(collector.join().unwrap().removed_blob_entries, 1);
        reader.join().unwrap();
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
        record_access(root.path(), "older").unwrap();
        thread::sleep(Duration::from_millis(10));
        record_access(root.path(), "newer").unwrap();
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

    const MIB: usize = 1024 * 1024;

    /// Distinct, non-zero content, so no file system can store it sparsely.
    fn patterned(bytes: usize, seed: &str) -> Vec<u8> {
        let seed = seed.bytes().fold(0_u8, u8::wrapping_add);
        (0..bytes)
            .map(|index| {
                let low = u8::try_from(index % 251).expect("index modulo 251 fits in a byte");
                low.wrapping_add(seed) | 1
            })
            .collect()
    }

    fn write_action(root: &Path, key: &str, blobs: &[&str], blob_bytes: usize) {
        fs::create_dir_all(root.join("actions")).unwrap();
        fs::create_dir_all(root.join("blobs")).unwrap();
        for blob in blobs {
            let path = root.join("blobs").join(blob);
            if !path.exists() {
                fs::write(path, patterned(blob_bytes, blob)).unwrap();
            }
        }
        let outputs = blobs
            .iter()
            .map(|blob| format!(r#"{{"sha256":"{blob}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            root.join("actions").join(format!("{key}.json")),
            format!(r#"{{"schema_version":1,"action_key":"{key}","outputs":[{outputs}]}}"#),
        )
        .unwrap();
    }

    /// Access markers have millisecond timestamps; keep the LRU order explicit.
    fn touch(root: &Path, key: &str) {
        record_access(root, key).unwrap();
        thread::sleep(Duration::from_millis(20));
    }

    fn collect_within(root: &Path, max_bytes: u64) -> GcReport {
        collect_garbage(
            root,
            GcOptions {
                max_bytes,
                min_free_bytes: 0,
                target_free_bytes: 0,
                dry_run: false,
            },
        )
        .unwrap()
    }

    fn mib(bytes: usize) -> u64 {
        u64::try_from(bytes).unwrap()
    }

    #[test]
    fn evicting_an_action_credits_the_blobs_only_it_referenced() {
        let root = tempdir().unwrap();
        for key in ["a0", "a1", "a2", "a3"] {
            write_action(root.path(), key, &[&format!("{key}-blob")], 4 * MIB);
            touch(root.path(), key);
        }
        let before = cache_stats(root.path()).unwrap();

        // Freeing two MiB needs exactly the oldest action and its own blob.
        let report = collect_within(root.path(), before.total_bytes - mib(2 * MIB));

        assert_eq!(report.removed_action_entries, 1);
        assert!(!root.path().join("actions/a0.json").exists());
        assert!(!root.path().join("blobs/a0-blob").exists());
        for key in ["a1", "a2", "a3"] {
            assert!(root.path().join(format!("actions/{key}.json")).exists());
            assert!(root.path().join(format!("blobs/{key}-blob")).exists());
        }
        assert!(report.capacity_satisfied);
    }

    #[test]
    fn a_blob_still_referenced_by_a_surviving_action_is_not_credited() {
        let root = tempdir().unwrap();
        write_action(root.path(), "a0", &["shared"], 4 * MIB);
        touch(root.path(), "a0");
        write_action(root.path(), "a1", &["shared"], 4 * MIB);
        touch(root.path(), "a1");
        write_action(root.path(), "a2", &["a2-blob"], 4 * MIB);
        touch(root.path(), "a2");
        let before = cache_stats(root.path()).unwrap();

        // Evicting a0 frees nothing while a1 still needs the shared blob.
        let report = collect_within(root.path(), before.total_bytes - mib(2 * MIB));

        assert_eq!(report.removed_action_entries, 2);
        assert!(!root.path().join("blobs/shared").exists());
        assert!(root.path().join("actions/a2.json").exists());
        assert!(root.path().join("blobs/a2-blob").exists());
        assert!(report.capacity_satisfied);
    }
}
