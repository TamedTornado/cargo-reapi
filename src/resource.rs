use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fs2::FileExt;

/// How long a compile waits for a lease while no other lease is released
/// before it reports an infrastructure stall rather than waiting forever.
const STALL_SECONDS: u64 = 300;

/// The memory a cache-miss compile reserves from the ledger.
const COMPILE_MEMORY_GIB: usize = 2;

const CPU_CAPACITY_ENV: &str = "CARGO_REAPI_RESOURCE_CPU_CAPACITY";
const MEMORY_CAPACITY_ENV: &str = "CARGO_REAPI_RESOURCE_MEMORY_GIB_CAPACITY";

/// One explicitly selected host ledger can coordinate separate artifact caches.
/// Resolve it once by the same rule at every driver and sandbox boundary.
pub fn ledger_root(cache_root: &Path) -> Result<PathBuf> {
    let root = std::env::var_os("CARGO_REAPI_RESOURCE_LEDGER")
        .map_or_else(|| cache_root.join("resource-ledger-v1"), PathBuf::from);
    fs::create_dir_all(&root)
        .with_context(|| format!("creating resource ledger {}", root.display()))?;
    root.canonicalize()
        .with_context(|| format!("resolving resource ledger {}", root.display()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ResourceCapacity {
    pub cpu: usize,
    pub memory_gib: usize,
}

impl ResourceCapacity {
    pub fn from_env() -> Result<Self> {
        let host_cpu = host_logical_cpus()
            .or_else(|| std::thread::available_parallelism().ok().map(usize::from));
        Self::resolve(
            host_cpu,
            host_memory_gib(),
            positive_env(CPU_CAPACITY_ENV)?,
            positive_env(MEMORY_CAPACITY_ENV)?,
        )
    }

    fn resolve(
        host_cpu: Option<usize>,
        host_memory: Option<usize>,
        configured_cpu: Option<usize>,
        configured_memory: Option<usize>,
    ) -> Result<Self> {
        let cpu = configured_cpu
            .or(host_cpu)
            .with_context(|| format!("cannot detect host CPUs; set {CPU_CAPACITY_ENV}"))?;
        let memory_gib = configured_memory
            .or(host_memory)
            .with_context(|| format!("cannot detect host memory; set {MEMORY_CAPACITY_ENV}"))?;
        if let Some(host_cpu) = host_cpu
            && cpu > host_cpu
        {
            bail!("{CPU_CAPACITY_ENV}={cpu} exceeds the host's {host_cpu} logical CPUs");
        }
        if memory_gib < 7 {
            bail!("{MEMORY_CAPACITY_ENV} must be at least 7 GiB for a native link lease");
        }
        if let Some(host_memory_gib) = host_memory
            && memory_gib > host_memory_gib
        {
            bail!(
                "{MEMORY_CAPACITY_ENV}={memory_gib} exceeds the host's {host_memory_gib} GiB physical memory"
            );
        }
        Ok(Self { cpu, memory_gib })
    }

    pub fn apply_to(self, command: &mut std::process::Command) {
        // Resolve outside the strict sandbox, whose proc view may hide host
        // discovery. Every wrapper must use the same selected ledger capacity.
        command.env(CPU_CAPACITY_ENV, self.cpu.to_string());
        command.env(MEMORY_CAPACITY_ENV, self.memory_gib.to_string());
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
    progress: PathBuf,
}

/// Each lease release rewrites this marker, so waiters can tell a queue
/// behind live work from a ledger where nothing completes.
const PROGRESS_MARKER: &str = "progress";

fn record_progress(marker: &Path) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    // Best effort: a lost write can only shorten a waiter's patience.
    let _ = crate::safe_io::write_replacing(
        marker,
        format!("{}:{nanos}", std::process::id()).as_bytes(),
    );
}

/// Retries `attempt` until it yields a value. Waiting behind other leases is
/// not a stall: the deadline restarts whenever another lease in the ledger is
/// released. Returns `None` only after `stall` passes with no release at all.
fn wait_for_progress<T>(
    marker: &Path,
    stall: Duration,
    mut attempt: impl FnMut() -> Result<Option<T>>,
) -> Result<Option<T>> {
    let mut observed = fs::read(marker).ok();
    let mut deadline = Instant::now() + stall;
    loop {
        if let Some(value) = attempt()? {
            return Ok(Some(value));
        }
        let current = fs::read(marker).ok();
        if current != observed {
            observed = current;
            deadline = Instant::now() + stall;
        } else if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

impl ResourceLease {
    pub fn acquire() -> Result<Self> {
        let lease_root = std::env::var_os("CARGO_REAPI_RESOURCE_LEDGER").map_or_else(
            || std::env::temp_dir().join("cargo-reapi-resource-ledger-v1"),
            std::path::PathBuf::from,
        );
        Self::acquire_at(&lease_root, ResourceCapacity::from_env()?)
    }

    fn acquire_at(lease_root: &Path, capacity: ResourceCapacity) -> Result<Self> {
        let cpu_demand = 1;
        let memory_demand = COMPILE_MEMORY_GIB;
        fs::create_dir_all(lease_root.join("cpu"))?;
        fs::create_dir_all(lease_root.join("memory-gib"))?;
        let progress = lease_root.join(PROGRESS_MARKER);

        let lease = wait_for_progress(&progress, Duration::from_secs(STALL_SECONDS), || {
            let mut files = Vec::with_capacity(cpu_demand + memory_demand);
            let acquired = try_acquire_tokens(
                &lease_root.join("cpu"),
                capacity.cpu,
                cpu_demand,
                &mut files,
            )? && try_acquire_tokens(
                &lease_root.join("memory-gib"),
                capacity.memory_gib,
                memory_demand,
                &mut files,
            )?;
            Ok(acquired.then(|| Self {
                files,
                progress: progress.clone(),
            }))
        })?;
        lease.with_context(|| {
            format!(
                "infrastructure stall: no {cpu_demand}-CPU/{memory_demand}-GiB physical-action lease was released for {STALL_SECONDS} seconds"
            )
        })
    }
}

impl Drop for ResourceLease {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = FileExt::unlock(file);
        }
        // Announce only after the tokens are free.
        record_progress(&self.progress);
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
        let file = crate::safe_io::open_lock(&path)
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

    use tempfile::tempdir;

    use super::{
        COMPILE_MEMORY_GIB, ResourceCapacity, ResourceLease, record_progress, wait_for_progress,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn a_queue_behind_releasing_leases_waits_past_the_stall_window() {
        let root = tempdir().expect("ledger root");
        let marker = root.path().join("progress");
        let stall = Duration::from_millis(150);
        let releases = {
            let marker = marker.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    std::thread::sleep(Duration::from_millis(40));
                    record_progress(&marker);
                }
            })
        };
        let started = Instant::now();
        let ready_after = Duration::from_millis(600);

        let outcome = wait_for_progress(&marker, stall, || {
            Ok((started.elapsed() >= ready_after).then_some(()))
        })
        .expect("wait");

        releases.join().expect("release thread");
        assert_eq!(outcome, Some(()), "a live queue is not a stall");
        assert!(started.elapsed() >= ready_after);
    }

    #[test]
    fn a_ledger_with_no_releases_still_stalls() {
        let root = tempdir().expect("ledger root");
        let marker = root.path().join("progress");
        let stall = Duration::from_millis(150);
        let started = Instant::now();

        let outcome = wait_for_progress::<()>(&marker, stall, || Ok(None)).expect("wait");

        assert_eq!(outcome, None);
        assert!(started.elapsed() >= stall);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn releasing_a_lease_records_ledger_progress() {
        let root = tempdir().expect("ledger root");
        let marker = root.path().join("progress");
        let before = std::fs::read(&marker).ok();

        let capacity = ResourceCapacity {
            cpu: 1,
            memory_gib: COMPILE_MEMORY_GIB,
        };

        drop(ResourceLease::acquire_at(root.path(), capacity).expect("lease"));

        assert_ne!(std::fs::read(&marker).ok(), before);
    }

    #[test]
    fn defaults_use_host_capacity_instead_of_acceptance_benchmark_limits() {
        assert_eq!(
            ResourceCapacity::resolve(Some(20), Some(125), None, None).unwrap(),
            ResourceCapacity {
                cpu: 20,
                memory_gib: 125
            },
        );
    }

    #[test]
    fn configured_capacity_is_respected_and_validated_against_the_host() {
        assert_eq!(
            ResourceCapacity::resolve(Some(20), Some(125), Some(3), Some(11)).unwrap(),
            ResourceCapacity {
                cpu: 3,
                memory_gib: 11
            },
        );
        assert!(ResourceCapacity::resolve(Some(20), Some(125), Some(21), None).is_err());
        assert!(ResourceCapacity::resolve(Some(20), Some(125), None, Some(126)).is_err());
        assert!(ResourceCapacity::resolve(Some(20), Some(125), None, Some(6)).is_err());
        assert!(ResourceCapacity::resolve(None, None, None, None).is_err());
        assert_eq!(
            ResourceCapacity::resolve(None, None, Some(3), Some(11)).unwrap(),
            ResourceCapacity {
                cpu: 3,
                memory_gib: 11
            },
        );
    }

    /// The most leases `workers` threads held at once from one ledger.
    fn peak_concurrent_leases(capacity: ResourceCapacity, workers: usize) -> usize {
        let root = Arc::new(tempdir().expect("ledger root"));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let handles = (0..workers)
            .map(|_| {
                let root = Arc::clone(&root);
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                std::thread::spawn(move || {
                    let _lease =
                        ResourceLease::acquire_at(root.path(), capacity).expect("resource lease");
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(4));
                    active.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().expect("worker");
        }

        peak.load(Ordering::SeqCst)
    }

    #[test]
    fn distinct_physical_actions_overlap_without_exceeding_the_cpu_capacity() {
        let capacity = ResourceCapacity {
            cpu: 8,
            memory_gib: 8 * COMPILE_MEMORY_GIB,
        };

        let peak = peak_concurrent_leases(capacity, 128);

        assert!(peak > 1, "ledger serialized all work");
        assert!(peak <= 8, "ledger exceeded CPU capacity");
    }

    #[test]
    fn physical_actions_overlap_without_exceeding_the_memory_capacity() {
        let capacity = ResourceCapacity {
            cpu: 8,
            memory_gib: 2 * COMPILE_MEMORY_GIB,
        };

        let peak = peak_concurrent_leases(capacity, 20);

        assert!(peak > 1, "ledger serialized all work");
        assert!(peak <= 2, "ledger exceeded memory capacity");
    }
}
