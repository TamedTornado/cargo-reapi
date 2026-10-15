//! The Cargo configuration files a Cargo run reads. cargo-reapi does not
//! interpret Cargo's configuration; it only looks for entries it must key
//! on or refuse.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// One configuration source and its contents.
pub struct Layer {
    pub origin: String,
    pub table: toml::Table,
}

/// Cargo's home: `CARGO_HOME`, or `$HOME/.cargo` when it is unset or empty,
/// as Cargo itself falls back.
pub fn cargo_home() -> Option<PathBuf> {
    env::var_os("CARGO_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".cargo"))
        })
}

/// Every configuration file a Cargo run in `cwd` may read: `.cargo/config`
/// and `.cargo/config.toml` in the working directory and each ancestor, then
/// in Cargo's home. Where both names exist Cargo reads only `config`; both
/// are listed, so that a search over them finds at least what Cargo sees.
pub fn files(cwd: &Path) -> Vec<PathBuf> {
    let mut directories = cwd
        .ancestors()
        .map(|directory| directory.join(".cargo"))
        .collect::<Vec<_>>();
    directories.extend(cargo_home());

    directories
        .iter()
        .flat_map(|directory| ["config", "config.toml"].map(|name| directory.join(name)))
        .filter(|path| path.is_file())
        .collect()
}

/// The configuration a Cargo run in `cwd` with `cargo_args` reads: every
/// `--config` argument, then every file from [`files`].
///
/// Fails when a source cannot be read completely, including one that
/// `include`s further configuration, which cargo-reapi does not follow.
pub fn layers(cwd: &Path, cargo_args: &[OsString]) -> Result<Vec<Layer>> {
    let mut layers = Vec::new();
    for argument in config_arguments(cargo_args).iter().rev() {
        let path = cwd.join(argument);
        if path.is_file() {
            layers.push(read_file(&path)?);
        } else {
            layers.push(parse(&argument.to_string_lossy(), "a --config argument")?);
        }
    }

    for path in files(cwd) {
        layers.push(read_file(&path)?);
    }
    Ok(layers)
}

/// The file defining `name` as a user alias for a Cargo run in `cwd`, if
/// any. Cargo expands aliases before it applies `--config` arguments, so
/// only files count. Fails when a file cannot be read completely.
pub fn defines_alias(cwd: &Path, name: &str) -> Result<Option<String>> {
    for path in files(cwd) {
        let layer = read_file(&path)?;
        let defined = layer
            .table
            .get("alias")
            .and_then(toml::Value::as_table)
            .is_some_and(|aliases| aliases.contains_key(name));
        if defined {
            return Ok(Some(layer.origin));
        }
    }
    Ok(None)
}

/// Configuration that changes what Cargo builds in ways cargo-reapi neither
/// reads nor keys, so an invocation under it is not cached. Each is a dotted
/// key, or a whole table for a trailing `.*`.
const DECLINED_SETTINGS: [&str; 7] = [
    "resolver.lockfile-path",
    "build.rustc",
    "build.rustdoc",
    "build.rustc-wrapper",
    "build.rustc-workspace-wrapper",
    "build.build-dir",
    "unstable.*",
];

/// The first configuration file that sets one of the [`DECLINED_SETTINGS`],
/// as `(origin, key)`. A table such as `[unstable]` counts however empty.
/// Fails when a file cannot be read completely.
pub fn declined_setting(cwd: &Path) -> Result<Option<(String, &'static str)>> {
    for path in files(cwd) {
        let layer = read_file(&path)?;
        for key in DECLINED_SETTINGS {
            if sets(&layer.table, key) {
                return Ok(Some((layer.origin, key)));
            }
        }
    }
    Ok(None)
}

fn sets(table: &toml::Table, key: &str) -> bool {
    if let Some(name) = key.strip_suffix(".*") {
        return table.contains_key(name);
    }
    let mut current = table;
    let mut parts = key.split('.').peekable();
    while let Some(part) = parts.next() {
        let Some(value) = current.get(part) else {
            return false;
        };
        if parts.peek().is_none() {
            return true;
        }
        let Some(next) = value.as_table() else {
            return false;
        };
        current = next;
    }
    false
}

/// Environment variables that do what a [`DECLINED_SETTINGS`] entry does.
/// An empty value counts as unset, as it does for Cargo's wrapper variables.
pub fn declined_environment() -> Option<String> {
    const NAMES: [&str; 8] = [
        "CARGO_RESOLVER_LOCKFILE_PATH",
        "CARGO_BUILD_RUSTC",
        "CARGO_BUILD_RUSTDOC",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_BUILD_DIR",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ];
    let set = |name: &str| env::var_os(name).is_some_and(|value| !value.is_empty());
    NAMES
        .iter()
        .find(|name| set(name))
        .map(|name| (*name).to_owned())
        .or_else(|| {
            env::vars_os()
                .filter_map(|(name, _)| name.into_string().ok())
                .find(|name| name.starts_with("CARGO_UNSTABLE_"))
        })
}

fn read_file(path: &Path) -> Result<Layer> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading Cargo configuration {}", path.display()))?;
    parse(&text, &path.display().to_string())
}

fn parse(text: &str, origin: &str) -> Result<Layer> {
    let table: toml::Table = text
        .parse()
        .with_context(|| format!("parsing Cargo configuration from {origin}"))?;
    if table.contains_key("include") {
        bail!("{origin} includes further configuration, which cargo-reapi does not follow");
    }
    Ok(Layer {
        origin: origin.to_owned(),
        table,
    })
}

/// The values of Cargo's `--config` options. Arguments after `--` belong to
/// the program Cargo runs.
pub fn config_arguments(cargo_args: &[OsString]) -> Vec<OsString> {
    let mut values = Vec::new();
    let mut arguments = cargo_args.iter();
    while let Some(argument) = arguments.next() {
        if argument == "--" {
            break;
        }
        if argument == "--config" {
            if let Some(value) = arguments.next() {
                values.push(value.clone());
            }
        } else if let Some(value) = argument
            .to_str()
            .and_then(|text| text.strip_prefix("--config="))
        {
            values.push(OsString::from(value));
        }
    }
    values
}
