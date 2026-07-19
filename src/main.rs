mod acceptance;
mod action;
mod cache;
mod capture;
mod gate;
mod invocation;
mod proof;
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

use crate::cache::{CacheOptions, execute_cached};
use crate::capture::{CaptureOptions, capture_invocation};
use crate::gate::GateSnapshot;
use crate::invocation::RustcInvocation;
use crate::reclient::{ReclientOptions, execute_reapi, validate_platform_template};

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
        #[arg(long)]
        report: PathBuf,
    },
    /// Validate a warm action log against the embedded zero-physical-action contract.
    ActionLog {
        #[arg(long)]
        action_log: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Validate that N or 2N complete gates were truly simultaneous and physically warm.
    Population {
        #[arg(long, value_enum)]
        kind: PopulationProofKind,
        #[arg(long)]
        evidence: PathBuf,
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

#[derive(Debug, Subcommand)]
enum ContractCommand {
    /// Print the immutable acceptance contract embedded in this binary.
    Show,
    /// Verify that a contract file exactly matches the embedded contract.
    Verify {
        #[arg(long, default_value = "acceptance/contract.toml")]
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
    let mut gate_snapshot = if matches!(cli.backend, Backend::Cache)
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
        )?)
    } else {
        None
    };
    let mut cargo = Command::new("cargo");
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
    if let Some(cache_dir) = cache_dir {
        cargo.env("CARGO_REAPI_CACHE_DIR", cache_dir);
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
        ProveCommand::Environment { report } => {
            let proof = proof::EnvironmentProof::capture()?;
            proof.write_and_require_pass(&report)?;
            println!("PASS  {}", report.display());
        }
        ProveCommand::ActionLog { action_log, report } => {
            let proof = proof::ActionLogProof::verify(&action_log)?;
            proof.write_and_require_pass(&report)?;
            println!("PASS  {}", report.display());
        }
        ProveCommand::Population {
            kind,
            evidence,
            report,
        } => {
            let kind = match kind {
                PopulationProofKind::Single => proof::PopulationKind::Single,
                PopulationProofKind::Five => proof::PopulationKind::Five,
                PopulationProofKind::Stress => proof::PopulationKind::Stress,
            };
            let proof = proof::PopulationProof::verify(&evidence, kind)?;
            proof.write_and_require_pass(&report)?;
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
    let arguments = args
        .iter()
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    writeln!(
        capture,
        "{}",
        serde_json::to_string(&arguments).context("serializing linker arguments")?
    )
    .context("writing linker capture")?;
    FileExt::unlock(&capture).context("unlocking linker capture")?;
    let status = Command::new(&real_linker)
        .args(args.into_iter().skip(1))
        .status()
        .with_context(|| format!("executing real linker {}", real_linker.display()))?;
    Ok(status.code().unwrap_or(1))
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
