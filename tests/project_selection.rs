//! cargo-reapi prepares the project a Cargo invocation selects before the
//! strict sandbox starts: it acquires its sources, chooses its target
//! directory and hashes its inputs for the gate. It reads only what it can
//! read exactly and refuses the rest. These tests drive real Cargo through
//! the gated cache backend.

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

fn assert_succeeded(output: &Output) {
    assert!(output.status.success(), "{}", stderr(output));
}

fn assert_refused(output: &Output, reason: &str) {
    assert!(!output.status.success(), "accepted: {}", stderr(output));
    assert!(stderr(output).contains(reason), "{}", stderr(output));
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

/// Runs a cached `fetch` with `project`'s configured target directory, edits
/// `source`, and runs it again: both runs must refuse the target, and the
/// edit must survive.
fn assert_target_refused_around_an_edit(fixture: &Fixture, project: &Path, source: &Path) {
    let first = fixture.cargo_reapi(project, &["fetch"]);
    fs::write(source, "fn main() { println!(\"edited\"); }\n").expect("edit source");
    let second = fixture.cargo_reapi(project, &["fetch"]);

    let text = fs::read_to_string(source).expect("source");
    assert!(text.contains("edited"), "a gate restored over the edit");
    for output in [&first, &second] {
        assert_refused(output, "Cargo's target directory");
    }
}

#[test]
fn a_target_directory_holding_sources_is_refused_before_it_can_be_restored() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "original", "");
    write_config(&project, "config.toml", "[build]\ntarget-dir = '.'\n");

    assert_target_refused_around_an_edit(&fixture, &project, &project.join("src/main.rs"));
}

#[cfg(unix)]
#[test]
fn a_target_directory_at_inside_or_linked_to_a_source_directory_is_refused() {
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

        assert_target_refused_around_an_edit(&fixture, &project, &project.join("src/main.rs"));
    }
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

    assert_target_refused_around_an_edit(&fixture, &project, &library);
}

#[cfg(unix)]
#[test]
fn a_package_selected_through_a_symlink_is_hashed_by_its_real_directory() {
    let fixture = Fixture::new();
    let outer = fixture.path("outer");
    let real = fixture.path("real");
    write_package(&outer, "outer", "outer", "");
    write_package(&real, "real", "first", "\n[workspace]\n");
    std::os::unix::fs::symlink("../real", outer.join("link")).expect("link to real");
    let arguments = ["build", "--manifest-path", "link/Cargo.toml"];

    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));
    assert_eq!(run(&real.join("target/debug/real")), "first");
    write_main(&real, "second");
    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));

    assert_eq!(run(&real.join("target/debug/real")), "second");
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
    let arguments = ["build", "--manifest-path", "target/tool/Cargo.toml"];

    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));
    assert_eq!(run(&tool.join("target/debug/tool")), "first");
    write_main(&tool, "second");
    assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));

    assert_eq!(run(&tool.join("target/debug/tool")), "second");
}

/// Defines the `ci` alias somewhere a Cargo run in `project` reads it, and
/// returns the environment that run needs.
type AliasDefinition = fn(&Fixture, &Path) -> Vec<(&'static str, PathBuf)>;

#[test]
fn user_aliases_are_refused_wherever_they_are_defined() {
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

        let output = fixture.cargo_reapi_with(&project, &["ci"], &environment);

        assert!(!output.status.success(), "{origin}: {}", stderr(&output));
        assert!(
            stderr(&output).contains("user-defined Cargo alias"),
            "{origin}: {}",
            stderr(&output)
        );
        assert!(!project.join("Cargo.lock").exists(), "{origin}");
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
fn configuration_that_includes_more_is_refused_only_where_an_alias_could_hide() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");
    write_config(&project, "config.toml", "include = ['more.toml']\n");
    write_config(&project, "more.toml", "");

    assert_succeeded(&fixture.cargo_reapi(&project, &["build"]));
    assert_refused(
        &fixture.cargo_reapi(&project, &["b"]),
        "includes further configuration",
    );
}

#[test]
fn options_that_could_change_the_selection_unseen_are_refused() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");

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
    ] {
        let output = fixture.cargo_reapi(&project, arguments);
        assert!(!output.status.success(), "{arguments:?} was accepted");
        assert!(
            stderr(&output).contains(reason),
            "{arguments:?}: {}",
            stderr(&output)
        );
    }
    assert!(!project.join("src/debug").exists(), "Cargo built into src");
    assert!(!project.join("target").exists(), "Cargo built the project");

    let harmless = ["build", "--config", "profile.dev.debug=0"];
    assert_succeeded(&fixture.cargo_reapi(&project, &harmless));
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
}

#[test]
fn a_repeated_lockfile_flag_reaches_metadata_once() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");
    assert_succeeded(&fixture.cargo_reapi(&project, &["generate-lockfile"]));

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
    assert_succeeded(&fixture.cargo_reapi(&project, &["generate-lockfile"]));

    let output = fixture.cargo_reapi(&project, &["build", "--offline", "-F", "small"]);

    assert_succeeded(&output);
    assert_eq!(run(&project.join("target/debug/project")), "small");
    assert_refused(
        &fixture.cargo_reapi(
            &project,
            &["build", "--offline", "-p", "project", "-Fsmall"],
        ),
        "selected with -p",
    );
}

#[test]
fn a_new_nested_target_directory_is_created() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "nested", "");

    let output = fixture.cargo_reapi(&project, &["build", "--target-dir", "artifacts/new"]);

    assert_succeeded(&output);
    assert_eq!(run(&project.join("artifacts/new/debug/project")), "nested");
}

#[test]
fn a_sibling_selected_workspace_stays_readable() {
    let fixture = Fixture::new();
    let outer = fixture.path("outer");
    let other = fixture.path("other");
    write_package(&outer, "outer", "outer", "");
    write_package(&other, "other", "sibling", "");

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

    for arguments in [
        &["build", "-m", "tool/Cargo.toml"][..],
        &["build", "-mtool/Cargo.toml"],
        &["build", "-m=tool/Cargo.toml"],
    ] {
        fs::remove_dir_all(tool.join("target")).ok();
        assert_succeeded(&fixture.cargo_reapi(&outer, arguments));
        assert_eq!(run(&tool.join("target/debug/tool")), "tool");
    }
    assert_refused(
        &fixture.cargo_reapi(&outer, &["build", "-qm", "tool/Cargo.toml"]),
        "own argument",
    );
}
