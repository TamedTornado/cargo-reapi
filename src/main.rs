mod acceptance;
mod action;
mod cache;
mod capture;
mod cargo_config;
mod environment;
mod evidence;
mod footprint;
mod gate;
mod hermetic;
mod invocation;
mod maintenance;
mod project;
mod proof;
mod query;
mod reclient;
mod relocation;
mod resource;
mod selection;

use std::env;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use crate::cache::{CacheOptions, LinkerCaptureRecord, execute_cached};
use crate::capture::{CaptureOptions, capture_invocation};
use crate::environment::{
    CALLER_ENVIRONMENT, CONFIG_ENVIRONMENT, RUNTIME_PLUMBING_ENVIRONMENT, caller_record,
    config_record,
};
use crate::gate::GateSnapshot;
use crate::hermetic::SnapshotPolicy;
use crate::invocation::RustcInvocation;
use crate::maintenance::{GcOptions, cache_stats, collect_garbage};
use crate::project::Project;
use crate::reclient::{ReclientOptions, execute_reapi, validate_platform_template};
use crate::resource::ResourceCapacity;

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
    version,
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

    /// JSON Lines action log. Defaults to cargo-reapi/actions.jsonl in Cargo's target directory.
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

#[derive(Debug, Parser)]
#[command(name = "cargo reapi", bin_name = "cargo reapi")]
struct CacheCli {
    #[command(subcommand)]
    command: CacheCommand,
}

#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// Report durable cache size and entry counts.
    Stats {
        #[arg(long, env = "CARGO_REAPI_CACHE_DIR")]
        cache_dir: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Reclaim least-recently-used entries without racing active cache operations.
    Gc {
        #[arg(long, env = "CARGO_REAPI_CACHE_DIR")]
        cache_dir: PathBuf,
        #[arg(long)]
        max_bytes: u64,
        #[arg(long, default_value_t = 0)]
        min_free_bytes: u64,
        #[arg(long, default_value_t = 0)]
        target_free_bytes: u64,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Parser)]
#[command(name = "cargo reapi", bin_name = "cargo reapi")]
struct DoctorCli {
    #[arg(long, env = "CARGO_REAPI_CACHE_DIR")]
    cache_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(serde::Serialize)]
struct DoctorReport {
    schema_version: u32,
    passed: bool,
    platform_os: &'static str,
    platform_arch: &'static str,
    sandbox_provider_sha256: String,
    clone_method: &'static str,
    resource_capacity: ResourceCapacity,
    cache: maintenance::CacheStats,
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
        return query::run_shim(&args);
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
    if args.get(1).is_some_and(|value| value == "cache") {
        return run_cache(args);
    }
    if args.get(1).is_some_and(|value| value == "doctor") {
        return run_doctor(args);
    }
    if RustcInvocation::looks_like_wrapper(&args) {
        return run_wrapper(args);
    }
    let cli = Cli::parse_from(args);
    validate_cli(&cli)?;

    run_cli(&cli)
}

