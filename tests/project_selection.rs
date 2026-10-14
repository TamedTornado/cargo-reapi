//! cargo-reapi caches only what it can prove safe: it prepares the project
//! a Cargo invocation selects before the strict sandbox starts, and runs
//! the user's exact command as plain Cargo, uncached, whenever it cannot.
//! These tests drive real Cargo through the gated cache backend.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tempfile::tempdir;

/// A user alias whose expansion forbids writing a lockfile.
const LOCKED_ALIAS: &str = "[alias]\nci = ['build', '--locked']\n";

struct Fixture {
    root: tempfile::TempDir,
    cargo_home: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir().expect("fixture");
        let cargo_home = root.path().join("cargo-home");
        let cache = root.path().join("cache");
        fs::create_dir_all(&cargo_home).expect("private CARGO_HOME");
        Self {
            root,
            cargo_home,
            cache,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    /// Writes `root`'s lockfile, which a cached invocation needs: preparing
    /// the project never creates one.
    fn lock(&self, root: &Path) {
        let status = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(root)
            .env("CARGO_HOME", &self.cargo_home)
            .status()
            .expect("cargo generate-lockfile");
        assert!(
            status.success(),
            "generate-lockfile failed in {}",
            root.display()
        );
    }

    /// Runs the gated cache backend in `cwd`, letting Cargo choose the
    /// target directory.
    fn cargo_reapi(&self, cwd: &Path, cargo_args: &[&str]) -> Output {
        self.cargo_reapi_with(cwd, cargo_args, &[])
    }

    /// As [`Self::cargo_reapi`], with extra environment.
    fn cargo_reapi_with(
        &self,
        cwd: &Path,
        cargo_args: &[&str],
        environment: &[(&str, &OsStr)],
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
        command
            .current_dir(cwd)
            .env("CARGO_HOME", &self.cargo_home)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_REAPI_ACTION_LOG")
            .env_remove("CARGO_REAPI_ACTION_CACHE_TEST_MODE")
            .args(["--backend", "cache", "--cache-dir"])
            .arg(&self.cache)
            .arg("--")
            .args(cargo_args);
        for (name, value) in environment {
            command.env(name, value);
        }
        command.output().expect("run cargo-reapi")
    }
}

/// A binary package printing `message`, with `extra` manifest text.
fn write_package(root: &Path, name: &str, message: &str, extra: &str) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname='{name}'\nversion='0.0.0'\nedition='2024'\n{extra}"),
    )
    .expect("manifest");
    write_main(root, message);
}

fn write_main(root: &Path, message: &str) {
    fs::write(
        root.join("src/main.rs"),
        format!("fn main() {{ println!(\"{message}\"); }}\n"),
    )
    .expect("main source");
}

/// Writes `.cargo/<name>` beneath `directory`.
fn write_config(directory: &Path, name: &str, text: &str) {
    fs::create_dir_all(directory.join(".cargo")).expect("configuration directory");
    fs::write(directory.join(".cargo").join(name), text).expect("configuration");
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The line cargo-reapi prints when it runs Cargo uncached.
const DECLINE: &str = "cargo-reapi: not caching (";

/// The invocation succeeded and cargo-reapi cached it: it did not decline.
fn assert_succeeded(output: &Output) {
    assert!(output.status.success(), "{}", stderr(output));
    assert!(!stderr(output).contains(DECLINE), "{}", stderr(output));
}

/// cargo-reapi declined for `reason` and ran Cargo uncached, which printed
/// exactly one decline line. Cargo's own status is the caller's to check.
fn assert_declined(output: &Output, reason: &str) {
    let text = stderr(output);
    let lines = text
        .lines()
        .filter(|line| line.contains(DECLINE))
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(lines[0].contains(reason), "{text}");
}

/// How many times the gate hit in `target`'s action log.
fn gate_hits(target: &Path) -> usize {
    let Ok(log) = fs::read_to_string(target.join("cargo-reapi/actions.jsonl")) else {
        return 0;
    };
    log.lines()
        .filter(|line| line.contains("gate-snapshot-hit") || line.contains("coalesced-gate-hit"))
        .count()
}

/// Whether the cache holds any published gate snapshot.
fn cache_holds_a_snapshot(fixture: &Fixture) -> bool {
    fs::read_dir(fixture.cache.join("gate-snapshots/objects"))
        .is_ok_and(|mut entries| entries.next().is_some())
}

fn run(binary: &Path) -> String {
    let output = Command::new(binary)
        .output()
        .unwrap_or_else(|error| panic!("run {}: {error}", binary.display()));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Whether the active Cargo has the `-m` short form of `--manifest-path`.
fn cargo_has_short_manifest_option() -> bool {
    let help = Command::new("cargo")
        .args(["build", "--help"])
        .output()
        .expect("cargo build --help");
    String::from_utf8_lossy(&help.stdout).contains("-m, --manifest-path")
}

/// Runs `cargo_args` in `project`, edits `source`, and runs them again: both
/// runs must decline the target and run Cargo uncached, and the edit must
/// survive and be built.
fn assert_target_declined_around_an_edit(
    fixture: &Fixture,
    project: &Path,
    source: &Path,
    cargo_args: &[&str],
) {
    fixture.lock(project);
    let first = fixture.cargo_reapi(project, cargo_args);
    fs::write(source, "fn main() { println!(\"edited\"); }\n").expect("edit source");
    let second = fixture.cargo_reapi(project, cargo_args);

    let text = fs::read_to_string(source).expect("source");
    assert!(text.contains("edited"), "a gate restored over the edit");
    for output in [&first, &second] {
        assert!(output.status.success(), "{}", stderr(output));
        assert_declined(output, "Cargo's target directory");
    }
    assert!(
        !cache_holds_a_snapshot(fixture),
        "a declined build was cached"
    );
}

#[test]
fn a_target_directory_holding_sources_runs_uncached_and_never_restores_over_them() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "original", "");
    write_config(&project, "config.toml", "[build]\ntarget-dir = '.'\n");

    assert_target_declined_around_an_edit(
        &fixture,
        &project,
        &project.join("src/main.rs"),
        &["build"],
    );
}

