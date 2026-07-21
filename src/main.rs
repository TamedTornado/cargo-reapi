mod acceptance;
mod action;
mod cache;
mod capture;
mod evidence;
mod gate;
mod hermetic;
mod invocation;
mod proof;
mod query;
mod reclient;
mod relocation;
mod resource;

use std::env;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use crate::cache::{CacheOptions, LinkerCaptureRecord, execute_cached};
use crate::capture::{CaptureOptions, capture_invocation};
use crate::gate::GateSnapshot;
use crate::hermetic::SnapshotPolicy;
use crate::invocation::RustcInvocation;
use crate::reclient::{ReclientOptions, execute_reapi, validate_platform_template};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Local,
    Capture,
    Cache,
    Reapi,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SnapshotPolicyArg {
    Strict,
    Off,
}

impl From<SnapshotPolicyArg> for SnapshotPolicy {
    fn from(value: SnapshotPolicyArg) -> Self {
        match value {
            SnapshotPolicyArg::Strict => Self::Strict,
            SnapshotPolicyArg::Off => Self::Off,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "cargo reapi",
    bin_name = "cargo reapi",
    trailing_var_arg = true
)]
struct Cli {
    /// Execution backend. Capture executes locally and records hermetic action inputs.
    #[arg(
        long,
        value_enum,
        env = "CARGO_REAPI_BACKEND",
        default_value_t = Backend::Capture
    )]
    backend: Backend,

    /// JSON Lines action log. Defaults to target/cargo-reapi/actions.jsonl.
    #[arg(long, env = "CARGO_REAPI_ACTION_LOG")]
    action_log: Option<PathBuf>,

    /// Shared filesystem CAS used by the cache backend.
    #[arg(long, env = "CARGO_REAPI_CACHE_DIR")]
    cache_dir: Option<PathBuf>,

    /// Hermetic policy for whole-gate snapshots. Strict is fail-closed.
    #[arg(long, value_enum, default_value_t = SnapshotPolicyArg::Strict)]
    snapshot_policy: SnapshotPolicyArg,

    /// Additional read-only input made visible to strict build/proc-macro sandboxes.
    #[arg(long = "declared-input")]
    declared_inputs: Vec<PathBuf>,

    /// Path to reclient's rewrapper binary.
    #[arg(long, env = "CARGO_REAPI_REWRAPPER")]
    rewrapper: Option<PathBuf>,

    /// Rewrapper/reproxy configuration file.
    #[arg(long, env = "CARGO_REAPI_REWRAPPER_CFG")]
    rewrapper_cfg: Option<PathBuf>,

    /// Action staging root used to construct explicit reclient input trees.
    #[arg(long, env = "CARGO_REAPI_RECLIENT_STAGING_DIR")]
    reclient_staging_dir: Option<PathBuf>,

    /// Reclient platform properties, including the platform-matched toolchain contract.
    #[arg(long, env = "CARGO_REAPI_RECLIENT_PLATFORM")]
    reclient_platform: Option<String>,

    /// Arguments passed verbatim to Cargo.
    #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
    cargo_args: Vec<OsString>,
}

#[derive(Debug, Parser)]
#[command(name = "cargo reapi", bin_name = "cargo reapi")]
struct ContractCli {
    #[command(subcommand)]
    command: ContractCommand,
}

#[derive(Debug, Parser)]
#[command(name = "cargo reapi", bin_name = "cargo reapi")]
struct ProveCli {
    #[command(subcommand)]
    command: ProveCommand,
}