fn run_cache(mut args: Vec<OsString>) -> Result<i32> {
    args.remove(1);
    let cli = CacheCli::parse_from(args);
    let value = match cli.command {
        CacheCommand::Stats { cache_dir, json } => {
            let report = cache_stats(&cache_dir)?;
            if !json {
                eprintln!(
                    "cache={} bytes={} available={}",
                    report.cache_root.display(),
                    report.total_bytes,
                    report.available_bytes
                );
            }
            serde_json::to_value(report)?
        }
        CacheCommand::Gc {
            cache_dir,
            max_bytes,
            min_free_bytes,
            target_free_bytes,
            dry_run,
            json,
        } => {
            let report = collect_garbage(
                &cache_dir,
                GcOptions {
                    max_bytes,
                    min_free_bytes,
                    target_free_bytes,
                    dry_run,
                },
            )?;
            if !json {
                eprintln!(
                    "cache={} before={} after={} freed={} satisfied={}",
                    report.before.cache_root.display(),
                    report.before.total_bytes,
                    report.after.total_bytes,
                    report.estimated_freed_bytes,
                    report.capacity_satisfied
                );
            }
            serde_json::to_value(report)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(0)
}

fn run_doctor(mut args: Vec<OsString>) -> Result<i32> {
    args.remove(1);
    let cli = DoctorCli::parse_from(args);
    let report = DoctorReport {
        schema_version: 1,
        passed: true,
        platform_os: env::consts::OS,
        platform_arch: env::consts::ARCH,
        sandbox_provider_sha256: hermetic::provider_identity_digest()?,
        clone_method: gate::probe_clone_method(&cli.cache_dir)?,
        resource_capacity: ResourceCapacity::from_env()?,
        cache: cache_stats(&cli.cache_dir)?,
    };
    if !cli.json {
        eprintln!(
            "cargo-reapi doctor passed: sandbox={} clone={} cpu={} memory_gib={}",
            report.sandbox_provider_sha256,
            report.clone_method,
            report.resource_capacity.cpu,
            report.resource_capacity.memory_gib
        );
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(0)
}

/// Gives compiles what they need to work out their keyed environment.
fn describe_compile_environment(
    cargo: &mut Command,
    workspace_root: &Path,
    cargo_args: &[OsString],
) {
    // Agent and container runtimes synthesize per-invocation plumbing. Letting
    // it reach build scripts would make otherwise identical worktrees
    // semantically different; omitting it from a key without scrubbing it
    // would be unsound.
    for name in RUNTIME_PLUMBING_ENVIRONMENT {
        cargo.env_remove(name);
    }
    cargo
        // Compiles compare against the caller's environment to find what
        // Cargo itself defined for them.
        .env(CALLER_ENVIRONMENT, caller_record())
        // Cargo's [env] configuration may define a variable with the value
        // the caller already has, which that comparison cannot see.
        .env(
            CONFIG_ENVIRONMENT,
            config_record(workspace_root, cargo_args),
        );
}

fn run_cli(cli: &Cli) -> Result<i32> {
    let context = resolve_driver_context(cli)?;
    if let Some(reason) = &context.decline {
        return run_uncached(cli, &context, reason);
    }
    if context.project.is_some() {
        crate::hermetic::provider_identity_digest().context(SANDBOX_UNAVAILABLE)?;
    }

    // Only `run`'s build is cached, as the `build` it contains. The program
    // then runs through Cargo as ever, with its own stdin and stdout, and
    // is never skipped, replayed or sandboxed.
    let Some(command_index) = context.runs_program.filter(|_| context.project.is_some()) else {
        return run_with(cli, context, &cli.cargo_args);
    };
    let build = build_arguments(&cli.cargo_args, command_index);
    let code = run_with(cli, context.clone(), &build)?;
    if code != 0 {
        return Ok(code);
    }
    let program = DriverContext {
        project: None,
        decline: None,
        runs_program: None,
        ..context
    };
    run_with(cli, program, &cli.cargo_args)
}

/// `run`'s arguments as the `build` they contain: the subcommand replaced,
/// and the program's own arguments after `--` dropped.
fn build_arguments(cargo_args: &[OsString], command_index: usize) -> Vec<OsString> {
    let mut build = cargo_args.to_vec();
    build[command_index] = OsString::from("build");
    if let Some(separator) = build[command_index..]
        .iter()
        .position(|argument| argument == "--")
    {
        build.truncate(command_index + separator);
    }
    build
}

fn run_with(cli: &Cli, context: DriverContext, cargo_args: &[OsString]) -> Result<i32> {
    // Compiles run in their package's directory, so a relative log would
    // land beside each package rather than where the caller asked.
    let action_log = cli.action_log.as_ref().map_or_else(
        || context.target_root.join("cargo-reapi/actions.jsonl"),
        |path| context.workspace_root.join(path),
    );
    let action_log_for_snapshot = action_log.clone();
    let cache_dir = cli.cache_dir.clone();
    // The project is resolved exactly when the strict sandbox is used. A
    // gate that cannot be prepared hands the command to Cargo.
    let mut gate_snapshot = match prepare_gate(
        cli,
        &context.workspace_root,
        &context.target_root,
        &action_log_for_snapshot,
        context.project.as_ref(),
        cargo_args,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return run_uncached(cli, &context, &one_line(&format!("{error:#}")));
        }
    };
    let DriverContext {
        executable,
        workspace_root,
        target_root,
        project,
        ..
    } = context;
    if let Some(snapshot) = gate_snapshot.as_ref()
        && snapshot.is_restored()
    {
        snapshot.record_successful_hit(&action_log_for_snapshot)?;
        return Ok(0);
    }
    // A gate can wait a long time for another producer. If cargo-reapi was
    // reinstalled meanwhile, Cargo would run the new version as the compiler
    // wrapper under this driver; the new version takes the command instead.
    if cache::replaced_since_start(&executable) {
        drop(gate_snapshot);
        return restart_installed(&executable);
    }
    let hermetic = cached_cargo_command(
        cli,
        &workspace_root,
        &target_root,
        &action_log_for_snapshot,
        project.as_ref(),
    )?;
    let mut cargo = hermetic.command;
    if matches!(cli.backend, Backend::Cache | Backend::Reapi) {
        ResourceCapacity::from_env()?.apply_to(&mut cargo);
    }
    cargo.args(cargo_args);
    describe_compile_environment(&mut cargo, &workspace_root, cargo_args);
    cargo
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
                resource::ledger_root(&cache_dir)?,
            )
            .env("CARGO_REAPI_CACHE_DIR", cache_dir);
    }
    configure_reclient_environment(&mut cargo, cli);
    let status = cargo.status().context("starting Cargo")?;
    complete_gate_snapshot(
        status.success(),
        gate_snapshot.as_mut(),
        &action_log_for_snapshot,
    )?;
    Ok(status.code().unwrap_or(1))
}