#[cfg(unix)]
#[test]
fn a_target_directory_at_inside_or_linked_to_a_source_directory_runs_uncached() {
    for target in ["src", "src/out/new", "link"] {
        let fixture = Fixture::new();
        let project = fixture.path("project");
        write_package(&project, "project", "original", "");
        std::os::unix::fs::symlink("src", project.join("link")).expect("link to src");
        write_config(
            &project,
            "config.toml",
            &format!("[build]\ntarget-dir = '{target}'\n"),
        );

        assert_target_declined_around_an_edit(
            &fixture,
            &project,
            &project.join("src/main.rs"),
            &["build"],
        );
    }

    // The same targets given on the command line.
    for target in ["src", "src/out/new", "link"] {
        let fixture = Fixture::new();
        let project = fixture.path("project");
        write_package(&project, "project", "original", "");
        std::os::unix::fs::symlink("src", project.join("link")).expect("link to src");

        assert_target_declined_around_an_edit(
            &fixture,
            &project,
            &project.join("src/main.rs"),
            &["build", "--target-dir", target],
        );
    }
}

/// Astra 0.2.11 finding 1: a target in a data directory that no target's
/// source path names, such as one read by `include_str!`.
#[test]
fn a_target_directory_that_is_an_include_str_data_directory_never_overwrites_it() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "unused", "");
    fs::create_dir_all(project.join("data")).expect("data directory");
    let message = project.join("data/message.txt");
    fs::write(&message, "original").expect("message");
    fs::write(
        project.join("src/main.rs"),
        "fn main() { println!(\"{}\", include_str!(\"../data/message.txt\")); }\n",
    )
    .expect("main source");
    write_config(&project, "config.toml", "[build]\ntarget-dir = 'data'\n");
    fixture.lock(&project);

    let first = fixture.cargo_reapi(&project, &["build"]);
    fs::write(&message, "edited").expect("edit message");
    let second = fixture.cargo_reapi(&project, &["build"]);

    for output in [&first, &second] {
        assert!(output.status.success(), "{}", stderr(output));
        assert_declined(output, "Cargo's target directory");
    }
    assert_eq!(fs::read_to_string(&message).expect("message"), "edited");
    assert_eq!(run(&project.join("data/debug/project")), "edited");
    assert!(!cache_holds_a_snapshot(&fixture));
}

#[test]
fn a_path_dependency_outside_the_workspace_is_never_the_target() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    let dependency = fixture.path("dependency");
    write_package(
        &project,
        "project",
        "project",
        "\n[dependencies]\ndependency={path='../dependency'}\n",
    );
    fs::create_dir_all(dependency.join("src")).expect("dependency source directory");
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname='dependency'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("dependency manifest");
    let library = dependency.join("src/lib.rs");
    fs::write(&library, "pub const WORD: &str = \"original\";\n").expect("dependency source");
    write_config(
        &project,
        "config.toml",
        "[build]\ntarget-dir = '../dependency'\n",
    );
    fixture.lock(&project);

    let first = fixture.cargo_reapi(&project, &["build"]);
    fs::write(&library, "pub const WORD: &str = \"edited\";\n").expect("edit dependency");
    let second = fixture.cargo_reapi(&project, &["build"]);

    for output in [&first, &second] {
        assert_declined(output, "Cargo's target directory");
    }
    assert!(
        fs::read_to_string(&library)
            .expect("library")
            .contains("edited")
    );
}

/// Cargo's own default, `<workspace root>/target`, is the one target
/// inside the project that is cached.
#[test]
fn cargos_default_target_directory_is_cached() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "default", "");
    fixture.lock(&project);

    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));

    assert_eq!(gate_hits(&project.join("target")), 1);
}

