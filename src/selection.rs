//! What a Cargo invocation selects: the project, its lockfile and network
//! policy, its features and its target directory. cargo-reapi prepares that
//! project outside the strict sandbox, so it must read the invocation the
//! way Cargo does, aliases included.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Cargo's own commands. A user alias cannot redefine them.
const BUILT_IN_COMMANDS: [&str; 39] = [
    "add",
    "bench",
    "build",
    "check",
    "clean",
    "config",
    "doc",
    "fetch",
    "fix",
    "generate-lockfile",
    "git-checkout",
    "help",
    "info",
    "init",
    "install",
    "locate-project",
    "login",
    "logout",
    "metadata",
    "new",
    "owner",
    "package",
    "pkgid",
    "publish",
    "read-manifest",
    "remove",
    "report",
    "run",
    "rustc",
    "rustdoc",
    "search",
    "test",
    "tree",
    "uninstall",
    "update",
    "vendor",
    "verify-project",
    "version",
    "yank",
];

/// Cargo's built-in aliases, which user aliases may redefine.
const BUILT_IN_ALIASES: [(&str, &str); 6] = [
    ("b", "build"),
    ("c", "check"),
    ("d", "doc"),
    ("r", "run"),
    ("rm", "remove"),
    ("t", "test"),
];

/// Global options that take a value before the subcommand.
const GLOBAL_VALUE_OPTIONS: [&str; 4] = ["--config", "--color", "-Z", "-C"];

/// Short options whose value follows them, attached or as the next
/// argument. `-p` takes an optional value.
const VALUE_SHORTS: [char; 6] = ['m', 'F', 'Z', 'C', 'j', 'p'];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// The options `cargo metadata` takes to resolve the selected project
    /// as the invocation would: its manifest, lockfile and network policy,
    /// configuration and, when features are selected, every feature, so
    /// that each optional dependency they can activate is included.
    pub metadata_arguments: Vec<OsString>,
    /// A `--target-dir` option, which overrides every other setting.
    pub target_dir: Option<PathBuf>,
    /// The invocation's arguments with its alias expanded.
    pub expanded: Vec<OsString>,
}

/// Reads what `cargo_args`, run in `cwd`, select, after expanding a user
/// or built-in alias the way Cargo does.
pub fn select(cwd: &Path, cargo_args: &[OsString]) -> Result<Selection> {
    let expanded = expand_aliases(cwd, cargo_args)?;
    let mut selection = parse(&expanded)?;
    selection.expanded = expanded;
    Ok(selection)
}

/// `cargo_args` with an aliased subcommand replaced by its expansion,
/// repeatedly, as Cargo expands it. Fails when the configuration that
/// could define the alias cannot be read, since the expansion may select a
/// different project or lockfile policy.
pub fn expand_aliases(cwd: &Path, cargo_args: &[OsString]) -> Result<Vec<OsString>> {
    let mut arguments = cargo_args.to_vec();
    let Some(position) = subcommand_position(&arguments) else {
        return Ok(arguments);
    };

    let mut seen = BTreeSet::new();
    loop {
        let Some(name) = arguments[position].to_str().map(str::to_owned) else {
            return Ok(arguments);
        };
        if BUILT_IN_COMMANDS.contains(&name.as_str()) {
            return Ok(arguments);
        }
        if !seen.insert(name.clone()) {
            bail!("Cargo alias `{name}` expands to itself");
        }

        let expansion = match user_alias(cwd, cargo_args, &name)? {
            Some(expansion) => expansion,
            None => match BUILT_IN_ALIASES.iter().find(|(alias, _)| *alias == name) {
                Some((_, command)) => vec![(*command).to_owned()],
                // An external subcommand, which cargo-reapi cannot see into.
                None => return Ok(arguments),
            },
        };
        arguments.splice(
            position..=position,
            expansion.into_iter().map(OsString::from),
        );
    }
}

/// The index of the subcommand: the first argument that is neither an
/// option nor a global option's value.
fn subcommand_position(arguments: &[OsString]) -> Option<usize> {
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        let text = argument.to_str()?;
        if text == "--" {
            return None;
        }
        if GLOBAL_VALUE_OPTIONS.contains(&text) {
            index += 2;
        } else if text.starts_with('-') {
            index += 1;
        } else {
            return Some(index);
        }
    }
    None
}