/// The Cargo command for a cached run: inside the strict sandbox when the
/// project was resolved for it.
fn cached_cargo_command(
    cli: &Cli,
    workspace_root: &Path,
    target_root: &Path,
    action_log: &Path,
    project: Option<&Project>,
) -> Result<crate::hermetic::HermeticCargo> {
    if project.is_some() {
        crate::hermetic::cargo_command(
            SnapshotPolicy::Strict,
            workspace_root,
            target_root,
            cli.cache_dir
                .as_deref()
                .context("cache directory is required")?,
            action_log,
            &cli.declared_inputs,
            project,
        )
    } else {
        crate::hermetic::cargo_command(
            SnapshotPolicy::Off,
            workspace_root,
            target_root,
            cli.cache_dir.as_deref().unwrap_or(target_root),
            action_log,
            &[],
            None,
        )
    }
}

/// The whole-gate snapshot for a cache build in the strict sandbox.
fn prepare_gate(
    cli: &Cli,
    workspace_root: &Path,
    target_root: &Path,
    action_log: &Path,
    project: Option<&Project>,
    cargo_args: &[OsString],
) -> Result<Option<GateSnapshot>> {
    let Some(project) = project else {
        return Ok(None);
    };
    #[cfg(feature = "test-hooks")]
    if env::var_os("CARGO_REAPI_ACTION_CACHE_TEST_MODE").is_some() {
        return Ok(None);
    }

    let cache_dir = cli
        .cache_dir
        .as_deref()
        .context("cache directory is required")?;
    GateSnapshot::prepare(
        cache_dir,
        workspace_root,
        target_root,
        action_log,
        cargo_args,
        &cli.declared_inputs,
        project,
    )
    .map(Some)
}

/// Runs this command with the cargo-reapi now installed at `executable`.
#[cfg(unix)]
fn restart_installed(executable: &Path) -> Result<i32> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(executable).args(env::args_os().skip(1)).exec();
    Err(error).with_context(|| format!("restarting the reinstalled {}", executable.display()))
}

#[cfg(not(unix))]
fn restart_installed(executable: &Path) -> Result<i32> {
    bail!(
        "{} was replaced while this command ran",
        executable.display()
    )
}

/// Said when the strict sandbox cannot be built. Cargo did not run, and
/// nothing was declined: this is a failure of the environment.
const SANDBOX_UNAVAILABLE: &str =
    "the strict sandbox is unavailable, so Cargo was not run (this is not a decline)";

#[derive(Clone)]
struct DriverContext {
    executable: PathBuf,
    workspace_root: PathBuf,
    target_root: PathBuf,
    /// The selected project, resolved for the strict sandbox and its gate.
    project: Option<Project>,
    /// Why this invocation is not cached, when it is not: the invocation
    /// runs as plain Cargo.
    decline: Option<String>,
    /// The invocation is `run`: only its build is cached, as `build`, and
    /// the program then runs normally.
    runs_program: Option<usize>,
}

/// What the preflight of a cached invocation learned before it declined or
/// finished.
#[derive(Default)]
struct Preflight {
    project: Option<Project>,
    target_root: Option<PathBuf>,
    /// For `run`, where the subcommand sits among the Cargo arguments.
    runs_program: Option<usize>,
}