#[cfg(unix)]
#[test]
fn a_package_selected_through_a_symlink_is_hashed_by_its_real_directory() {
    let fixture = Fixture::new();
    let outer = fixture.path("outer");
    let real = fixture.path("real");
    write_package(&outer, "outer", "outer", "");
    write_package(&real, "real", "first", "\n[workspace]\n");
    fixture.lock(&real);
    std::os::unix::fs::symlink("../real", outer.join("link")).expect("link to real");
    let arguments = ["build", "--manifest-path", "link/Cargo.toml"];

    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));
    assert_eq!(run(&real.join("target/debug/real")), "first");
    write_main(&real, "second");
    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));

    assert_eq!(run(&real.join("target/debug/real")), "second");
}

#[cfg(unix)]
#[test]
fn a_path_dependency_reached_through_a_symlink_is_hashed_by_its_real_directory() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    let real = fixture.path("real");
    write_package(
        &project,
        "project",
        "unused",
        "\n[dependencies]\nword={path='link'}\n",
    );
    fs::write(
        project.join("src/main.rs"),
        "fn main() { println!(\"{}\", word::WORD); }\n",
    )
    .expect("main source");
    fs::create_dir_all(real.join("src")).expect("dependency source directory");
    fs::write(
        real.join("Cargo.toml"),
        "[package]\nname='word'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("dependency manifest");
    let write_word = |word: &str| {
        fs::write(
            real.join("src/lib.rs"),
            format!("pub const WORD: &str = \"{word}\";\n"),
        )
        .expect("dependency source");
    };
    write_word("first");
    std::os::unix::fs::symlink("../real", project.join("link")).expect("link to real");
    fixture.lock(&project);

    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert_eq!(run(&project.join("target/debug/project")), "first");
    write_word("second");
    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));

    assert_eq!(run(&project.join("target/debug/project")), "second");
}

#[test]
fn a_selected_package_beneath_a_directory_named_target_is_hashed() {
    let fixture = Fixture::new();
    let outer = fixture.path("outer");
    write_package(
        &outer,
        "outer",
        "outer",
        "\n[workspace]\nexclude=['target']\n",
    );
    let tool = outer.join("target/tool");
    write_package(&tool, "tool", "first", "\n[workspace]\n");
    fixture.lock(&tool);
    let arguments = ["build", "--manifest-path", "target/tool/Cargo.toml"];

    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));
    assert_eq!(run(&tool.join("target/debug/tool")), "first");
    write_main(&tool, "second");
    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));

    assert_eq!(run(&tool.join("target/debug/tool")), "second");
}

/// Astra 0.2.11 finding 3: a source directory named `target` is ordinary
/// source, even beside an external output directory.
#[test]
fn a_source_directory_named_target_is_hashed() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    let output = fixture.path("output");
    write_package(&project, "project", "unused", "");
    fs::create_dir_all(project.join("src/target")).expect("source target directory");
    let message = project.join("src/target/message.txt");
    fs::write(&message, "original").expect("message");
    fs::write(
        project.join("src/main.rs"),
        "fn main() { println!(\"{}\", include_str!(\"target/message.txt\")); }\n",
    )
    .expect("main source");
    fixture.lock(&project);
    let environment = [("CARGO_TARGET_DIR", output.as_os_str())];

    assert_succeeded(&fixture.cargo_reapi_with(&project, &["build"], &environment));
    assert_eq!(run(&output.join("debug/project")), "original");
    fs::write(&message, "edited").expect("edit message");
    assert_succeeded(&fixture.cargo_reapi_with(&project, &["build"], &environment));

    assert_eq!(run(&output.join("debug/project")), "edited");
}

/// Astra 0.2.11 finding 4: another Cargo run in the same target directory
/// replaces outputs without touching the gate's marker.
#[test]
fn a_gate_hit_restores_outputs_that_another_cargo_run_replaced() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "unused", "\n[features]\nextra = []\n");
    fs::write(
        project.join("src/main.rs"),
        "fn main() { println!(\"{}\", if cfg!(feature = \"extra\") { \"extra\" } else { \"plain\" }); }\n",
    )
    .expect("main source");
    fixture.lock(&project);
    let target = project.join("target");
    let binary = target.join("debug/project");

    // Cold, then a restore that writes the marker, then a marker hit.
    for _ in 0..3 {
        assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
        assert_eq!(run(&binary), "plain");
    }
    assert_eq!(gate_hits(&target), 2);

    // A plain build with the feature replaces the executable.
    let plain = Command::new("cargo")
        .args(["build", "--features", "extra"])
        .current_dir(&project)
        .env("CARGO_HOME", &fixture.cargo_home)
        .output()
        .expect("plain cargo build");
    assert!(plain.status.success(), "{}", stderr(&plain));
    assert_eq!(run(&binary), "extra");

    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert_eq!(run(&binary), "plain");

    // A deleted output is restored the same way.
    fs::remove_file(&binary).expect("delete the executable");
    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert_eq!(run(&binary), "plain");
}