/// A user alias's expansion, from `CARGO_ALIAS_<NAME>` or `[alias]` in the
/// configuration, which `--config` overrides.
fn user_alias(cwd: &Path, cargo_args: &[OsString], name: &str) -> Result<Option<Vec<String>>> {
    let layers = crate::cargo_config::layers(cwd, cargo_args)
        .with_context(|| format!("reading the configuration that may define the `{name}` alias"))?;
    let (overrides, files): (Vec<_>, Vec<_>) = layers
        .into_iter()
        .partition(|layer| layer.origin == "a --config argument");

    let configured = |layers: &[crate::cargo_config::Layer]| -> Result<Option<Vec<String>>> {
        for layer in layers {
            if let Some(value) = layer.table.get("alias").and_then(|alias| alias.get(name)) {
                return alias_words(value, &layer.origin).map(Some);
            }
        }
        Ok(None)
    };
    if let Some(words) = configured(&overrides)? {
        return Ok(Some(words));
    }

    let variable = format!("CARGO_ALIAS_{}", name.to_uppercase().replace('-', "_"));
    if let Some(value) = env::var_os(&variable) {
        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("{variable} is not UTF-8"))?;
        return Ok(Some(value.split_whitespace().map(str::to_owned).collect()));
    }
    configured(&files)
}

fn alias_words(value: &toml::Value, origin: &str) -> Result<Vec<String>> {
    match value {
        toml::Value::String(text) => Ok(text.split_whitespace().map(str::to_owned).collect()),
        toml::Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .with_context(|| format!("an alias in {origin} lists a non-string"))
            })
            .collect(),
        _ => bail!("an alias in {origin} is neither a string nor a list"),
    }
}

/// Reads the selection from already expanded arguments.
fn parse(arguments: &[OsString]) -> Result<Selection> {
    let mut parser = Parser::default();
    let mut remaining = arguments.iter().peekable();
    while let Some(argument) = remaining.next() {
        let Some(text) = argument.to_str() else {
            continue;
        };
        if text == "--" {
            break;
        }

        if let Some(long) = text.strip_prefix("--") {
            parser.long(long, &mut remaining);
        } else if let Some(shorts) = text.strip_prefix('-')
            && !shorts.is_empty()
        {
            parser.shorts(shorts, &mut remaining)?;
        }
    }
    Ok(parser.finish())
}

type Remaining<'a> = std::iter::Peekable<std::slice::Iter<'a, OsString>>;

#[derive(Default)]
struct Parser {
    metadata_arguments: Vec<OsString>,
    features: bool,
    target_dir: Option<PathBuf>,
}

impl Parser {
    fn long(&mut self, long: &str, remaining: &mut Remaining<'_>) {
        let (name, attached) = match long.split_once('=') {
            Some((name, value)) => (name, Some(OsString::from(value))),
            None => (long, None),
        };
        let mut value = || attached.clone().or_else(|| remaining.next().cloned());

        match name {
            "locked" | "frozen" | "offline" => {
                self.metadata_arguments
                    .push(OsString::from(format!("--{name}")));
            }
            "manifest-path" | "config" => {
                if let Some(value) = value() {
                    self.metadata_arguments
                        .extend([OsString::from(format!("--{name}")), value]);
                }
            }
            "features" => {
                let _ = value();
                self.features = true;
            }
            "all-features" => self.features = true,
            "target-dir" => self.target_dir = value().map(PathBuf::from),
            _ => {}
        }
    }

    /// A cluster of short options such as `-q`, `-mtool/Cargo.toml` or
    /// `-vF=extra`.
    fn shorts(&mut self, shorts: &str, remaining: &mut Remaining<'_>) -> Result<()> {
        for (offset, short) in shorts.char_indices() {
            if !VALUE_SHORTS.contains(&short) {
                continue;
            }

            let attached = &shorts[offset + short.len_utf8()..];
            let attached = attached.strip_prefix('=').unwrap_or(attached);
            let value = if !attached.is_empty() {
                Some(OsString::from(attached))
            } else if short == 'p' {
                remaining
                    .next_if(|next| !next.to_string_lossy().starts_with('-'))
                    .cloned()
            } else {
                remaining.next().cloned()
            };

            match short {
                'm' => {
                    if let Some(value) = value {
                        self.metadata_arguments
                            .extend([OsString::from("--manifest-path"), value]);
                    }
                }
                'Z' => {
                    if let Some(value) = value {
                        self.metadata_arguments
                            .extend([OsString::from("-Z"), value]);
                    }
                }
                'F' => self.features = true,
                'C' => bail!(
                    "cargo-reapi does not support Cargo's -C; run it from the directory instead"
                ),
                _ => {}
            }
            return Ok(());
        }
        Ok(())
    }

