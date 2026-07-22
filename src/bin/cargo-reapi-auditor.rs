use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest, Sha256};

const MAXIMUM_RSS_BYTES: u64 = 15 * 1024 * 1024 * 1024;
const MAXIMUM_SWAP_GROWTH_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Parser)]
struct Cli {
    #[command(subcommand)]
    command: AuditorCommand,
}

#[derive(Debug, Subcommand)]
enum AuditorCommand {
    /// Verify a dedicated, tool-selected eslogger JSON stream.
    Eslog {
        #[arg(long)]
        events: PathBuf,
        #[arg(long, required = true)]
        select: Vec<PathBuf>,
        #[arg(long, value_enum)]
        expected: EventExpectation,
        #[arg(long)]
        report: PathBuf,
    },
    /// Run a command while sampling its complete process tree and host swap.
    Run {
        #[arg(long)]
        report: PathBuf,
        /// Cross-process token directory whose open leases are sampled externally.
        #[arg(long)]
        ledger_root: PathBuf,
        #[arg(long, default_value_t = 300)]
        stall_seconds: u64,
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EventExpectation {
    Zero,
    Nonzero,
}

#[derive(Debug, Serialize)]
struct EslogReport {
    schema_version: u32,
    evidence: PathBuf,
    selected_executables: Vec<PathBuf>,
    total_event_count: usize,
    parsed_event_count: usize,
    invalid_line_count: usize,
    expected: String,
    violations: Vec<String>,
    passed: bool,
}

#[derive(Clone, Debug)]
struct ProcessSample {
    pid: u32,
    ppid: u32,
    cpu_percent: f32,
    rss_bytes: u64,
    cpu_time: String,
    command: String,
}

#[derive(Debug, Serialize)]
struct RetainedProcessSample {
    pid: u32,
    ppid: u32,
    cpu_percent: f32,
    rss_bytes: u64,
    cpu_time: String,
    command: String,
    command_sha256: String,
}

#[derive(Debug, Serialize)]
struct ResourceSample {
    at_unix_ms: u128,
    aggregate_rss_bytes: u64,
    swap_bytes: u64,
    processes: Vec<RetainedProcessSample>,
    lease_ownership: BTreeMap<u32, Vec<PathBuf>>,
}

#[derive(Debug, Serialize)]
struct ResourceReport {
    schema_version: u32,
    root_pid: u32,
    command: Vec<String>,
    started_at_unix_ms: u128,
    completed_at_unix_ms: u128,
    exit_code: i32,
    peak_aggregate_rss_bytes: u64,
    peak_simultaneous_progress_processes: usize,
    maximum_rss_bytes: u64,
    swap_start_bytes: u64,
    swap_end_bytes: u64,
    swap_growth_bytes: u64,
    maximum_swap_growth_bytes: u64,
    stall_seconds: u64,
    infrastructure_stall: bool,
    ledger_root: PathBuf,
    observed_lease_owners: usize,
    observed_action_identities: BTreeSet<String>,
    samples: Vec<ResourceSample>,
    violations: Vec<String>,
    passed: bool,
}

struct MonitorOutcome {
    exit_code: i32,
    peak_aggregate_rss_bytes: u64,
    peak_simultaneous_progress_processes: usize,
    infrastructure_stall: bool,
    observed_lease_pids: BTreeSet<u32>,
    observed_action_identities: BTreeSet<String>,
    samples: Vec<ResourceSample>,
}

struct MonitorReportContext<'a> {
    report: &'a Path,
    command: &'a [String],
    ledger_root: &'a Path,
    stall_seconds: u64,
    started_at_unix_ms: u128,
    swap_start_bytes: u64,
    root_pid: u32,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cargo-reapi-auditor: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        AuditorCommand::Eslog {
            events,
            select,
            expected,
            report,
        } => verify_eslog(&events, &select, expected, &report),
        AuditorCommand::Run {
            report,
            ledger_root,
            stall_seconds,
            command,
        } => monitor_command(&command, stall_seconds, &ledger_root, &report),
    }
}