/// Defines the `ci` alias somewhere a Cargo run in `project` reads it, and
/// returns the environment that run needs.
type AliasDefinition = fn(&Fixture, &Path) -> Vec<(&'static str, PathBuf)>;

#[test]
fn user_aliases_run_as_plain_cargo_wherever_they_are_defined() {
    let cases: [(&str, AliasDefinition); 5] = [
        ("project configuration", |_, project| {
            write_config(project, "config.toml", LOCKED_ALIAS);
            Vec::new()
        }),
        ("legacy file beside config.toml", |_, project| {
            write_config(project, "config", LOCKED_ALIAS);
            write_config(project, "config.toml", "[build]\njobs = 2\n");
            Vec::new()
        }),
        ("CARGO_HOME", |fixture, _| {
            fs::write(fixture.cargo_home.join("config.toml"), LOCKED_ALIAS).expect("home config");
            Vec::new()
        }),
        ("HOME/.cargo for an empty CARGO_HOME", |fixture, _| {
            let home = fixture.path("home");
            write_config(&home, "config.toml", LOCKED_ALIAS);
            vec![
                ("HOME", home),
                ("CARGO_HOME", PathBuf::new()),
                ("RUSTUP_HOME", rustup_home()),
            ]
        }),
        ("the environment", |_, _| {
            vec![("CARGO_ALIAS_CI", PathBuf::from("build --locked"))]
        }),
    ];

    for (origin, define) in cases {
        let fixture = Fixture::new();
        let project = fixture.path("project");
        write_package(&project, "project", "project", "");
        let environment = define(&fixture, &project);
        let environment = environment
            .iter()
            .map(|(name, value)| (*name, value.as_os_str()))
            .collect::<Vec<_>>();

        // Cargo itself runs the alias, whose `--locked` forbids writing the
        // missing lockfile.
        let output = fixture.cargo_reapi_with(&project, &["ci"], &environment);

        assert!(!output.status.success(), "{origin}: {}", stderr(&output));
        assert_declined(&output, "user-defined Cargo alias");
        assert!(!project.join("Cargo.lock").exists(), "{origin}");
        assert!(!cache_holds_a_snapshot(&fixture), "{origin}");
    }
}

/// Astra 0.2.11 finding 8: a valid alias works in the transparent modes.
#[test]
fn an_alias_works_without_caching_and_in_the_local_backend() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");
    write_config(&project, "config.toml", LOCKED_ALIAS);
    fixture.lock(&project);

    let uncached = fixture.cargo_reapi(&project, &["ci"]);
    assert!(uncached.status.success(), "{}", stderr(&uncached));
    assert_declined(&uncached, "user-defined Cargo alias");
    assert_eq!(run(&project.join("target/debug/project")), "project");

    fs::remove_dir_all(project.join("target")).expect("clean");
    for arguments in [
        &["--backend", "local", "--", "ci"][..],
        &[
            "--backend",
            "cache",
            "--snapshot-policy",
            "off",
            "--cache-dir",
        ],
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
        command
            .current_dir(&project)
            .env("CARGO_HOME", &fixture.cargo_home)
            .env_remove("CARGO_TARGET_DIR")
            .args(arguments);
        if arguments.len() == 5 {
            command.arg(&fixture.cache).args(["--", "ci"]);
        }
        let output = command.output().expect("run cargo-reapi");
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains(DECLINE));
    }
}

/// The toolchains rustup manages, for a test that moves `HOME`.
fn rustup_home() -> PathBuf {
    std::env::var_os("RUSTUP_HOME").map_or_else(
        || PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".rustup"),
        PathBuf::from,
    )
}

#[test]
fn configuration_that_includes_more_is_never_cached() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");
    write_config(&project, "config.toml", "include = ['more.toml']\n");
    write_config(&project, "more.toml", "");
    fixture.lock(&project);

    // It could set an alias or a lockfile path that cargo-reapi cannot see.
    for arguments in [&["build"][..], &["b"]] {
        let output = fixture.cargo_reapi(&project, arguments);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_declined(&output, "includes further configuration");
    }
    assert!(!cache_holds_a_snapshot(&fixture));
}

#[test]
fn options_that_could_change_the_selection_unseen_run_as_plain_cargo() {
    let not_accepted = "does not accept";
    let before_subcommand = "before the subcommand exactly";
    for (arguments, reason) in [
        (&["--config", "alias.ci='build'", "ci"][..], not_accepted),
        (
            &["build", "--config", "build.target-dir='src'"],
            not_accepted,
        ),
        (&["--", "build"], before_subcommand),
        (&["build", "-Zunstable-options"], "unstable -Z flags"),
        (&["-vZ", "unstable-options", "build"], before_subcommand),
        (&["build", "--ignore-rust-version"], "--ignore-rust-version"),
    ] {
        let fixture = Fixture::new();
        let project = fixture.path("project");
        write_package(&project, "project", "project", "");
        fixture.lock(&project);

        let output = fixture.cargo_reapi(&project, arguments);

        assert_declined(&output, reason);
        assert!(!cache_holds_a_snapshot(&fixture), "{arguments:?}");
    }

    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");
    fixture.lock(&project);
    let harmless = ["build", "--config", "profile.dev.debug=0"];
    assert_succeeded(&fixture.cargo_reapi(&project, &harmless));
}

