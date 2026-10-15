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

/// Options without a value that the cached subcommands accept, besides the
/// ones read individually. Anything else after the subcommand declines: an
/// unknown option could select a lockfile, project or compiler unseen.
const KNOWN_FLAGS: [&str; 22] = [
    "all-targets",
    "benches",
    "bins",
    "doc",
    "document-private-items",
    "examples",
    "future-incompat-report",
    "keep-going",
    "lib",
    "no-deps",
    "no-fail-fast",
    "no-run",
    "open",
    "quiet",
    "release",
    "tests",
    "timings",
    "verbose",
    "frozen",
    "locked",
    "offline",
    "workspace",
];

/// Short flags that take no value.
const FLAG_SHORTS: &str = "vqrhV";

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
    /// The subcommand, as given.
    pub command: Option<String>,
    /// Where the subcommand sits among the arguments.
    pub command_index: Option<usize>,
    /// `--help`, `-h`, `--version`, `-V` or `--list` was given: Cargo answers
    /// without building anything.
    pub informational: bool,
}

impl Selection {
    /// Whether the invocation builds or runs the selected project, the only
    /// commands cargo-reapi caches. Everything else, and `--help` and
    /// `--version`, is Cargo's alone.
    pub fn is_cacheable(&self) -> bool {
        !self.informational
            && self
                .command
                .as_deref()
                .is_some_and(|command| CACHEABLE_COMMANDS.contains(&command))
    }
}

/// The commands whose builds cargo-reapi caches, with Cargo's built-in
/// aliases for them.
const CACHEABLE_COMMANDS: [&str; 12] = [
    "bench", "build", "b", "check", "c", "clippy", "doc", "d", "run", "r", "test", "t",
];

/// Reads what `cargo_args`, run in `cwd`, select, or explains why
/// cargo-reapi cannot tell exactly.
pub fn select(cwd: &Path, cargo_args: &[OsString]) -> Result<Selection> {
    let mut reader = Reader::new(cwd);
    let mut remaining = cargo_args.iter().peekable();

    let Some(command) = reader.global_options(&mut remaining)? else {
        return reader.finish();
    };
    refuse_user_alias(cwd, &command)?;
    let cacheable = CACHEABLE_COMMANDS.contains(&command.as_str());
    reader.command = Some(command);
    reader.command_index = Some(cargo_args.len() - remaining.len() - 1);
    if !cacheable {
        // Nothing else about an invocation that is never cached matters.
        return reader.finish();
    }

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

/// `--all-features` and `--no-default-features`.
#[derive(Default)]
struct FeatureFlags {
    all: bool,
    no_default: bool,
}

struct Reader<'a> {
    cwd: &'a Path,
    manifest_path: Option<OsString>,
    /// `--locked`, `--frozen` and `--offline`, each once however often
    /// given, since `cargo metadata` refuses a repeated flag.
    lockfile_policy: BTreeSet<&'static str>,
    features: Vec<OsString>,
    features_flags: FeatureFlags,
    selects_packages: bool,
    target_dir: Option<PathBuf>,
    command: Option<String>,
    command_index: Option<usize>,
    informational: bool,
}