fn verify_eslog(
    events: &Path,
    selected_executables: &[PathBuf],
    expected: EventExpectation,
    report: &Path,
) -> Result<()> {
    let input = fs::read_to_string(events)
        .with_context(|| format!("reading eslogger evidence {}", events.display()))?;
    let mut parsed_event_count = 0;
    let mut total_event_count = 0;
    let mut invalid_line_count = 0;
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(event) => {
                total_event_count += 1;
                let Some(executable) = event
                    .pointer("/event/exec/target/executable/path")
                    .and_then(serde_json::Value::as_str)
                else {
                    invalid_line_count += 1;
                    continue;
                };
                if selected_executables
                    .iter()
                    .any(|selected| selected == Path::new(executable))
                {
                    parsed_event_count += 1;
                }
            }
            Err(_) => invalid_line_count += 1,
        }
    }
    let mut violations = Vec::new();
    if invalid_line_count != 0 {
        violations.push(format!(
            "eslogger evidence contains {invalid_line_count} non-JSON lines"
        ));
    }
    match expected {
        EventExpectation::Zero if parsed_event_count != 0 => violations.push(format!(
            "OS observer recorded {parsed_event_count} selected compiler/linker events"
        )),
        EventExpectation::Nonzero if parsed_event_count == 0 => {
            violations.push("OS observer did not record the required producer event".to_owned());
        }
        _ => {}
    }
    let proof = EslogReport {
        schema_version: 1,
        evidence: events.to_path_buf(),
        selected_executables: selected_executables.to_vec(),
        total_event_count,
        parsed_event_count,
        invalid_line_count,
        expected: match expected {
            EventExpectation::Zero => "zero",
            EventExpectation::Nonzero => "nonzero",
        }
        .to_owned(),
        passed: violations.is_empty(),
        violations,
    };
    write_report(report, &proof)?;
    if !proof.passed {
        bail!("eslogger evidence failed; report: {}", report.display());
    }
    Ok(())
}

fn monitor_command(
    command: &[String],
    stall_seconds: u64,
    ledger_root: &Path,
    report: &Path,
) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let (program, arguments) = command
        .split_first()
        .context("monitored command is empty")?;
    let started_at_unix_ms = unix_ms();
    let swap_start_bytes = swap_bytes()?;
    let mut child = Command::new(program)
        .args(arguments)
        .process_group(0)
        .spawn()
        .with_context(|| format!("starting monitored command {program}"))?;
    let root_pid = child.id();
    let outcome = monitor_child(&mut child, root_pid, stall_seconds, ledger_root)?;
    let context = MonitorReportContext {
        report,
        command,
        ledger_root,
        stall_seconds,
        started_at_unix_ms,
        swap_start_bytes,
        root_pid,
    };
    write_monitor_report(&context, outcome)
}

fn monitor_child(
    child: &mut Child,
    root_pid: u32,
    stall_seconds: u64,
    ledger_root: &Path,
) -> Result<MonitorOutcome> {
    let mut samples = Vec::new();
    let mut peak_aggregate_rss_bytes = 0;
    let mut peak_simultaneous_progress_processes = 0;
    let mut last_progress = Instant::now();
    let mut previous_heavy_cpu = BTreeMap::new();
    let mut infrastructure_stall = false;
    let mut observed_lease_pids = BTreeSet::new();
    let mut observed_action_identities = BTreeSet::new();
    let exit_code;

    loop {
        if let Some(status) = child.try_wait().context("polling monitored command")? {
            exit_code = status.code().unwrap_or(1);
            break;
        }
        let all = process_table()?;
        let descendants = descendant_processes(root_pid, &all);
        let descendant_pids = descendants
            .iter()
            .map(|process| process.pid)
            .collect::<BTreeSet<_>>();
        let lease_ownership = externally_observed_leases(ledger_root, &descendant_pids)?;
        observed_lease_pids.extend(lease_ownership.keys().copied());
        observed_action_identities.extend(
            descendants
                .iter()
                .filter(|process| is_progress_process(&process.command))
                .map(|process| process.command.clone()),
        );
        let aggregate_rss_bytes = descendants.iter().map(|process| process.rss_bytes).sum();
        peak_aggregate_rss_bytes = peak_aggregate_rss_bytes.max(aggregate_rss_bytes);
        let heavy_cpu = descendants
            .iter()
            .filter(|process| is_progress_process(&process.command))
            .map(|process| (process.pid, process.cpu_time.clone()))
            .collect::<BTreeMap<_, _>>();
        peak_simultaneous_progress_processes =
            peak_simultaneous_progress_processes.max(heavy_cpu.len());
        if heavy_cpu != previous_heavy_cpu {
            last_progress = Instant::now();
            previous_heavy_cpu = heavy_cpu;
        } else if last_progress.elapsed() >= Duration::from_secs(stall_seconds) {
            infrastructure_stall = true;
            let status = terminate_process_group(child, root_pid)?;
            exit_code = status.code().unwrap_or(1);
            break;
        }
        samples.push(ResourceSample {
            at_unix_ms: unix_ms(),
            aggregate_rss_bytes,
            swap_bytes: swap_bytes()?,
            processes: descendants.iter().map(retain_process_sample).collect(),
            lease_ownership,
        });
        thread::sleep(Duration::from_millis(250));
    }

    Ok(MonitorOutcome {
        exit_code,
        peak_aggregate_rss_bytes,
        peak_simultaneous_progress_processes,
        infrastructure_stall,
        observed_lease_pids,
        observed_action_identities,
        samples,
    })
}