/// Astra 0.2.11 finding 5: with the lockfile present, `--ignore-rust-version`
/// is never cached, so the wrapper cannot pick a dependency for Cargo.
#[test]
fn ignoring_the_rust_version_is_never_cached() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");
    fixture.lock(&project);

    let first = fixture.cargo_reapi(&project, &["build", "--ignore-rust-version"]);
    let second = fixture.cargo_reapi(&project, &["build", "--ignore-rust-version"]);

    for output in [&first, &second] {
        assert!(output.status.success(), "{}", stderr(output));
        assert_declined(output, "--ignore-rust-version");
    }
    assert!(!cache_holds_a_snapshot(&fixture));
    assert_eq!(run(&project.join("target/debug/project")), "project");
}

/// Astra 0.2.11 finding 2: an alternate lockfile is neither hashed nor
/// protected, so it is declined, from the environment or configuration.
#[test]
fn an_alternate_lockfile_is_never_cached() {
    for source in ["environment", "configuration"] {
        let fixture = Fixture::new();
        let project = fixture.path("project");
        let elsewhere = fixture.path("elsewhere/Cargo.lock");
        write_package(&project, "project", "project", "");
        fixture.lock(&project);
        let mut environment = Vec::new();
        let reason = if source == "environment" {
            environment.push(("CARGO_RESOLVER_LOCKFILE_PATH", elsewhere.as_os_str()));
            "CARGO_RESOLVER_LOCKFILE_PATH"
        } else {
            write_config(
                &project,
                "config.toml",
                &format!("[resolver]\nlockfile-path = '{}'\n", elsewhere.display()),
            );
            "resolver.lockfile-path"
        };

        let first = fixture.cargo_reapi_with(&project, &["build"], &environment);
        let second = fixture.cargo_reapi_with(&project, &["build"], &environment);

        for output in [&first, &second] {
            assert_declined(output, reason);
        }
        assert!(!cache_holds_a_snapshot(&fixture), "{source}");
    }
}

/// Astra 0.2.11 finding 6: commands that do not build, and requests for
/// help or the version, are Cargo's alone, before any metadata run.
#[test]
fn commands_that_do_not_build_are_never_prepared_or_cached() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    let tool = fixture.path("tool");
    write_package(&project, "project", "project", "");
    write_package(&tool, "tool", "tool", "");

    for arguments in [
        &["build", "--help"][..],
        &["--version"],
        &["metadata", "--no-deps"],
        &["install", "--path", "../tool", "--root", "../installed"],
    ] {
        let output = fixture.cargo_reapi(&project, arguments);

        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            stderr(&output)
        );
        assert_declined(
            &output,
            "only build, check, clippy, test, run, bench and doc",
        );
        assert!(!project.join("Cargo.lock").exists(), "{arguments:?}");
        assert!(!project.join("target").exists(), "{arguments:?}");
        assert!(!cache_holds_a_snapshot(&fixture), "{arguments:?}");
    }
    assert!(
        fixture.path("installed/bin/tool").exists(),
        "installed the tool"
    );

    // Outside any project, even `--version` answers.
    let nowhere = fixture.path("nowhere");
    fs::create_dir_all(&nowhere).expect("empty directory");
    let output = fixture.cargo_reapi(&nowhere, &["--version"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("cargo "));

    // With a lockfile there is a project to prepare, which still never
    // happens for these.
    fixture.lock(&project);
    let output = fixture.cargo_reapi(&project, &["build", "--help"]);
    assert_declined(&output, "--help");
    assert!(!project.join("target").exists());
}

