mod action;
#[cfg(unix)]
mod beneath;
mod cache;
mod capture;
mod cargo_config;
mod environment;
mod footprint;
mod invocation;
mod maintenance;
mod project;
mod reclient;
mod relocation;
mod resource;
mod rlib;
mod safe_io;

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use crate::cache::{CacheOptions, execute_cached};
use crate::capture::{CaptureOptions, capture_invocation};
use crate::environment::{
    CALLER_ENVIRONMENT, CONFIG_ENVIRONMENT, RUNTIME_PLUMBING_ENVIRONMENT, caller_record,
    config_record,
};
use crate::invocation::RustcInvocation;
use crate::maintenance::{GcOptions, cache_stats, collect_garbage};
use crate::reclient::{ReclientOptions, execute_reapi, validate_platform_template};
use crate::resource::ResourceCapacity;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Local,
    Capture,
    Cache,
    Reapi,
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
    resource_capacity: ResourceCapacity,
    cache: maintenance::CacheStats,
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
    let args = strip_cargo_subcommand_name(args);
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
        resource_capacity: ResourceCapacity::from_env()?,
        cache: cache_stats(&cli.cache_dir)?,
    };
    if !cli.json {
        eprintln!(
            "cargo-reapi doctor passed: cpu={} memory_gib={}",
            report.resource_capacity.cpu, report.resource_capacity.memory_gib
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
    let executable = env::current_exe()
        .context("locating cargo-reapi executable")?
        .canonicalize()
        .context("resolving cargo-reapi executable")?;
    let workspace_root =
        crate::project::canonical(&env::current_dir().context("locating Cargo workspace root")?)?;

    // cargo-reapi is Cargo's compiler wrapper, so a wrapper the user already
    // set would be replaced: that invocation runs as plain Cargo.
    if let Some(reason) = user_wrapper(&workspace_root, &cli.cargo_args) {
        eprintln!("cargo-reapi: not caching ({reason}); running Cargo uncached");
        let status = Command::new("cargo")
            .args(&cli.cargo_args)
            .status()
            .context("starting Cargo")?;
        return Ok(status.code().unwrap_or(1));
    }

    let target_root = resolve_target_root(cli, &workspace_root)?;
    validate_driver_paths(&executable, &target_root)?;
    run_cargo(cli, &executable, &workspace_root, &target_root)
}

/// Runs Cargo with cargo-reapi as its compiler wrapper. Cargo runs every
/// time; only the compiles it asks the wrapper for are cached.
fn run_cargo(
    cli: &Cli,
    executable: &Path,
    workspace_root: &Path,
    target_root: &Path,
) -> Result<i32> {
    // Compiles run in their package's directory, so a relative log would
    // land beside each package rather than where the caller asked.
    let action_log = cli.action_log.as_ref().map_or_else(
        || target_root.join("cargo-reapi/actions.jsonl"),
        |path| workspace_root.join(path),
    );
    let mut cargo = Command::new("cargo");
    if matches!(cli.backend, Backend::Cache | Backend::Reapi) {
        ResourceCapacity::from_env()?.apply_to(&mut cargo);
    }
    cargo.args(&cli.cargo_args);
    describe_compile_environment(&mut cargo, workspace_root, &cli.cargo_args);
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
    if let Some(cache_dir) = &cli.cache_dir {
        cargo
            .env(
                "CARGO_REAPI_RESOURCE_LEDGER",
                resource::ledger_root(cache_dir)?,
            )
            .env("CARGO_REAPI_CACHE_DIR", cache_dir);
    }
    configure_reclient_environment(&mut cargo, cli);
    let status = cargo.status().context("starting Cargo")?;
    Ok(status.code().unwrap_or(1))
}

/// Why this invocation cannot use cargo-reapi as its compiler wrapper, when
/// the user's own configuration already names one.
fn user_wrapper(workspace_root: &Path, cargo_args: &[OsString]) -> Option<String> {
    if let Some(name) = crate::cargo_config::wrapper_environment() {
        return Some(format!("{name} already names a compiler wrapper"));
    }
    match crate::cargo_config::wrapper_setting(workspace_root, cargo_args) {
        Ok(None) => None,
        Ok(Some((origin, key))) => Some(format!("{origin} sets {key}, a compiler wrapper")),
        Err(error) => Some(one_line(&format!(
            "cargo-reapi cannot tell whether a compiler wrapper is configured: {error:#}"
        ))),
    }
}

/// Cargo's target directory: `--target-dir`, else `CARGO_TARGET_DIR`, else
/// what Cargo reports for the working directory's project given the same
/// `--manifest-path` and `--config` arguments.
fn resolve_target_root(cli: &Cli, workspace_root: &Path) -> Result<PathBuf> {
    let mut target_dir = None;
    let mut manifest_path = None;
    let mut arguments = cli.cargo_args.iter();
    while let Some(argument) = arguments.next() {
        let Some(text) = argument.to_str() else {
            continue;
        };
        if text == "--" {
            break;
        }
        for (name, slot) in [
            ("--target-dir", &mut target_dir),
            ("--manifest-path", &mut manifest_path),
        ] {
            if text == name {
                *slot = arguments.next().cloned();
            } else if let Some(value) = text.strip_prefix(&format!("{name}=")) {
                *slot = Some(OsString::from(value));
            }
        }
    }
    // `--config build.target-dir=...` moves the target directory as well.
    let metadata_arguments = manifest_path
        .into_iter()
        .flat_map(|path| [OsString::from("--manifest-path"), path])
        .chain(
            crate::cargo_config::config_arguments(&cli.cargo_args)
                .into_iter()
                .flat_map(|value| [OsString::from("--config"), value]),
        )
        .collect::<Vec<_>>();
    let configured = target_dir
        .map(PathBuf::from)
        .or_else(|| env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .or_else(|| crate::project::target_directory(workspace_root, &metadata_arguments));
    let target_root = configured.map_or_else(
        || workspace_root.join("target"),
        |path| workspace_root.join(path),
    );
    crate::project::canonical(&target_root)
        .with_context(|| format!("resolving Cargo target directory {}", target_root.display()))
}

/// An error message as one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
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

    use super::{Cli, validate_driver_paths};

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
}