fn resolve_driver_context(cli: &Cli) -> Result<DriverContext> {
    let executable = env::current_exe()
        .context("locating cargo-reapi executable")?
        .canonicalize()
        .context("resolving cargo-reapi executable")?;
    let workspace_root =
        crate::project::canonical(&env::current_dir().context("locating Cargo workspace root")?)?;

    let snapshots: SnapshotPolicy = cli.snapshot_policy.into();
    if matches!(cli.backend, Backend::Cache) && snapshots == SnapshotPolicy::Strict {
        let mut preflight = Preflight::default();
        let decline = prepare_cached_project(cli, &workspace_root, &executable, &mut preflight)
            .err()
            .map(|error| one_line(&format!("{error:#}")));
        let target_root = match preflight.target_root {
            Some(target_root) => target_root,
            None => resolve_target_root_from(&workspace_root, None)?,
        };
        return Ok(DriverContext {
            executable,
            workspace_root,
            target_root,
            project: preflight.project,
            decline,
            runs_program: preflight.runs_program,
        });
    }

    // Local, capture and snapshot-free runs cache nothing at the gate, so
    // the restrictions that keep it sound do not apply. The selection only
    // locates the target directory, where Cargo's own answer for the
    // working directory serves when it cannot be read.
    let (metadata_arguments, target_dir) =
        crate::selection::select(&workspace_root, &cli.cargo_args)
            .map(|selection| (selection.metadata_arguments, selection.target_dir))
            .unwrap_or_default();
    let configured = target_dir
        .or_else(|| env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .or_else(|| crate::project::target_directory(&workspace_root, &metadata_arguments));
    let target_root = resolve_target_root_from(&workspace_root, configured)?;
    validate_driver_paths(&executable, &target_root)?;

    Ok(DriverContext {
        executable,
        workspace_root,
        target_root,
        project: None,
        decline: None,
        runs_program: None,
    })
}

/// Proves the invocation safe to cache, or says why not. Nothing here
/// changes the project: metadata runs with `--locked`, so it cannot write
/// the lockfile.
fn prepare_cached_project(
    cli: &Cli,
    workspace_root: &Path,
    executable: &Path,
    preflight: &mut Preflight,
) -> Result<()> {
    let selection = crate::selection::select(workspace_root, &cli.cargo_args)?;
    if selection
        .command
        .as_deref()
        .is_some_and(|c| c == "run" || c == "r")
    {
        preflight.runs_program = selection.command_index;
    }
    if !selection.is_cacheable() {
        bail!(
            "only build, check, clippy, test, run, bench and doc invocations are cached, and not \
             --help or --version"
        );
    }
    if let Some(name) = crate::cargo_config::declined_environment() {
        bail!(
            "{name} changes what Cargo builds in a way cargo-reapi neither reads nor keys \
             (a lockfile path, compiler, wrapper, build directory or unstable option)"
        );
    }
    if let Some((origin, key)) = crate::cargo_config::declined_setting(workspace_root)? {
        bail!(
            "{origin} sets {key}, which changes what Cargo builds in a way cargo-reapi neither \
             reads nor keys"
        );
    }

    if let Some(missing) = cli
        .declared_inputs
        .iter()
        .find(|input| !workspace_root.join(input).exists())
    {
        bail!("declared input does not exist: {}", missing.display());
    }

    let cache_dir = cli
        .cache_dir
        .as_deref()
        .context("cache directory is required")?;
    let project = Project::resolve(workspace_root, cache_dir, &selection.metadata_arguments)?;
    let configured = selection
        .target_dir
        .or_else(|| env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| project.target_directory.clone());
    let target_root = resolve_target_root_from(workspace_root, Some(configured))?;
    preflight.target_root = Some(target_root.clone());

    // The sandbox policy assumes a target disjoint from the project, so a
    // project is kept for the uncached run only once its target passes.
    validate_driver_paths(executable, &target_root)?;
    project.validate_target(workspace_root, &target_root, &cli.declared_inputs)?;
    crate::gate::validate_administrative_directory(&target_root)?;
    preflight.project = Some(project);
    Ok(())
}

/// An error message as one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Runs the user's exact Cargo command without caching: nothing is
/// restored, recorded or wrapped. With a resolved project the strict
/// sandbox still applies; without one no policy can be built and Cargo runs
/// plainly.
fn run_uncached(cli: &Cli, context: &DriverContext, reason: &str) -> Result<i32> {
    let mut cargo = match &context.project {
        Some(project) => {
            let cache_dir = cli
                .cache_dir
                .as_deref()
                .context("cache directory is required")?;
            let action_log = context.target_root.join("cargo-reapi/actions.jsonl");
            crate::hermetic::cargo_command(
                SnapshotPolicy::Strict,
                &context.workspace_root,
                &context.target_root,
                cache_dir,
                &action_log,
                &cli.declared_inputs,
                Some(project),
            )
            .context(SANDBOX_UNAVAILABLE)?
            .command
        }
        None => Command::new("cargo"),
    };
    eprintln!("cargo-reapi: not caching ({reason}); running Cargo uncached");
    let status = cargo
        .args(&cli.cargo_args)
        .status()
        .context("starting Cargo")?;
    Ok(status.code().unwrap_or(1))
}

/// Cargo's target directory, canonical even before Cargo creates it.
fn resolve_target_root_from(workspace_root: &Path, configured: Option<PathBuf>) -> Result<PathBuf> {
    let target_root = configured.map_or_else(
        || workspace_root.join("target"),
        |path| workspace_root.join(path),
    );
    crate::project::canonical(&target_root)
        .with_context(|| format!("resolving Cargo target directory {}", target_root.display()))
}

fn validate_driver_paths(executable: &Path, target_root: &Path) -> Result<()> {
    if executable.starts_with(target_root) {
        bail!(
            "cargo-reapi executable {} is inside the active Cargo target directory {}. \
             This usually means CARGO_TARGET_DIR leaked from a parent Cargo invocation into a \
             nested cargo-reapi build. Unset CARGO_TARGET_DIR for the child build or select a \
             separate external target directory; cargo-reapi will not manage a target that \
             contains its own running executable",
            executable.display(),
            target_root.display()
        );
    }
    Ok(())
}

fn configure_reclient_environment(cargo: &mut Command, cli: &Cli) {
    for (name, value) in [
        (
            "CARGO_REAPI_REWRAPPER",
            cli.rewrapper
                .as_ref()
                .map(|path| path.as_os_str().to_owned()),
        ),
        (
            "CARGO_REAPI_REWRAPPER_CFG",
            cli.rewrapper_cfg
                .as_ref()
                .map(|path| path.as_os_str().to_owned()),
        ),
        (
            "CARGO_REAPI_RECLIENT_STAGING_DIR",
            cli.reclient_staging_dir
                .as_ref()
                .map(|path| path.as_os_str().to_owned()),
        ),
        (
            "CARGO_REAPI_RECLIENT_PLATFORM",
            cli.reclient_platform.as_ref().map(OsString::from),
        ),
    ] {
        if let Some(value) = value {
            cargo.env(name, value);
        }
    }
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

#[cfg(test)]
mod cli_tests {
    use std::path::Path;

    use clap::{Parser, error::ErrorKind};

    use std::ffi::OsString;

    use super::{Cli, build_arguments, resolve_target_root_from, validate_driver_paths};

    fn arguments(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn run_is_cached_as_the_build_it_contains() {
        assert_eq!(
            build_arguments(
                &arguments(&["--locked", "run", "-p", "tool", "--", "--all"]),
                1
            ),
            arguments(&["--locked", "build", "-p", "tool"])
        );
        assert_eq!(
            build_arguments(&arguments(&["r", "--bin", "tool"]), 0),
            arguments(&["build", "--bin", "tool"])
        );
    }

    #[test]
    fn version_identifies_cargo_reapi_instead_of_delegating_to_cargo() {
        let error = Cli::try_parse_from(["cargo-reapi", "--version"])
            .expect_err("--version exits through clap");

        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert_eq!(
            error.to_string(),
            format!("cargo reapi {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn rejects_driver_executable_inside_managed_target_with_leak_diagnostic() {
        let error = validate_driver_paths(
            Path::new("/workspace/target/debug/cargo-reapi"),
            Path::new("/workspace/target"),
        )
        .expect_err("driver inside managed target must fail closed");
        let message = error.to_string();

        assert!(message.contains("CARGO_TARGET_DIR leaked"), "{message}");
        assert!(message.contains("/workspace/target"), "{message}");
    }

    #[test]
    fn permits_installed_driver_with_external_managed_target() {
        validate_driver_paths(
            Path::new("/usr/local/bin/cargo-reapi"),
            Path::new("/build/cargo-target"),
        )
        .expect("installed driver may manage an external target");
    }

    #[test]
    fn resolves_relative_target_directory_against_workspace() {
        let resolved = resolve_target_root_from(
            Path::new("/workspace/project"),
            Some("shared-target".into()),
        )
        .expect("resolve relative target");

        assert_eq!(resolved, Path::new("/workspace/project/shared-target"));
    }
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
