//! What a Cargo invocation selects: the project, its lockfile and network
//! policy, its features and its target directory. cargo-reapi prepares that
//! project outside the strict sandbox, so it must know exactly what Cargo
//! will build.
//!
//! cargo-reapi does not emulate Cargo's command line or configuration. It
//! reads the plain option forms Cargo documents and refuses what it cannot
//! read exactly: user-defined aliases, `--config` overrides and `-Z` flags
//! that can change what Cargo selects, global options before the
//! subcommand that it does not know, and short-option clusters holding an
//! option that takes a value.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Cargo's own commands. A user alias cannot redefine them, so they need
/// no configuration lookup. Cargo's built-in aliases `b`, `c`, `d`, `r`,
/// `rm` and `t` are not listed: a user alias can redefine them.
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

/// Configuration keys a `--config KEY=VALUE` override may set. They change
/// how packages compile, which the gate keys through the arguments, never
/// which project, sources, lockfile or target directory Cargo uses.
const HARMLESS_CONFIG_KEYS: [&str; 3] = ["profile.", "term.", "build.jobs"];

/// Short options that take a value, attached (`-mPATH`, `-m=PATH`) or as
/// the next argument. `-p` takes an optional value.
const VALUE_SHORTS: [char; 6] = ['m', 'F', 'p', 'j', 'Z', 'C'];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// The options `cargo metadata` takes to resolve the selected project
    /// and acquire what the invocation builds: its manifest, lockfile and
    /// network policy and its features.
    pub metadata_arguments: Vec<OsString>,
    /// A `--target-dir` option, which overrides every other setting.
    pub target_dir: Option<PathBuf>,
}

/// Reads what `cargo_args`, run in `cwd`, select, or explains why
/// cargo-reapi cannot tell exactly.
pub fn select(cwd: &Path, cargo_args: &[OsString]) -> Result<Selection> {
    let mut reader = Reader::new(cwd);
    let mut remaining = cargo_args.iter().peekable();

    let Some(command) = reader.global_options(&mut remaining)? else {
        return reader.finish();
    };
    refuse_user_alias(cwd, &command)?;

    while let Some(argument) = remaining.next() {
        if argument == "--" {
            break;
        }
        reader.argument(argument, &mut remaining)?;
    }
    reader.finish()
}

/// Refuses a user-defined alias. Its expansion could select a different
/// project or lockfile policy, and cargo-reapi does not expand aliases.
fn refuse_user_alias(cwd: &Path, command: &str) -> Result<()> {
    if BUILT_IN_COMMANDS.contains(&command) {
        return Ok(());
    }

    let variable = format!("CARGO_ALIAS_{}", command.to_uppercase().replace('-', "_"));
    if env::var_os(&variable).is_some() {
        bail!(
            "`{command}` is a user-defined Cargo alias ({variable}); cargo-reapi does not expand \
             aliases, so spell the command out"
        );
    }

    match crate::cargo_config::defines_alias(cwd, command) {
        Ok(None) => Ok(()),
        Ok(Some(origin)) => bail!(
            "`{command}` is a user-defined Cargo alias in {origin}; cargo-reapi does not expand \
             aliases, so spell the command out"
        ),
        Err(error) => Err(error.context(format!(
            "cargo-reapi cannot tell whether `{command}` is a user-defined Cargo alias; spell the \
             command out as a built-in Cargo command"
        ))),
    }
}

type Remaining<'a> = std::iter::Peekable<std::slice::Iter<'a, OsString>>;

struct Reader<'a> {
    cwd: &'a Path,
    manifest_path: Option<OsString>,
    /// `--locked`, `--frozen` and `--offline`, each once however often
    /// given, since `cargo metadata` refuses a repeated flag.
    lockfile_policy: BTreeSet<&'static str>,
    features: Vec<OsString>,
    all_features: bool,
    no_default_features: bool,
    selects_packages: bool,
    target_dir: Option<PathBuf>,
}

impl<'a> Reader<'a> {
    fn new(cwd: &'a Path) -> Self {
        Self {
            cwd,
            manifest_path: None,
            lockfile_policy: BTreeSet::new(),
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            selects_packages: false,
            target_dir: None,
        }
    }