fn retain_process_sample(process: &ProcessSample) -> RetainedProcessSample {
    const MAX_COMMAND_CHARS: usize = 1_024;
    RetainedProcessSample {
        pid: process.pid,
        ppid: process.ppid,
        cpu_percent: process.cpu_percent,
        rss_bytes: process.rss_bytes,
        cpu_time: process.cpu_time.clone(),
        command: process.command.chars().take(MAX_COMMAND_CHARS).collect(),
        command_sha256: format!("{:x}", Sha256::digest(process.command.as_bytes())),
    }
}

fn write_monitor_report(context: &MonitorReportContext<'_>, outcome: MonitorOutcome) -> Result<()> {
    let swap_end_bytes = swap_bytes()?;
    let swap_growth_bytes = swap_end_bytes.saturating_sub(context.swap_start_bytes);
    let mut violations = Vec::new();
    if outcome.peak_aggregate_rss_bytes > MAXIMUM_RSS_BYTES {
        violations.push(format!(
            "peak process-tree RSS {} exceeds {MAXIMUM_RSS_BYTES}",
            outcome.peak_aggregate_rss_bytes
        ));
    }
    if swap_growth_bytes > MAXIMUM_SWAP_GROWTH_BYTES {
        violations.push(format!(
            "swap growth {swap_growth_bytes} exceeds {MAXIMUM_SWAP_GROWTH_BYTES}"
        ));
    }
    if outcome.infrastructure_stall {
        violations
            .push("compiler/linker progress stalled; classified as infrastructure".to_owned());
    }
    if outcome.exit_code != 0 {
        violations.push(format!("monitored command exited {}", outcome.exit_code));
    }
    let proof = ResourceReport {
        schema_version: 1,
        root_pid: context.root_pid,
        command: context.command.to_vec(),
        started_at_unix_ms: context.started_at_unix_ms,
        completed_at_unix_ms: unix_ms(),
        exit_code: outcome.exit_code,
        peak_aggregate_rss_bytes: outcome.peak_aggregate_rss_bytes,
        peak_simultaneous_progress_processes: outcome.peak_simultaneous_progress_processes,
        maximum_rss_bytes: MAXIMUM_RSS_BYTES,
        swap_start_bytes: context.swap_start_bytes,
        swap_end_bytes,
        swap_growth_bytes,
        maximum_swap_growth_bytes: MAXIMUM_SWAP_GROWTH_BYTES,
        stall_seconds: context.stall_seconds,
        infrastructure_stall: outcome.infrastructure_stall,
        ledger_root: context.ledger_root.to_path_buf(),
        observed_lease_owners: outcome.observed_lease_pids.len(),
        observed_action_identities: outcome.observed_action_identities,
        samples: outcome.samples,
        passed: violations.is_empty(),
        violations,
    };
    write_report(context.report, &proof)?;
    if !proof.passed {
        bail!(
            "monitored command failed resource acceptance: {}; report: {}",
            proof.violations.join("; "),
            context.report.display()
        );
    }
    Ok(())
}

