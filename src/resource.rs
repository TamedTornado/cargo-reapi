use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

use crate::acceptance::AcceptanceContract;

pub struct ResourceLease {
    files: Vec<File>,
}

impl ResourceLease {
    pub fn acquire(native_link: bool) -> Result<Self> {
        let lease_root = std::env::var_os("CARGO_REAPI_RESOURCE_LEDGER").map_or_else(
            || std::env::temp_dir().join("cargo-reapi-resource-ledger-v1"),
            std::path::PathBuf::from,
        );
        Self::acquire_at(&lease_root, native_link)
    }

    fn acquire_at(lease_root: &Path, native_link: bool) -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        let cpu_capacity = std::thread::available_parallelism()
            .map_or(contract.minimum_logical_cpus, usize::from)
            .min(contract.minimum_logical_cpus);
        let memory_capacity = usize::try_from(contract.maximum_build_rss_gib)
            .context("resource memory capacity does not fit usize")?;
        let cpu_demand = 1;
        // Bevy/wgpu links are the observed high-water action. Two may overlap
        // inside the 15-GiB build budget; ordinary rustc actions may overlap up
        // to the CPU limit while still reserving a realistic 2 GiB each.
        let memory_demand = if native_link { 7 } else { 2 };
        fs::create_dir_all(lease_root.join("cpu"))?;
        fs::create_dir_all(lease_root.join("memory-gib"))?;
        let deadline = Instant::now() + Duration::from_secs(contract.stall_seconds);

        loop {
            let mut files = Vec::with_capacity(cpu_demand + memory_demand);
            if try_acquire_tokens(
                &lease_root.join("cpu"),
                cpu_capacity,
                cpu_demand,
                &mut files,
            )? && try_acquire_tokens(
                &lease_root.join("memory-gib"),
                memory_capacity,
                memory_demand,
                &mut files,
            )? {
                return Ok(Self { files });
            }
            drop(files);
            if Instant::now() >= deadline {
                bail!(
                    "infrastructure stall: no {cpu_demand}-CPU/{memory_demand}-GiB physical-action lease became available within {} seconds",
                    contract.stall_seconds
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn acquire_snapshot_signing_at(lease_root: &Path) -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        let root = lease_root.join("snapshot-signing");
        fs::create_dir_all(&root)?;
        let deadline = Instant::now() + Duration::from_secs(contract.stall_seconds);
        loop {
            let mut files = Vec::with_capacity(1);
            if try_acquire_tokens(&root, 4, 1, &mut files)? {
                return Ok(Self { files });
            }
            if Instant::now() >= deadline {
                bail!(
                    "infrastructure stall: no snapshot-signing lease became available within {} seconds",
                    contract.stall_seconds
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for ResourceLease {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = FileExt::unlock(file);
        }
    }
}

fn try_acquire_tokens(
    root: &Path,
    capacity: usize,
    demand: usize,
    acquired: &mut Vec<File>,
) -> Result<bool> {
    let initial = acquired.len();
    for index in 0..capacity {
        let path = root.join(format!("{index}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening resource token {}", path.display()))?;
        match file.try_lock_exclusive() {
            Ok(()) => acquired.push(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).with_context(|| format!("locking {}", path.display())),
        }
        if acquired.len() - initial == demand {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use std::path::Path;
    use tempfile::tempdir;

    use super::ResourceLease;

    #[test]
    fn distinct_physical_actions_overlap_without_exceeding_the_ledger() {
        let root = Arc::new(tempdir().expect("ledger root"));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..128 {
            let root = Arc::clone(&root);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            workers.push(std::thread::spawn(move || {
                let _lease = ResourceLease::acquire_at(root.path(), false).expect("resource lease");
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(2));
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for worker in workers {
            worker.join().expect("worker");
        }
        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "ledger serialized all work"
        );
        assert!(
            peak.load(Ordering::SeqCst) <= 8,
            "ledger exceeded CPU capacity"
        );
    }

    #[test]
    fn bevy_scale_links_overlap_without_exceeding_memory_tokens() {
        let root = Arc::new(tempdir().expect("ledger root"));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..20 {
            let root = Arc::clone(&root);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            workers.push(std::thread::spawn(move || {
                let _lease = ResourceLease::acquire_at(root.path(), true).expect("link lease");
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(4));
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for worker in workers {
            worker.join().expect("worker");
        }
        assert!(peak.load(Ordering::SeqCst) > 1, "ledger serialized links");
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "ledger exceeded the link memory budget"
        );
    }

    #[test]
    fn snapshot_signing_has_four_shared_tokens() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..20 {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            workers.push(std::thread::spawn(move || {
                let _lease = ResourceLease::acquire_snapshot_signing_at(Path::new(
                    "/tmp/cargo-reapi-resource-ledger-v1-test",
                ))
                .expect("signing lease");
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(current, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(4));
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for worker in workers {
            worker.join().expect("worker");
        }
        assert!(peak.load(Ordering::SeqCst) > 1);
        assert!(peak.load(Ordering::SeqCst) <= 4);
    }
}