/// Astra 0.2.11 finding 7: preflight that cannot complete hands the command
/// to Cargo, which does not need what preflight wanted.
#[test]
fn offline_builds_do_not_need_unrelated_dependency_sources() {
    let fixture = Fixture::new();
    write_local_registry(&fixture.path("registry"), &fixture.cargo_home);
    let project = fixture.path("project");
    write_package(
        &project,
        "project",
        "unused",
        "\n[dev-dependencies]\nopt = { version = '0.1', registry = 'local' }\n",
    );
    fs::write(project.join("src/lib.rs"), "pub const X: u8 = 1;\n").expect("library");
    fs::remove_file(project.join("src/main.rs")).expect("no binary");
    fixture.lock(&project);
    fs::remove_dir_all(fixture.path("registry/dl")).expect("no sources to download");

    let output = fixture.cargo_reapi(&project, &["build", "--lib", "--offline"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_declined(&output, "cargo metadata failed");
    assert!(project.join("target/debug/libproject.rlib").exists());
}

#[test]
fn a_missing_or_outdated_lockfile_is_never_written_by_preparation() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");

    // Plain Cargo may write the lockfile; cargo-reapi's preparation may not.
    let output = fixture.cargo_reapi(&project, &["build", "--locked"]);
    assert!(!output.status.success());
    assert_declined(&output, "cargo metadata failed");
    assert!(!project.join("Cargo.lock").exists());

    fixture.lock(&project);
    let lock = fs::read(project.join("Cargo.lock")).expect("lockfile");
    fs::write(
        project.join("Cargo.toml"),
        "[package]\nname='project'\nversion='0.1.0'\nedition='2024'\n",
    )
    .expect("changed manifest");
    let output = fixture.cargo_reapi(&project, &["build", "--locked"]);
    assert_declined(&output, "cargo metadata failed");
    assert!(!cache_holds_a_snapshot(&fixture));
    assert_ne!(fs::read(project.join("Cargo.lock")).expect("lockfile"), b"");
    let _ = lock;
}

#[test]
fn an_explicitly_empty_feature_value_keeps_the_lockfile_policy() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");

    // Plain Cargo refuses: `--locked` forbids writing the missing lockfile.
    let output = fixture.cargo_reapi(&project, &["build", "-F=", "--locked", "--offline"]);

    assert!(
        !output.status.success(),
        "a --locked build wrote a lockfile"
    );
    assert!(!project.join("Cargo.lock").exists());

    // With the lockfile in place plain Cargo accepts the same invocation.
    fixture.lock(&project);
    assert_succeeded(&fixture.cargo_reapi(&project, &["build", "-F=", "--locked"]));
}

#[test]
fn a_repeated_lockfile_flag_reaches_metadata_once() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");
    fixture.lock(&project);

    let output = fixture.cargo_reapi(&project, &["--locked", "build", "--locked"]);

    assert_succeeded(&output);
    assert_eq!(run(&project.join("target/debug/project")), "project");
}

#[test]
fn a_sibling_virtual_workspace_is_readable_and_its_manifest_is_hashed() {
    let fixture = Fixture::new();
    let outer = fixture.path("outer");
    let other = fixture.path("other");
    write_package(&outer, "outer", "outer", "");
    let manifest = "[workspace]\nmembers = ['app']\nresolver = '3'\n";
    fs::create_dir_all(&other).expect("sibling workspace");
    fs::write(other.join("Cargo.toml"), manifest).expect("virtual manifest");
    write_package(&other.join("app"), "app", "unused", "");
    fs::write(
        other.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", cfg!(debug_assertions)); }\n",
    )
    .expect("app source");
    fixture.lock(&other);
    let arguments = ["build", "--manifest-path=../other/Cargo.toml"];

    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));
    assert_eq!(run(&other.join("target/debug/app")), "true");
    let profile = "\n[profile.dev]\ndebug-assertions = false\n";
    fs::write(other.join("Cargo.toml"), format!("{manifest}{profile}")).expect("profile");
    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));

    assert_eq!(run(&other.join("target/debug/app")), "false");
}

#[test]
fn selected_features_reach_their_optional_dependencies() {
    let fixture = Fixture::new();
    let outer = fixture.path("outer");
    let extra = fixture.path("extra");
    write_package(
        &outer,
        "outer",
        "outer",
        "\n[workspace]\nexclude=['tool']\n",
    );
    fs::create_dir_all(extra.join("src")).expect("extra source directory");
    fs::write(
        extra.join("Cargo.toml"),
        "[package]\nname='extra'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("extra manifest");
    fs::write(
        extra.join("src/lib.rs"),
        "pub const WORD: &str = \"extra\";\n",
    )
    .expect("extra source");
    let tool = outer.join("tool");
    write_package(
        &tool,
        "tool",
        "unused",
        &format!(
            "\n[dependencies]\nextra={{path='{}',optional=true}}\n\n[workspace]\n",
            extra.display()
        ),
    );
    fs::write(
        tool.join("src/main.rs"),
        "fn main() { println!(\"{}\", extra::WORD); }\n",
    )
    .expect("tool source");
    fixture.lock(&tool);

    for features in [
        &["--features", "extra"][..],
        &["-Fextra"],
        &["-F=extra"],
        &["--all-features"],
        &["-p", "tool", "--features", "extra"],
    ] {
        let mut arguments = vec!["build", "--manifest-path", "tool/Cargo.toml"];
        arguments.extend_from_slice(features);
        assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));
        assert_eq!(run(&tool.join("target/debug/tool")), "extra");
    }
}