fn terminate_process_group(child: &mut Child, root_pid: u32) -> Result<ExitStatus> {
    let group = format!("-{root_pid}");
    let term = Command::new("/bin/kill")
        .args(["-TERM", "--", &group])
        .status()
        .context("sending SIGTERM to stalled process group")?;
    if !term.success() {
        child
            .kill()
            .context("terminating stalled command after process-group SIGTERM failed")?;
        return child.wait().context("waiting for stalled command shutdown");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child
            .try_wait()
            .context("polling stalled command shutdown")?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let kill = Command::new("/bin/kill")
        .args(["-KILL", "--", &group])
        .status()
        .context("sending SIGKILL to stalled process group")?;
    if !kill.success() {
        child
            .kill()
            .context("killing stalled command after process-group SIGKILL failed")?;
    }
    child.wait().context("waiting for killed stalled command")
}

fn externally_observed_leases(
    ledger_root: &Path,
    pids: &BTreeSet<u32>,
) -> Result<BTreeMap<u32, Vec<PathBuf>>> {
    let canonical_root = ledger_root
        .canonicalize()
        .unwrap_or_else(|_| ledger_root.to_path_buf());
    if cfg!(target_os = "linux") {
        let mut result = BTreeMap::new();
        for pid in pids {
            let directory = PathBuf::from(format!("/proc/{pid}/fd"));
            let mut leases = Vec::new();
            let Ok(entries) = fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                if let Ok(target) = fs::read_link(entry.path())
                    && target.starts_with(&canonical_root)
                {
                    leases.push(target);
                }
            }
            if !leases.is_empty() {
                result.insert(*pid, leases);
            }
        }
        return Ok(result);
    }
    let output = Command::new("/usr/sbin/lsof")
        .args(["-Fpn", "+D"])
        .arg(&canonical_root)
        .output()
        .context("observing ledger leases with lsof")?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!("lsof failed while observing resource ledger");
    }
    let mut result = BTreeMap::<u32, Vec<PathBuf>>::new();
    let mut current = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(pid) = line.strip_prefix('p').and_then(|v| v.parse::<u32>().ok()) {
            current = pids.contains(&pid).then_some(pid);
        } else if let (Some(pid), Some(path)) = (current, line.strip_prefix('n')) {
            result.entry(pid).or_default().push(PathBuf::from(path));
        }
    }
    Ok(result)
}

fn process_table() -> Result<Vec<ProcessSample>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,%cpu=,rss=,time=,command="])
        .output()
        .context("sampling process table")?;
    if !output.status.success() {
        bail!("ps failed while sampling the monitored process tree");
    }
    let text = String::from_utf8(output.stdout).context("ps output is not UTF-8")?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut rest = line.trim_start();
            let pid = take_field(&mut rest)?.parse().ok()?;
            let parent_pid = take_field(&mut rest)?.parse().ok()?;
            let cpu_percent = take_field(&mut rest)?.parse().ok()?;
            let rss_kib = take_field(&mut rest)?.parse::<u64>().ok()?;
            let cpu_time = take_field(&mut rest)?.to_owned();
            Some(ProcessSample {
                pid,
                ppid: parent_pid,
                cpu_percent,
                rss_bytes: rss_kib.saturating_mul(1024),
                cpu_time,
                command: rest.trim_start().to_owned(),
            })
        })
        .collect())
}

fn take_field<'a>(input: &mut &'a str) -> Option<&'a str> {
    let trimmed = input.trim_start();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let field = &trimmed[..end];
    *input = &trimmed[end..];
    (!field.is_empty()).then_some(field)
}

fn descendant_processes(root_pid: u32, all: &[ProcessSample]) -> Vec<ProcessSample> {
    let mut pids = BTreeSet::from([root_pid]);
    loop {
        let before = pids.len();
        for process in all {
            if pids.contains(&process.ppid) {
                pids.insert(process.pid);
            }
        }
        if pids.len() == before {
            break;
        }
    }
    all.iter()
        .filter(|process| pids.contains(&process.pid))
        .cloned()
        .collect()
}

fn is_progress_process(command: &str) -> bool {
    let executable = command.split_whitespace().next().unwrap_or(command);
    let name = Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(executable);
    matches!(
        name,
        "rustc" | "clang" | "clang++" | "cc" | "ld" | "codesign"
    )
}