impl<'a> Reader<'a> {
    fn new(cwd: &'a Path) -> Self {
        Self {
            cwd,
            manifest_path: None,
            lockfile_policy: BTreeSet::new(),
            features: Vec::new(),
            features_flags: FeatureFlags::default(),
            selects_packages: false,
            target_dir: None,
            command: None,
            command_index: None,
            informational: false,
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
                self.informational |= shorts.contains(['V', 'h']);
            } else {
                return Ok(Some(text.to_owned()));
            }
        }
        Ok(None)
    }

    fn global_long(&mut self, long: &str, remaining: &mut Remaining<'_>) -> Result<()> {
        let (name, attached) = split_long(long);
        match name {
            "verbose" | "quiet" => {}
            "version" | "help" | "list" => self.informational = true,
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
            "help" | "version" => self.informational = true,
            "ignore-rust-version" => bail!(
                "cargo-reapi does not cache `--ignore-rust-version`, which changes how Cargo \
                 selects dependencies"
            ),
            "manifest-path" => self.manifest_path(attached_or_next(remaining))?,
            "features" => self.features.extend(attached_or_next(remaining)),
            "all-features" => self.features_flags.all = true,
            "no-default-features" => self.features_flags.no_default = true,
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
            "profile" | "target" | "jobs" | "message-format" | "color" => {
                attached_or_next(remaining);
            }
            "bin" | "example" | "test" | "bench" => {
                if attached.is_none() {
                    remaining.next_if(|next| !is_option(next));
                }
            }
            _ if KNOWN_FLAGS.contains(&name) => {}
            _ => bail!(
                "cargo-reapi does not know Cargo's `--{name}` for a cached invocation, so it \
                 cannot tell what it selects"
            ),
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
            if let Some(unknown) = shorts
                .chars()
                .find(|short| !FLAG_SHORTS.contains(*short) && !VALUE_SHORTS.contains(short))
            {
                bail!(
                    "cargo-reapi does not know Cargo's `-{unknown}` for a cached invocation, so \
                     it cannot tell what it selects"
                );
            }
            self.informational |= shorts.chars().all(|short| FLAG_SHORTS.contains(short))
                && shorts.contains(['V', 'h']);
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
        // Preparing the project must never write the lockfile: with
        // `--locked`, a missing or outdated one fails metadata instead.
        // `--frozen` implies it.
        let mut policy = self.lockfile_policy.clone();
        if !policy.contains("--frozen") {
            policy.insert("--locked");
        }
        arguments.extend(policy.iter().map(OsString::from));
        let offline = ["--frozen", "--offline"]
            .iter()
            .any(|flag| self.lockfile_policy.contains(flag));

        let features: Vec<OsString> = self
            .features
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect();
        let selects_features =
            !features.is_empty() || self.features_flags.all || self.features_flags.no_default;
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
            if self.features_flags.all {
                arguments.push(OsString::from("--all-features"));
            }
            if self.features_flags.no_default {
                arguments.push(OsString::from("--no-default-features"));
            }
        }

        Ok(Selection {
            metadata_arguments: arguments,
            target_dir: self.target_dir,
            command: self.command,
            command_index: self.command_index,
            informational: self.informational,
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
        let manifest = arguments(&["--manifest-path", "tool/Cargo.toml", "--locked"]);
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
        assert_eq!(
            metadata_arguments(&["build", "-p", "m"]),
            arguments(&["--locked"])
        );
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
            arguments(&["--manifest-path", "tool/Cargo.toml", "--locked"])
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
            &["build", "--lockfile-path", "alt/Cargo.lock"],
            &["build", "--no-such-option"],
            &["build", "-x"],
            &["test", "-qx"],
        ] {
            refusal(words);
        }
    }

    #[test]
    fn metadata_never_writes_the_lockfile() {
        assert_eq!(metadata_arguments(&["build"]), arguments(&["--locked"]));
        assert_eq!(
            metadata_arguments(&["build", "--frozen"]),
            arguments(&["--frozen"])
        );
    }

    #[test]
    fn only_builds_that_are_not_asking_for_help_are_cacheable() {
        let cacheable = |words: &[&str]| {
            select(Path::new("/"), &arguments(words))
                .unwrap_or_else(|error| panic!("{words:?}: {error:#}"))
                .is_cacheable()
        };
        for words in [
            &["build"][..],
            &["check", "--workspace"],
            &["test", "--", "--help"],
            &["--locked", "run", "--", "--version"],
            &["doc"],
            &["bench"],
        ] {
            assert!(cacheable(words), "{words:?}");
        }
        for words in [
            &["build", "--help"][..],
            &["build", "-h"],
            &["--version"],
            &["-V"],
            &["--list"],
            &["metadata", "--no-deps"],
            &["fetch"],
            &["install", "--path", "../tool"],
            &["help", "build"],
            &["update"],
        ] {
            assert!(!cacheable(words), "{words:?}");
        }
    }

    #[test]
    fn ignoring_the_rust_version_changes_dependency_selection_and_is_refused() {
        assert!(refusal(&["build", "--ignore-rust-version"]).contains("--ignore-rust-version"));
    }

    #[test]
    fn the_subcommand_is_located_among_the_arguments() {
        for (words, index) in [
            (&["run"][..], 0),
            (&["--locked", "run", "-p", "x"], 1),
            (&["-v", "--color", "always", "build"], 3),
        ] {
            let selection = select(Path::new("/"), &arguments(words)).expect("selection");
            assert_eq!(selection.command_index, Some(index), "{words:?}");
        }
    }

    #[test]
    fn a_target_dir_option_is_kept() {
        let selection =
            select(Path::new("/"), &arguments(&["build", "--target-dir=out"])).expect("selection");
        assert_eq!(selection.target_dir.as_deref(), Some("out".as_ref()));
    }
}