#[derive(Debug, Subcommand)]
enum ProveCommand {
    /// Capture and enforce the locked host CPU, memory, OS, and architecture contract.
    Environment {
        #[arg(long, value_enum)]
        storage_profile: StorageProfileArg,
        #[arg(long)]
        platform_profile: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Validate a warm action log against the embedded zero-physical-action contract.
    ActionLog {
        #[arg(long)]
        action_log: PathBuf,
        #[arg(long)]
        rustc_trace: PathBuf,
        #[arg(long)]
        worktree: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Validate that N or 2N complete gates were truly simultaneous and physically warm.
    Population {
        #[arg(long, value_enum)]
        kind: PopulationProofKind,
        #[arg(long, value_enum)]
        storage_profile: StorageProfileArg,
        #[arg(long)]
        evidence: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Fail closed unless every receipt required by the embedded criteria is present and valid.
    Complete {
        #[arg(long)]
        receipts: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Verify complete macOS and Linux current-schema evidence graphs.
    Aggregate {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PopulationProofKind {
    Single,
    Five,
    Stress,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum StorageProfileArg {
    Ssd,
    Rotational,
}

impl From<StorageProfileArg> for proof::StorageProfile {
    fn from(value: StorageProfileArg) -> Self {
        match value {
            StorageProfileArg::Ssd => Self::Ssd,
            StorageProfileArg::Rotational => Self::Rotational,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ContractCommand {
    /// Print the immutable acceptance contract embedded in this binary.
    Show,
    /// Verify that a contract file exactly matches the embedded contract.
    Verify {
        #[arg(long, default_value = "acceptance/contract.toml")]
        path: PathBuf,
    },
    /// Print the normative and exact-document identities for acceptance criteria.
    CriteriaIdentity {
        #[arg(long, default_value = "acceptance/ACCEPTANCE_CRITERIA.md")]
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("cargo-reapi: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<i32> {
    let args: Vec<OsString> = env::args_os().collect();
    if env::var_os("CARGO_REAPI_RUSTC_QUERY_SHIM").is_some() {
        return query::run_shim(args);
    }
    if env::var_os("CARGO_REAPI_LINKER_CAPTURE").is_some() {
        return run_linker_capture(args);
    }
    let args = strip_cargo_subcommand_name(args);
    if args.get(1).is_some_and(|value| value == "contract") {
        return run_contract(args);
    }
    if args.get(1).is_some_and(|value| value == "prove") {
        return run_prove(args);
    }
    if RustcInvocation::looks_like_wrapper(&args) {
        return run_wrapper(args);
    }
    let cli = Cli::parse_from(args);
    validate_cli(&cli)?;

    let executable = env::current_exe().context("locating cargo-reapi executable")?;
    let workspace_root = env::current_dir().context("locating Cargo workspace root")?;
    let target_root = env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace_root.join("target"), PathBuf::from);
    let action_log = cli
        .action_log
        .unwrap_or_else(|| PathBuf::from("target/cargo-reapi/actions.jsonl"));
    let action_log_for_snapshot = action_log.clone();
    let cache_dir = cli.cache_dir.clone();
    let snapshot_policy: SnapshotPolicy = cli.snapshot_policy.into();
    let snapshots_enabled = snapshot_policy == SnapshotPolicy::Strict;
    let mut gate_snapshot = if matches!(cli.backend, Backend::Cache)
        && snapshots_enabled
        && env::var_os("CARGO_REAPI_ACTION_CACHE_TEST_MODE").is_none()
    {
        Some(GateSnapshot::prepare(
            cache_dir
                .as_deref()
                .context("cache directory is required")?,
            &workspace_root,
            &target_root,
            &action_log_for_snapshot,
            &cli.cargo_args,
            &cli.declared_inputs,
        )?)
    } else {
        None
    };
    if let Some(snapshot) = gate_snapshot.as_ref()
        && snapshot.is_restored()
    {
        snapshot.record_successful_hit(&action_log_for_snapshot)?;
        return Ok(0);
    }
    let hermetic = if matches!(cli.backend, Backend::Cache) && snapshots_enabled {
        crate::hermetic::cargo_command(
            snapshot_policy,
            &workspace_root,
            &target_root,
            cache_dir
                .as_deref()
                .context("cache directory is required")?,
            &action_log_for_snapshot,
            &cli.declared_inputs,
        )?
    } else {
        crate::hermetic::cargo_command(
            SnapshotPolicy::Off,
            &workspace_root,
            &target_root,
            cache_dir.as_deref().unwrap_or(&target_root),
            &action_log_for_snapshot,
            &[],
        )?
    };
    let mut cargo = hermetic.command;
    cargo
        .args(&cli.cargo_args)
        .env("RUSTC_WRAPPER", executable)
        .env(
            "CARGO_REAPI_BACKEND",
            match cli.backend {
                Backend::Local => "local",
                Backend::Capture => "capture",
                Backend::Cache => "cache",
                Backend::Reapi => "reapi",
            },
        )
        .env("CARGO_REAPI_ACTION_LOG", action_log)
        .env("CARGO_REAPI_WORKSPACE_ROOT", workspace_root)
        .env("CARGO_REAPI_TARGET_ROOT", target_root);
    if matches!(cli.backend, Backend::Cache) {
        let default_linker = cache::default_linker_for_sandbox()?;
        cargo.env("CARGO_REAPI_DEFAULT_LINKER", &default_linker);
        if env::var_os("CC").is_none() {
            // Native build scripts use cc-rs directly rather than rustc's
            // linker setting. Supply the already-canonicalized distro
            // default so /etc/alternatives does not have to be exposed.
            cargo.env("CC", &default_linker);
        }
    }
    if let Some(cache_dir) = cache_dir {
        cargo
            .env(
                "CARGO_REAPI_RESOURCE_LEDGER",
                cache_dir.join("resource-ledger-v1"),
            )
            .env("CARGO_REAPI_CACHE_DIR", cache_dir);
    }
    for (name, value) in [
        (
            "CARGO_REAPI_REWRAPPER",
            cli.rewrapper.map(PathBuf::into_os_string),
        ),
        (
            "CARGO_REAPI_REWRAPPER_CFG",
            cli.rewrapper_cfg.map(PathBuf::into_os_string),
        ),
        (
            "CARGO_REAPI_RECLIENT_STAGING_DIR",
            cli.reclient_staging_dir.map(PathBuf::into_os_string),
        ),
        (
            "CARGO_REAPI_RECLIENT_PLATFORM",
            cli.reclient_platform.map(OsString::from),
        ),
    ] {
        if let Some(value) = value {
            cargo.env(name, value);
        }
    }
    let status = cargo.status().context("starting Cargo")?;
    complete_gate_snapshot(
        status.success(),
        gate_snapshot.as_mut(),
        &action_log_for_snapshot,
    )?;
    Ok(status.code().unwrap_or(1))
}

fn validate_cli(cli: &Cli) -> Result<()> {
    if matches!(cli.backend, Backend::Cache) && cli.cache_dir.is_none() {
        bail!("--backend cache requires an explicit shared --cache-dir")
    }
    if matches!(cli.backend, Backend::Reapi)
        && (cli.rewrapper.is_none()
            || cli.rewrapper_cfg.is_none()
            || cli.reclient_staging_dir.is_none()
            || cli.reclient_platform.is_none())
    {
        bail!(
            "--backend reapi requires --rewrapper, --rewrapper-cfg, --reclient-staging-dir, and --reclient-platform"
        )
    }
    if let Some(platform) = &cli.reclient_platform {
        validate_platform_template(platform)?;
    }
    Ok(())
}

fn complete_gate_snapshot(
    succeeded: bool,
    snapshot: Option<&mut GateSnapshot>,
    action_log: &std::path::Path,
) -> Result<()> {
    if succeeded && let Some(snapshot) = snapshot {
        snapshot.record_successful_hit(action_log)?;
        snapshot.publish_after_success()?;
    }
    Ok(())
}

fn run_prove(mut args: Vec<OsString>) -> Result<i32> {
    args.remove(1);
    let cli = ProveCli::parse_from(args);
    match cli.command {
        ProveCommand::Environment {
            storage_profile,
            platform_profile,
            report,
        } => {
            let proof =
                proof::EnvironmentProof::capture(storage_profile.into(), &platform_profile)?;
            proof.write_and_require_pass(&report)?;
            println!("PASS  {}", report.display());
        }
        ProveCommand::ActionLog {
            action_log,
            rustc_trace,
            worktree,
            report,
        } => {
            let proof = proof::ActionLogProof::verify_with_trace(
                &action_log,
                Some(&rustc_trace),
                Some(&worktree),
            )?;
            proof.write_and_require_pass(&report)?;
            println!("PASS  {}", report.display());
        }
        ProveCommand::Population {
            kind,
            storage_profile,
            evidence,
            report,
        } => {
            let kind = match kind {
                PopulationProofKind::Single => proof::PopulationKind::Single,
                PopulationProofKind::Five => proof::PopulationKind::Five,
                PopulationProofKind::Stress => proof::PopulationKind::Stress,
            };
            let proof = proof::PopulationProof::verify(&evidence, kind, storage_profile.into())?;
            proof.write_and_require_pass(&report)?;
            println!("PASS  {}", report.display());
        }
        ProveCommand::Complete { receipts, report } => {
            let proof = proof::CompleteProof::verify(&receipts)?;
            proof.write_and_require_pass(&report)?;
            println!("PASS  {}", report.display());
        }
        ProveCommand::Aggregate { root, report } => {
            let proof = evidence::AggregateProofV2::verify(&root)?;
            proof.write(&report)?;
            if !proof.passed {
                bail!(
                    "multi-platform aggregate acceptance failed closed; report: {}",
                    report.display()
                );
            }
            println!("PASS  {}", report.display());
        }
    }
    Ok(0)
}

fn run_linker_capture(args: Vec<OsString>) -> Result<i32> {
    use fs2::FileExt;

    let capture_path = env::var_os("CARGO_REAPI_LINKER_CAPTURE")
        .map(PathBuf::from)
        .context("CARGO_REAPI_LINKER_CAPTURE is required in linker mode")?;
    let real_linker = env::var_os("CARGO_REAPI_REAL_LINKER")
        .map(PathBuf::from)
        .context("CARGO_REAPI_REAL_LINKER is required in linker mode")?;
    let arguments = args
        .iter()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut command = Command::new(&real_linker);
    command.args(args.into_iter().skip(1));
    #[cfg(target_os = "linux")]
    let trace_argument = linux_linker_trace_argument(&real_linker);
    #[cfg(target_os = "linux")]
    if let Some(argument) = trace_argument {
        command.arg(argument);
    }

    #[cfg(target_os = "linux")]
    let (exit_code, traced_inputs) = if trace_argument.is_some() {
        let output = command
            .output()
            .with_context(|| format!("executing real linker {}", real_linker.display()))?;
        std::io::stderr()
            .write_all(&output.stderr)
            .context("replaying linker stderr")?;
        if !output.status.success() {
            std::io::stdout()
                .write_all(&output.stdout)
                .context("replaying failed linker stdout")?;
        }
        (
            output.status.code().unwrap_or(1),
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>(),
        )
    } else {
        let status = command
            .status()
            .with_context(|| format!("executing real linker {}", real_linker.display()))?;
        (status.code().unwrap_or(1), Vec::new())
    };
    #[cfg(not(target_os = "linux"))]
    let (exit_code, traced_inputs) = {
        let status = command
            .status()
            .with_context(|| format!("executing real linker {}", real_linker.display()))?;
        (status.code().unwrap_or(1), Vec::new())
    };

    if let Some(parent) = capture_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating linker capture directory {}", parent.display()))?;
    }
    let mut capture = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture_path)
        .with_context(|| format!("opening linker capture {}", capture_path.display()))?;
    capture.lock_exclusive().context("locking linker capture")?;
    let record = LinkerCaptureRecord {
        schema_version: 1,
        arguments,
        traced_inputs,
    };
    writeln!(
        capture,
        "{}",
        serde_json::to_string(&record).context("serializing linker capture")?
    )
    .context("writing linker capture")?;
    FileExt::unlock(&capture).context("unlocking linker capture")?;
    Ok(exit_code)
}

#[cfg(target_os = "linux")]
fn linux_linker_trace_argument(linker: &std::path::Path) -> Option<&'static str> {
    let name = linker.file_name()?.to_str()?;
    if name.contains("gcc") || name.contains("g++") || name.contains("clang") || name == "cc" {
        Some("-Wl,-t")
    } else if name == "ld" || name.starts_with("ld.") || name.ends_with("-ld") {
        Some("-t")
    } else {
        None
    }
}

fn run_contract(mut args: Vec<OsString>) -> Result<i32> {
    args.remove(1);
    let cli = ContractCli::parse_from(args);
    match cli.command {
        ContractCommand::Show => {
            print!("{}", acceptance::EMBEDDED_CONTRACT);
        }
        ContractCommand::Verify { path } => {
            acceptance::AcceptanceContract::verify_file(&path)?;
            println!(
                "{}  {}",
                acceptance::AcceptanceContract::digest(),
                path.display()
            );
        }
        ContractCommand::CriteriaIdentity { path } => {
            let (criteria_sha256, criteria_document_sha256) =
                acceptance::criteria_file_identity(&path)?;
            if criteria_sha256 != acceptance::criteria_digest() {
                bail!(
                    "normative criteria do not match the criteria embedded in this binary: {}",
                    path.display()
                );
            }
            println!(
                "{}",
                serde_json::json!({
                    "criteria_sha256": criteria_sha256,
                    "criteria_document_sha256": criteria_document_sha256,
                })
            );
        }
    }
    Ok(0)
}

fn run_wrapper(args: Vec<OsString>) -> Result<i32> {
    let mut invocation = RustcInvocation::parse(args)?;
    invocation.add_stable_path_remapping()?;
    let backend = env::var("CARGO_REAPI_BACKEND").unwrap_or_else(|_| "capture".to_owned());
    match backend.as_str() {
        "local" => invocation.execute(),
        "capture" => capture_invocation(&invocation, &CaptureOptions::from_env()?),
        "cache" => execute_cached(
            &invocation,
            &CaptureOptions::from_env()?,
            &CacheOptions::new(
                env::var_os("CARGO_REAPI_CACHE_DIR")
                    .map(PathBuf::from)
                    .context("CARGO_REAPI_CACHE_DIR is required in cache mode")?,
            ),
        ),
        "reapi" => execute_reapi(
            &invocation,
            &CaptureOptions::from_env()?,
            &ReclientOptions::from_env()?,
        ),
        value => bail!("unknown CARGO_REAPI_BACKEND value: {value}"),
    }
}

fn strip_cargo_subcommand_name(mut args: Vec<OsString>) -> Vec<OsString> {
    if args.get(1).is_some_and(|value| value == "reapi") {
        args.remove(1);
    }
    args
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use std::path::Path;

    use super::linux_linker_trace_argument;

    #[test]
    fn selects_trace_syntax_for_driver_and_direct_linker() {
        assert_eq!(
            linux_linker_trace_argument(Path::new("/usr/bin/x86_64-linux-gnu-gcc-12")),
            Some("-Wl,-t")
        );
        assert_eq!(
            linux_linker_trace_argument(Path::new("/usr/bin/ld")),
            Some("-t")
        );
        assert_eq!(
            linux_linker_trace_argument(Path::new("/opt/custom/linker")),
            None
        );
    }
}