fn swap_bytes() -> Result<u64> {
    if cfg!(target_os = "linux") {
        if let Ok(cgroup) = fs::read_to_string("/proc/self/cgroup")
            && let Some(relative) = cgroup.lines().find_map(|line| line.strip_prefix("0::"))
        {
            let path = Path::new("/sys/fs/cgroup")
                .join(relative.trim_start_matches('/'))
                .join("memory.swap.current");
            if let Ok(value) = fs::read_to_string(path) {
                return value.trim().parse().context("parsing cgroup v2 swap usage");
            }
        }
        let meminfo = fs::read_to_string("/proc/meminfo")?;
        let field = |name: &str| -> Result<u64> {
            let kib = meminfo
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|value| value.split_whitespace().next())
                .with_context(|| format!("{name} is absent from /proc/meminfo"))?
                .parse::<u64>()?;
            Ok(kib * 1024)
        };
        return Ok(field("SwapTotal:")?.saturating_sub(field("SwapFree:")?));
    }
    let output = Command::new("/usr/sbin/sysctl")
        .args(["-n", "vm.swapusage"])
        .output()
        .context("sampling host swap")?;
    if !output.status.success() {
        bail!("sysctl vm.swapusage failed");
    }
    let text = String::from_utf8(output.stdout).context("swap usage is not UTF-8")?;
    let fields = text.split_whitespace().collect::<Vec<_>>();
    let used_index = fields
        .iter()
        .position(|field| *field == "used")
        .context("vm.swapusage has no used value")?;
    let used_mib = fields
        .get(used_index + 2)
        .context("vm.swapusage used value is incomplete")?
        .trim_end_matches('M');
    parse_mib_bytes(used_mib)
}

fn parse_mib_bytes(value: &str) -> Result<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole_bytes = whole
        .parse::<u64>()
        .context("parsing whole used swap MiB")?
        .checked_mul(1024 * 1024)
        .context("used swap MiB exceeds u64")?;
    if fraction.is_empty() {
        return Ok(whole_bytes);
    }
    let numerator = fraction
        .parse::<u64>()
        .context("parsing fractional used swap MiB")?;
    let denominator = (0..fraction.len()).try_fold(1_u64, |scale, _| {
        scale.checked_mul(10).context("swap precision exceeds u64")
    })?;
    let fractional_bytes = numerator
        .checked_mul(1024 * 1024)
        .context("fractional used swap MiB exceeds u64")?
        / denominator;
    whole_bytes
        .checked_add(fractional_bytes)
        .context("used swap bytes exceeds u64")
}

fn write_report<T: Serialize>(path: &Path, report: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(report)?)
        .with_context(|| format!("writing audit report {}", path.display()))
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_samples_bound_commands_and_hash_the_full_value() {
        let command = format!(
            "/usr/bin/bwrap {}",
            "--ro-bind /source /target ".repeat(1_000)
        );
        let process = ProcessSample {
            pid: 7,
            ppid: 1,
            cpu_percent: 25.0,
            rss_bytes: 42,
            cpu_time: "00:01".to_owned(),
            command: command.clone(),
        };
        let retained = retain_process_sample(&process);
        assert_eq!(retained.command.chars().count(), 1_024);
        assert_eq!(
            retained.command_sha256,
            format!("{:x}", Sha256::digest(command.as_bytes()))
        );
    }

    #[test]
    fn eslog_counts_only_selected_exec_targets() {
        let temporary = tempfile::tempdir().unwrap();
        let events = temporary.path().join("events.jsonl");
        let report = temporary.path().join("report.json");
        fs::write(
            &events,
            concat!(
                "{\"event\":{\"exec\":{\"target\":{\"executable\":{\"path\":\"/bin/echo\"}}}}}\n",
                "{\"event\":{\"exec\":{\"target\":{\"executable\":{\"path\":\"/bin/sleep\"}}}}}\n"
            ),
        )
        .unwrap();

        verify_eslog(
            &events,
            &[PathBuf::from("/bin/echo")],
            EventExpectation::Nonzero,
            &report,
        )
        .unwrap();

        let report: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert_eq!(report["total_event_count"], 2);
        assert_eq!(report["parsed_event_count"], 1);
        assert_eq!(report["passed"], true);
    }

    #[test]
    fn eslog_rejects_unparseable_or_schema_incomplete_evidence() {
        let temporary = tempfile::tempdir().unwrap();
        let events = temporary.path().join("events.jsonl");
        let report = temporary.path().join("report.json");
        fs::write(&events, "not-json\n{\"event\":{\"exec\":{}}}\n").unwrap();

        assert!(
            verify_eslog(
                &events,
                &[PathBuf::from("/bin/echo")],
                EventExpectation::Zero,
                &report,
            )
            .is_err()
        );

        let report: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
        assert_eq!(report["invalid_line_count"], 2);
        assert_eq!(report["passed"], false);
    }
}