/// A Git index and file downloads for one crate, `opt`, registered in
/// `cargo_home` as the registry `local`.
fn write_local_registry(root: &Path, cargo_home: &Path) {
    let crate_root = root.join("crate/opt-0.1.0");
    fs::create_dir_all(crate_root.join("src")).expect("crate source directory");
    fs::write(
        crate_root.join("Cargo.toml"),
        "[package]\nname='opt'\nversion='0.1.0'\nedition='2021'\n",
    )
    .expect("crate manifest");
    fs::write(crate_root.join("src/lib.rs"), "pub const X: u8 = 1;\n").expect("crate source");

    let download = root.join("dl/opt/0.1.0/download");
    fs::create_dir_all(download.parent().expect("download directory")).expect("downloads");
    let archived = Command::new("tar")
        .arg("-C")
        .arg(root.join("crate"))
        .arg("-czf")
        .arg(&download)
        .arg("opt-0.1.0")
        .status()
        .expect("tar");
    assert!(archived.success(), "archiving the crate failed");
    let checksum = format!("{:x}", Sha256::digest(fs::read(&download).expect("crate")));

    let index = root.join("index");
    fs::create_dir_all(index.join("3/o")).expect("index");
    fs::write(
        index.join("config.json"),
        format!(
            "{{\"dl\":\"file://{}/dl/{{crate}}/{{version}}/download\"}}",
            root.display()
        ),
    )
    .expect("index configuration");
    fs::write(
        index.join("3/o/opt"),
        format!(
            "{{\"name\":\"opt\",\"vers\":\"0.1.0\",\"deps\":[],\"cksum\":\"{checksum}\",\
             \"features\":{{}},\"yanked\":false}}\n"
        ),
    )
    .expect("index entry");
    commit_all(&index);

    fs::write(
        cargo_home.join("config.toml"),
        format!("[registries.local]\nindex = 'file://{}'\n", index.display()),
    )
    .expect("registry configuration");
}

fn commit_all(repository: &Path) {
    let identity = ["-c", "user.name=test", "-c", "user.email=test@example.com"];
    for arguments in [
        &["init", "-q"][..],
        &["add", "."],
        &[
            identity[0],
            identity[1],
            identity[2],
            identity[3],
            "commit",
            "-qm",
            "index",
        ],
    ] {
        let status = Command::new("git")
            .args(arguments)
            .current_dir(repository)
            .status()
            .expect("git");
        assert!(status.success(), "git {arguments:?} failed");
    }
}

