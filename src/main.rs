mod action;
mod cache;
mod capture;
mod invocation;
mod reclient;

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};

use crate::cache::{CacheOptions, execute_cached};
use crate::capture::{CaptureOptions, capture_invocation};
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
#[command(name = "cargo reapi", bin_name = "cargo reapi")]
struct Cli {
    /// Execution backend. Capture executes locally and records hermetic action inputs.
    #[arg(long, value_enum, default_value_t = Backend::Capture)]
    backend: Backend,

    /// JSON Lines action log. Defaults to target/cargo-reapi/actions.jsonl.
    #[arg(long)]
    action_log: Option<PathBuf>,

    /// Shared filesystem CAS used by the cache backend.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Path to reclient's rewrapper binary.
    #[arg(long)]
    rewrapper: Option<PathBuf>,

    /// Rewrapper/reproxy configuration file.
    #[arg(long)]
    rewrapper_cfg: Option<PathBuf>,

    /// Action staging root used to construct explicit reclient input trees.
    #[arg(long)]
    reclient_staging_dir: Option<PathBuf>,

    /// Reclient platform properties, including the platform-matched toolchain contract.
    #[arg(long)]
    reclient_platform: Option<String>,

    /// Arguments passed verbatim to Cargo.
    #[arg(last = true, required = true, num_args = 1..)]
    cargo_args: Vec<OsString>,
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
    if RustcInvocation::looks_like_wrapper(&args) {
        return run_wrapper(args);
    }

    let cli = Cli::parse_from(strip_cargo_subcommand_name(args));
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

    let executable = env::current_exe().context("locating cargo-reapi executable")?;
    let workspace_root = env::current_dir().context("locating Cargo workspace root")?;
    let target_root = env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace_root.join("target"), PathBuf::from);
    let action_log = cli
        .action_log
        .unwrap_or_else(|| PathBuf::from("target/cargo-reapi/actions.jsonl"));
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
    if let Some(cache_dir) = cli.cache_dir {
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
    Ok(status.code().unwrap_or(1))
}

fn run_wrapper(args: Vec<OsString>) -> Result<i32> {
    let invocation = RustcInvocation::parse(args)?;
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
