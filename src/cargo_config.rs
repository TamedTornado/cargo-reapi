//! Cargo's configuration as Cargo itself would read it for one invocation.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// One configuration source and its contents.
pub struct Layer {
    pub origin: String,
    pub table: toml::Table,
}

/// The configuration a Cargo run in `cwd` with `cargo_args` reads, highest
/// precedence first: every `--config` argument (the last first), then
/// `.cargo/config.toml` (or the older `.cargo/config`) in the working
/// directory and each ancestor, then in `CARGO_HOME`.
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

    let mut directories = cwd
        .ancestors()
        .map(|directory| directory.join(".cargo"))
        .collect::<Vec<_>>();
    if let Some(cargo_home) = crate::gate::cargo_home() {
        directories.push(cargo_home);
    }
    for directory in directories {
        for file in ["config.toml", "config"] {
            let path = directory.join(file);
            if path.is_file() {
                layers.push(read_file(&path)?);
            }
        }
    }
    Ok(layers)
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