    fn finish(mut self) -> Selection {
        if self.features {
            self.metadata_arguments
                .push(OsString::from("--all-features"));
        }
        Selection {
            metadata_arguments: self.metadata_arguments,
            target_dir: self.target_dir,
            expanded: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use super::{expand_aliases, parse};

    fn arguments(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn metadata_arguments(words: &[&str]) -> Vec<OsString> {
        parse(&arguments(words))
            .expect("selection")
            .metadata_arguments
    }

    #[test]
    fn manifest_lockfile_configuration_and_features_reach_metadata() {
        let selection = metadata_arguments(&[
            "build",
            "--manifest-path",
            "vendor/tool/Cargo.toml",
            "-p",
            "tool",
            "--locked",
            "--config=net.retry=2",
            "-Zunstable-options",
            "--features",
            "extra",
            "--",
            "--offline",
        ]);

        let expected = arguments(&[
            "--manifest-path",
            "vendor/tool/Cargo.toml",
            "--locked",
            "--config",
            "net.retry=2",
            "-Z",
            "unstable-options",
            "--all-features",
        ]);
        assert_eq!(selection, expected);
    }

    #[test]
    fn short_options_are_read_separated_attached_and_clustered() {
        let manifest = arguments(&["--manifest-path", "tool/Cargo.toml"]);
        for words in [
            &["build", "-m", "tool/Cargo.toml"][..],
            &["build", "-mtool/Cargo.toml"],
            &["build", "-m=tool/Cargo.toml"],
            &["build", "-qm", "tool/Cargo.toml"],
            &["build", "-p", "-mtool/Cargo.toml"],
        ] {
            assert_eq!(metadata_arguments(words), manifest, "{words:?}");
        }

        // `-p`'s value is a package, not a manifest.
        assert!(metadata_arguments(&["build", "-p", "m"]).is_empty());
        assert_eq!(
            metadata_arguments(&["build", "-vFextra"]),
            arguments(&["--all-features"])
        );
        assert!(parse(&arguments(&["-C", "elsewhere", "build"])).is_err());
    }

    #[test]
    fn a_target_dir_option_is_kept() {
        let selection = parse(&arguments(&["build", "--target-dir=out"])).expect("selection");
        assert_eq!(selection.target_dir.as_deref(), Some("out".as_ref()));
    }

    #[test]
    fn aliases_expand_as_cargo_expands_them() {
        let project = tempfile::tempdir().expect("project");
        fs::create_dir_all(project.path().join(".cargo")).expect("configuration directory");
        fs::write(
            project.path().join(".cargo/config.toml"),
            "[alias]\nci = 'lint --locked'\nlint = ['clippy', '-mtool/Cargo.toml']\nbuild = 'check'\n",
        )
        .expect("configuration");

        let expanded =
            expand_aliases(project.path(), &arguments(&["ci", "--offline"])).expect("expansion");
        assert_eq!(
            expanded,
            arguments(&["clippy", "-mtool/Cargo.toml", "--locked", "--offline"])
        );

        // Built-in commands cannot be redefined; built-in aliases expand.
        let build = expand_aliases(project.path(), &arguments(&["build"])).expect("build");
        assert_eq!(build, arguments(&["build"]));
        let short = expand_aliases(project.path(), &arguments(&["--locked", "b"])).expect("b");
        assert_eq!(short, arguments(&["--locked", "build"]));
    }

    #[test]
    fn an_alias_that_cannot_be_read_is_refused() {
        let project = tempfile::tempdir().expect("project");
        fs::create_dir_all(project.path().join(".cargo")).expect("configuration directory");
        fs::write(
            project.path().join(".cargo/config.toml"),
            "include = 'more.toml'\n",
        )
        .expect("configuration");

        assert!(expand_aliases(project.path(), &arguments(&["ci"])).is_err());
        assert!(expand_aliases(project.path(), &arguments(&["build"])).is_ok());
    }
}