    /// Reads the global options before the subcommand and returns the
    /// subcommand. Only options whose meaning is exact are accepted.
    fn global_options(&mut self, remaining: &mut Remaining<'_>) -> Result<Option<String>> {
        while let Some(argument) = remaining.next() {
            let Some(text) = argument.to_str() else {
                bail!("cargo-reapi cannot read a Cargo argument that is not UTF-8");
            };

            if let Some(long) = text.strip_prefix("--") {
                self.global_long(long, remaining)?;
            } else if let Some(shorts) = text.strip_prefix('-') {
                if shorts.is_empty() || !shorts.chars().all(|short| "vqVh".contains(short)) {
                    bail!(
                        "cargo-reapi cannot read Cargo's `{text}` before the subcommand exactly; \
                         put the subcommand first and spell the command out"
                    );
                }
            } else {
                return Ok(Some(text.to_owned()));
            }
        }
        Ok(None)
    }

    fn global_long(&mut self, long: &str, remaining: &mut Remaining<'_>) -> Result<()> {
        let (name, attached) = split_long(long);
        match name {
            "verbose" | "quiet" | "version" | "help" | "list" => {}
            "locked" | "frozen" | "offline" => self.lockfile_flag(name),
            "color" => {
                if attached.is_none() {
                    remaining.next();
                }
            }
            "config" => {
                let value = attached.or_else(|| remaining.next().cloned());
                self.check_config_override(value)?;
            }
            _ => bail!(
                "cargo-reapi cannot read Cargo's `--{long}` before the subcommand exactly; put \
                 the subcommand first and spell the command out"
            ),
        }
        Ok(())
    }

    fn argument(&mut self, argument: &OsString, remaining: &mut Remaining<'_>) -> Result<()> {
        let Some(text) = argument.to_str() else {
            return Ok(());
        };

        if let Some(long) = text.strip_prefix("--") {
            self.long(long, remaining)
        } else if let Some(shorts) = text.strip_prefix('-')
            && !shorts.is_empty()
        {
            self.shorts(shorts, remaining)
        } else {
            Ok(())
        }
    }

    fn long(&mut self, long: &str, remaining: &mut Remaining<'_>) -> Result<()> {
        let (name, attached) = split_long(long);
        let attached_or_next =
            |remaining: &mut Remaining<'_>| attached.clone().or_else(|| remaining.next().cloned());

        match name {
            "locked" | "frozen" | "offline" => self.lockfile_flag(name),
            "manifest-path" => self.manifest_path(attached_or_next(remaining))?,
            "features" => self.features.extend(attached_or_next(remaining)),
            "all-features" => self.all_features = true,
            "no-default-features" => self.no_default_features = true,
            "package" => {
                if attached.is_none() {
                    remaining.next_if(|next| !is_option(next));
                }
                self.selects_packages = true;
            }
            "exclude" => {
                attached_or_next(remaining);
                self.selects_packages = true;
            }
            "workspace" | "all" => self.selects_packages = true,
            "target-dir" => self.target_dir = attached_or_next(remaining).map(PathBuf::from),
            "config" => self.check_config_override(attached_or_next(remaining))?,
            _ => {}
        }
        Ok(())
    }

    /// A short option, alone or with its value attached.
    fn shorts(&mut self, shorts: &str, remaining: &mut Remaining<'_>) -> Result<()> {
        let mut characters = shorts.chars();
        let Some(first) = characters.next() else {
            return Ok(());
        };
        let rest = characters.as_str();

        if !VALUE_SHORTS.contains(&first) {
            if let Some(short) = rest.chars().find(|short| VALUE_SHORTS.contains(short)) {
                bail!(
                    "cargo-reapi cannot read `-{shorts}` exactly; give `-{short}` as its own \
                     argument"
                );
            }
            return Ok(());
        }

        // `-F=` is an explicitly empty value: it takes nothing that follows.
        let value = if let Some(attached) = rest.strip_prefix('=') {
            Some(OsString::from(attached))
        } else if !rest.is_empty() {
            Some(OsString::from(rest))
        } else if first == 'p' {
            remaining.next_if(|next| !is_option(next)).cloned()
        } else {
            remaining.next().cloned()
        };

        match first {
            'm' => self.manifest_path(value)?,
            'F' => self.features.extend(value),
            'p' => self.selects_packages = true,
            'Z' => bail!(
                "cargo-reapi does not accept Cargo's unstable -Z flags, which can change what \
                 Cargo selects"
            ),
            'C' => bail!("cargo-reapi does not accept Cargo's -C; run it from that directory"),
            _ => {}
        }
        Ok(())
    }