#[test]
fn an_offline_feature_build_acquires_only_what_its_features_need() {
    let fixture = Fixture::new();
    write_local_registry(&fixture.path("registry"), &fixture.cargo_home);
    let project = fixture.path("project");
    let dependencies =
        "\n[dependencies]\nopt = { version = '0.1', registry = 'local', optional = true }\n";
    write_package(
        &project,
        "project",
        "small",
        &format!("\n[features]\nsmall = []\n{dependencies}"),
    );
    // The lockfile and index are present; the unused optional crate is not.
    fixture.lock(&project);

    let output = fixture.cargo_reapi(&project, &["build", "--offline", "-F", "small"]);

    assert_succeeded(&output);
    assert_eq!(run(&project.join("target/debug/project")), "small");

    // Preparation cannot apply features to a package selection offline, so
    // Cargo runs the command itself.
    let output = fixture.cargo_reapi(
        &project,
        &["build", "--offline", "-p", "project", "-Fsmall"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_declined(&output, "selected with -p");
}

#[test]
fn a_target_directory_inside_the_project_other_than_cargos_default_runs_uncached() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "nested", "");
    fixture.lock(&project);

    let output = fixture.cargo_reapi(&project, &["build", "--target-dir", "artifacts/new"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_declined(&output, "Cargo's target directory");
    assert_eq!(run(&project.join("artifacts/new/debug/project")), "nested");
}

#[test]
fn a_new_nested_external_target_directory_is_created() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    let output = fixture.path("output/nested/new");
    write_package(&project, "project", "nested", "");
    fixture.lock(&project);

    let result = fixture.cargo_reapi(
        &project,
        &["build", "--target-dir", &output.to_string_lossy()],
    );

    assert_succeeded(&result);
    assert_eq!(run(&output.join("debug/project")), "nested");
}

#[test]
fn a_sibling_selected_workspace_stays_readable() {
    let fixture = Fixture::new();
    let outer = fixture.path("outer");
    let other = fixture.path("other");
    write_package(&outer, "outer", "outer", "");
    write_package(&other, "other", "sibling", "");
    fixture.lock(&other);

    let output = fixture.cargo_reapi(&outer, &["build", "--manifest-path=../other/Cargo.toml"]);

    assert_succeeded(&output);
    assert_eq!(run(&other.join("target/debug/other")), "sibling");
}

#[test]
fn the_short_manifest_option_selects_the_project() {
    if !cargo_has_short_manifest_option() {
        eprintln!("skipped: this Cargo has no -m option");
        return;
    }
    let fixture = Fixture::new();
    let outer = fixture.path("outer");
    write_package(
        &outer,
        "outer",
        "outer",
        "\n[workspace]\nexclude=['tool']\n",
    );
    let tool = outer.join("tool");
    write_package(&tool, "tool", "tool", "\n[workspace]\n");
    fixture.lock(&tool);

    for arguments in [
        &["build", "-m", "tool/Cargo.toml"][..],
        &["build", "-mtool/Cargo.toml"],
        &["build", "-m=tool/Cargo.toml"],
    ] {
        fs::remove_dir_all(tool.join("target")).ok();
        assert_succeeded(&fixture.cargo_reapi(&outer, arguments));
        assert_eq!(run(&tool.join("target/debug/tool")), "tool");
    }
    let output = fixture.cargo_reapi(&outer, &["build", "-qm", "tool/Cargo.toml"]);
    assert_declined(&output, "own argument");
}

/// The invocations Moria's `moria_cargo` makes, with `CARGO_TARGET_DIR` at
/// `<checkout>/target`, stay cached: each hits on its second run.
#[test]
fn the_ordinary_moria_cargo_invocations_still_hit() {
    let fixture = Fixture::new();
    let checkout = fixture.path("checkout");
    fs::create_dir_all(&checkout).expect("checkout");
    fs::write(
        checkout.join("Cargo.toml"),
        "[workspace]\nmembers = ['moria', 'crate_name']\nresolver = '3'\n",
    )
    .expect("workspace manifest");
    write_package(
        &checkout.join("crate_name"),
        "crate_name",
        "unused",
        "\n[features]\ndefault = ['extra']\nextra = []\n",
    );
    fs::remove_file(checkout.join("crate_name/src/main.rs")).expect("no binary");
    fs::write(
        checkout.join("crate_name/src/lib.rs"),
        "pub fn answer() -> u8 { 42 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn answer() {\n        assert_eq!(super::answer(), 42);\n    }\n}\n",
    )
    .expect("library");
    fs::create_dir_all(checkout.join("crate_name/tests")).expect("tests");
    fs::write(
        checkout.join("crate_name/tests/name.rs"),
        "#[test]\nfn works() {\n    assert_eq!(crate_name::answer(), 42);\n}\n",
    )
    .expect("integration test");
    write_package(
        &checkout.join("moria"),
        "moria",
        "unused",
        "\n[dependencies]\ncrate_name = { path = '../crate_name' }\n",
    );
    fs::write(
        checkout.join("moria/src/main.rs"),
        "fn main() { println!(\"{}\", crate_name::answer()); }\n",
    )
    .expect("moria source");
    fixture.lock(&checkout);
    let target = checkout.join("target");
    let environment = [("CARGO_TARGET_DIR", target.as_os_str())];

    let invocations: [&[&str]; 10] = [
        &["build", "-p", "moria", "--profile", "release", "-q"],
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
            "answer",
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
            "moria",
            "--",
            "--all",
            "--deny-warnings",
        ],
        &["run", "--locked", "--bin", "moria"],
        &["build", "--all-features"],
    ];
    for arguments in invocations {
        let before = gate_hits(&target);
        for _ in 0..2 {
            let output = fixture.cargo_reapi_with(&checkout, arguments, &environment);
            assert_succeeded(&output);
        }
        assert_eq!(gate_hits(&target), before + 1, "{arguments:?} did not hit");
    }
}

/// A build script reading Git state: the gate keys `.git` except its
/// objects and logs.
#[test]
fn git_state_read_by_a_build_script_is_keyed_but_repacking_objects_is_not() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "unused", "");
    fs::write(
        project.join("build.rs"),
        "fn main() {\n    let head = std::process::Command::new(\"git\").args([\"rev-parse\", \"HEAD\"]).output().expect(\"git\");\n    println!(\"cargo:rustc-env=HEAD={}\", String::from_utf8_lossy(&head.stdout).trim());\n}\n",
    )
    .expect("build script");
    fs::write(
        project.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"HEAD\")); }\n",
    )
    .expect("main source");
    fs::write(project.join(".gitignore"), "target\n").expect("ignore");
    fixture.lock(&project);
    let git = |arguments: &[&str]| {
        let status = Command::new("git")
            .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
            .args(arguments)
            .current_dir(&project)
            .status()
            .expect("git");
        assert!(status.success(), "git {arguments:?} failed");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "first"]);
    let binary = project.join("target/debug/project");
    let head = |project: &Path| {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(project)
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };

    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert_eq!(run(&binary), head(&project));

    // A new commit changes what the build script reads.
    git(&["commit", "-qm", "second", "--allow-empty"]);
    let hits = gate_hits(&project.join("target"));
    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert_eq!(gate_hits(&project.join("target")), hits, "a stale hit");
    assert_eq!(run(&binary), head(&project));

    // Repacking objects changes nothing it can read.
    git(&["repack", "-a", "-d", "-q"]);
    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert!(
        gate_hits(&project.join("target")) > hits,
        "no hit after repacking"
    );
    assert_eq!(run(&binary), head(&project));
}
