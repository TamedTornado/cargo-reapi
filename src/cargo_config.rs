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

/// Configuration keys that name a compiler wrapper. cargo-reapi is Cargo's
/// compiler wrapper, so a wrapper the user already chose would be replaced.
const WRAPPER_SETTINGS: [(&str, &str); 2] = [
    ("build", "rustc-wrapper"),
    ("build", "rustc-workspace-wrapper"),
];

/// The first configuration file that sets a compiler wrapper, as
/// `(origin, key)`. Fails when a file cannot be read completely.
pub fn wrapper_setting(cwd: &Path) -> Result<Option<(String, String)>> {
    for path in files(cwd) {
        let layer = read_file(&path)?;
        for (table, key) in WRAPPER_SETTINGS {
            let set = layer
                .table
                .get(table)
                .and_then(toml::Value::as_table)
                .is_some_and(|values| values.contains_key(key));
            if set {
                return Ok(Some((layer.origin, format!("{table}.{key}"))));
            }
        }
    }
    Ok(None)
}

/// The environment variable that already names a compiler wrapper, if any.
/// An empty value counts as unset, as it does for Cargo.
pub fn wrapper_environment() -> Option<String> {
    const NAMES: [&str; 4] = [
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    ];
    NAMES
        .iter()
        .find(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
        .map(|name| (*name).to_owned())
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