    fn lockfile_flag(&mut self, name: &str) {
        let flag = match name {
            "locked" => "--locked",
            "frozen" => "--frozen",
            _ => "--offline",
        };
        self.lockfile_policy.insert(flag);
    }

    fn manifest_path(&mut self, value: Option<OsString>) -> Result<()> {
        let Some(value) = value else {
            bail!("Cargo's --manifest-path needs a path");
        };
        if self.manifest_path.replace(value).is_some() {
            bail!("cargo-reapi cannot read more than one manifest path");
        }
        Ok(())
    }

    /// Accepts only a `--config KEY=VALUE` override of a key that cannot
    /// change what Cargo selects. A configuration file is refused.
    fn check_config_override(&self, value: Option<OsString>) -> Result<()> {
        let value = value.unwrap_or_default();
        let text = value.to_string_lossy();
        let harmless = !self.cwd.join(&value).exists()
            && text.split_once('=').is_some_and(|(key, _)| {
                HARMLESS_CONFIG_KEYS
                    .iter()
                    .any(|prefix| key.trim().starts_with(prefix))
            });
        if !harmless {
            bail!(
                "cargo-reapi does not accept `--config {text}`, which can change the project, \
                 sources, lockfile or target directory Cargo uses; set it in Cargo's \
                 configuration files instead"
            );
        }
        Ok(())
    }

    fn finish(self) -> Result<Selection> {
        let mut arguments = Vec::new();
        if let Some(manifest_path) = self.manifest_path {
            arguments.extend([OsString::from("--manifest-path"), manifest_path]);
        }
        arguments.extend(self.lockfile_policy.iter().map(OsString::from));
        let offline = ["--frozen", "--offline"]
            .iter()
            .any(|flag| self.lockfile_policy.contains(flag));

        let features: Vec<OsString> = self
            .features
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect();
        let selects_features =
            !features.is_empty() || self.all_features || self.no_default_features;
        if selects_features && self.selects_packages {
            // `cargo metadata` has no package selection, so it cannot apply
            // features to the packages selected with -p, --workspace or
            // --exclude as Cargo does. Every feature covers them, but needs
            // the sources of every optional dependency, which an offline
            // invocation may not have.
            if offline {
                bail!(
                    "cargo-reapi cannot prepare the features of packages selected with -p, \
                     --workspace or --exclude offline; run without --offline or --frozen, or \
                     select the package with --manifest-path"
                );
            }
            arguments.push(OsString::from("--all-features"));
        } else {
            // Without a package selection Cargo applies the features to the
            // package or workspace `cargo metadata` resolves.
            for value in features {
                arguments.extend([OsString::from("--features"), value]);
            }
            if self.all_features {
                arguments.push(OsString::from("--all-features"));
            }
            if self.no_default_features {
                arguments.push(OsString::from("--no-default-features"));
            }
        }

        Ok(Selection {
            metadata_arguments: arguments,
            target_dir: self.target_dir,
        })
    }
}

/// A long option's name and its `=` value, if attached.
fn split_long(long: &str) -> (&str, Option<OsString>) {
    match long.split_once('=') {
        Some((name, value)) => (name, Some(OsString::from(value))),
        None => (long, None),
    }
}

