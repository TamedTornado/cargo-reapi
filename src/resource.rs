use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

use crate::acceptance::AcceptanceContract;

const CPU_CAPACITY_ENV: &str = "CARGO_REAPI_RESOURCE_CPU_CAPACITY";
const MEMORY_CAPACITY_ENV: &str = "CARGO_REAPI_RESOURCE_MEMORY_GIB_CAPACITY";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ResourceCapacity {
    pub cpu: usize,
    pub memory_gib: usize,
}

impl ResourceCapacity {
    pub fn from_env() -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        let host_cpu = host_logical_cpus().unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(contract.minimum_logical_cpus, usize::from)
        });
        let default_cpu = host_cpu.min(contract.minimum_logical_cpus);
        let default_memory = usize::try_from(contract.maximum_build_rss_gib)
            .context("resource memory capacity does not fit usize")?;
        let cpu = positive_env(CPU_CAPACITY_ENV)?.unwrap_or(default_cpu);
        let memory_gib = positive_env(MEMORY_CAPACITY_ENV)?.unwrap_or(default_memory);
        if cpu > host_cpu {
            bail!("{CPU_CAPACITY_ENV}={cpu} exceeds the host's {host_cpu} logical CPUs");
        }
        if memory_gib < 7 {
            bail!("{MEMORY_CAPACITY_ENV} must be at least 7 GiB for a native link lease");
        }
        if let Some(host_memory_gib) = host_memory_gib()
            && memory_gib > host_memory_gib
        {
            bail!(
                "{MEMORY_CAPACITY_ENV}={memory_gib} exceeds the host's {host_memory_gib} GiB physical memory"
            );
        }
        Ok(Self { cpu, memory_gib })
    }
}

#[cfg(target_os = "linux")]
fn host_logical_cpus() -> Option<usize> {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").ok()?;
    let count = cpuinfo
        .lines()
        .filter(|line| line.starts_with("processor"))
        .count();
    (count > 0).then_some(count)
}

#[cfg(target_os = "macos")]
fn host_logical_cpus() -> Option<usize> {
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.logicalcpu"])
        .output()
        .ok()?;
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn host_logical_cpus() -> Option<usize> {
    None
}

fn positive_env(name: &str) -> Result<Option<usize>> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(None);
    };
    let raw = raw
        .to_str()
        .with_context(|| format!("{name} is not valid UTF-8"))?;
    let value = raw
        .parse::<usize>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if value == 0 {
        bail!("{name} must be a positive integer");
    }
    Ok(Some(value))
}

#[cfg(target_os = "linux")]
fn host_memory_gib() -> Option<usize> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let kib = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    usize::try_from(kib / 1024 / 1024).ok()
}

#[cfg(target_os = "macos")]
fn host_memory_gib() -> Option<usize> {
    let output = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    let bytes = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    usize::try_from(bytes / 1024 / 1024 / 1024).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn host_memory_gib() -> Option<usize> {
    None
}

pub struct ResourceLease {
    files: Vec<File>,
}

impl ResourceLease {
    pub fn acquire(native_link: bool) -> Result<Self> {
        let lease_root = std::env::var_os("CARGO_REAPI_RESOURCE_LEDGER").map_or_else(
            || std::env::temp_dir().join("cargo-reapi-resource-ledger-v1"),
            std::path::PathBuf::from,
        );
        Self::acquire_at_with(&lease_root, native_link, ResourceCapacity::from_env()?)
    }

    #[cfg(test)]
    fn acquire_at(lease_root: &Path, native_link: bool) -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
        Self::acquire_at_with(
            lease_root,
            native_link,
            ResourceCapacity {
                cpu: contract.minimum_logical_cpus,
                memory_gib: usize::try_from(contract.maximum_build_rss_gib)
                    .context("resource memory capacity does not fit usize")?,
            },
        )
    }

    fn acquire_at_with(
        lease_root: &Path,
        native_link: bool,
        capacity: ResourceCapacity,
    ) -> Result<Self> {
        let contract = AcceptanceContract::embedded()?;
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
                capacity.cpu,
                cpu_demand,
                &mut files,
            )? && try_acquire_tokens(
                &lease_root.join("memory-gib"),
                capacity.memory_gib,
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