fn is_option(argument: &OsString) -> bool {
    argument.to_string_lossy().starts_with('-')
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::select;

    fn arguments(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// Runs in a directory without Cargo configuration. Every command here
    /// is built in, so no configuration is read.
    fn metadata_arguments(words: &[&str]) -> Vec<OsString> {
        select(Path::new("/"), &arguments(words))
            .unwrap_or_else(|error| panic!("{words:?}: {error:#}"))
            .metadata_arguments
    }

    fn refusal(words: &[&str]) -> String {
        match select(Path::new("/"), &arguments(words)) {
            Ok(selection) => panic!("{words:?} was accepted: {selection:?}"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn manifest_lockfile_and_features_reach_metadata() {
        let selection = metadata_arguments(&[
            "--locked",
            "build",
            "--manifest-path",
            "vendor/tool/Cargo.toml",
            "--locked",
            "--features",
            "extra",
            "--no-default-features",
            "--",
            "--offline",
        ]);

        let expected = arguments(&[
            "--manifest-path",
            "vendor/tool/Cargo.toml",
            "--locked",
            "--features",
            "extra",
            "--no-default-features",
        ]);
        assert_eq!(selection, expected);
    }

    #[test]
    fn manifest_short_options_are_read_separated_and_attached() {
        let manifest = arguments(&["--manifest-path", "tool/Cargo.toml"]);
        for words in [
            &["build", "-m", "tool/Cargo.toml"][..],
            &["build", "-mtool/Cargo.toml"],
            &["build", "-m=tool/Cargo.toml"],
            &["build", "--manifest-path=tool/Cargo.toml"],
            &["build", "-p", "-mtool/Cargo.toml"],
        ] {
            assert_eq!(metadata_arguments(words), manifest, "{words:?}");
        }

        // `-p`'s value is a package, not a manifest.
        assert!(metadata_arguments(&["build", "-p", "m"]).is_empty());
    }

    #[test]
    fn an_explicitly_empty_feature_value_takes_nothing_that_follows() {
        for words in [
            &["build", "-F=", "--locked", "--offline"][..],
            &["build", "--features=", "--locked", "--offline"],
        ] {
            assert_eq!(
                metadata_arguments(words),
                arguments(&["--locked", "--offline"]),
                "{words:?}"
            );
        }
        assert_eq!(
            metadata_arguments(&["build", "-F=", "--manifest-path", "tool/Cargo.toml"]),
            arguments(&["--manifest-path", "tool/Cargo.toml"])
        );
    }

    #[test]
    fn features_of_selected_packages_take_every_feature_online_and_are_refused_offline() {
        assert_eq!(
            metadata_arguments(&["build", "-p", "tool", "-Fextra", "--locked"]),
            arguments(&["--locked", "--all-features"])
        );
        assert_eq!(
            metadata_arguments(&["test", "--workspace", "--all-features", "--locked"]),
            arguments(&["--locked", "--all-features"])
        );
        assert!(refusal(&["build", "-p", "tool", "-F", "small", "--offline"]).contains("offline"));
        assert!(
            refusal(&["--frozen", "build", "--workspace", "--no-default-features"])
                .contains("offline")
        );
    }

    #[test]
    fn ordinary_invocations_are_read() {
        for words in [
            &["build", "-p", "moria-render", "--profile", "release", "-q"][..],
            &["check", "--workspace", "--all-targets", "--locked"],
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[
                "clippy",
                "-p",
                "moria",
                "--all-targets",
                "--no-default-features",
                "--locked",
            ],
            &[
                "test",
                "-p",
                "crate_name",
                "test_name",
                "--locked",
                "--",
                "--nocapture",
            ],
            &["test", "--workspace", "--doc", "--all-features", "--locked"],
            &["test", "--test", "name", "--no-run", "-j", "4"],
            &[
                "run",
                "--locked",
                "-p",
                "moria-shader-check",
                "--",
                "--all",
                "--deny-warnings",
            ],
            &[
                "run",
                "--locked",
                "--bin",
                "binary_name",
                "--",
                "-Zanything",
                "--config=x",
            ],
            &["-v", "build", "--config", "profile.dev.opt-level=1"],
            &[
                "--color",
                "always",
                "build",
                "--color=never",
                "--config=build.jobs=2",
            ],
            &["--version"],
        ] {
            select(Path::new("/"), &arguments(words))
                .unwrap_or_else(|error| panic!("{words:?}: {error:#}"));
        }
    }

    #[test]
    fn options_whose_meaning_cannot_be_read_exactly_are_refused() {
        for words in [
            &["--", "build"][..],
            &["-C", "elsewhere", "build"],
            &["-Zunstable-options", "build"],
            &["-vZ", "unstable-options", "build"],
            &["--offline", "--manifest-path", "x/Cargo.toml", "build"],
            &["build", "-Zunstable-options"],
            &["build", "-Z", "build-std"],
            &["build", "-qm", "tool/Cargo.toml"],
            &["build", "-vFextra"],
            &["build", "--config", "build.target-dir='src'"],
            &["build", "--config=alias.ci='build'"],
            &["--config", "net.offline=true", "build"],
            &["build", "--config", "extra.toml"],
            &["build", "--manifest-path", "a/Cargo.toml", "-mb/Cargo.toml"],
        ] {
            refusal(words);
        }
    }

    #[test]
    fn a_target_dir_option_is_kept() {
        let selection =
            select(Path::new("/"), &arguments(&["build", "--target-dir=out"])).expect("selection");
        assert_eq!(selection.target_dir.as_deref(), Some("out".as_ref()));
    }
}
