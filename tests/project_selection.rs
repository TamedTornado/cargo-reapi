//! cargo-reapi prepares the project a Cargo invocation selects before the
//! strict sandbox starts: it acquires its sources, chooses its target
//! directory and hashes its inputs for the gate. These tests drive real
//! Cargo through the gated cache backend with selections that differ from
//! the working directory's own workspace.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::tempdir;

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
        Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
            .current_dir(cwd)
            .env("CARGO_HOME", &self.cargo_home)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_REAPI_ACTION_LOG")
            .env_remove("CARGO_REAPI_ACTION_CACHE_TEST_MODE")
            .args(["--backend", "cache", "--cache-dir"])
            .arg(&self.cache)
            .arg("--")
            .args(cargo_args)
            .output()
            .expect("run cargo-reapi")
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

fn assert_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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

#[test]
fn a_target_directory_holding_sources_is_refused_before_it_can_be_restored() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "original", "");
    fs::create_dir_all(project.join(".cargo")).expect("configuration directory");
    fs::write(
        project.join(".cargo/config.toml"),
        "[build]\ntarget-dir = '.'\n",
    )
    .expect("configuration");

    let first = fixture.cargo_reapi(&project, &["fetch"]);
    write_main(&project, "edited");
    let second = fixture.cargo_reapi(&project, &["fetch"]);

    let source = fs::read_to_string(project.join("src/main.rs")).expect("main source");
    assert!(source.contains("edited"), "a gate restored over the edit");
    for output in [&first, &second] {
        assert!(
            !output.status.success(),
            "a source tree was used as the target"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("contains source"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
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

#[test]
fn an_alias_keeps_its_lockfile_policy() {
    let fixture = Fixture::new();
    let project = fixture.path("project");
    write_package(&project, "project", "project", "");
    fs::create_dir_all(project.join(".cargo")).expect("configuration directory");
    fs::write(
        project.join(".cargo/config.toml"),
        "[alias]\nci = 'build --locked'\n",
    )
    .expect("configuration");

    // Plain `cargo ci` refuses to create the missing lockfile.
    let output = fixture.cargo_reapi(&project, &["ci"]);

    assert!(
        !output.status.success(),
        "an aliased --locked build wrote a lockfile"
    );
    assert!(!project.join("Cargo.lock").exists());
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
        &["--all-features"],
    ] {
        let mut arguments = vec!["build", "--manifest-path", "tool/Cargo.toml"];
        arguments.extend_from_slice(features);
        assert_succeeded(&fixture.cargo_reapi(&outer, &arguments));
        assert_eq!(run(&tool.join("target/debug/tool")), "extra");
    }
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
        &["build", "-qm", "tool/Cargo.toml"],
    ] {
        fs::remove_dir_all(tool.join("target")).ok();
        assert_succeeded(&fixture.cargo_reapi(&outer, arguments));
        assert_eq!(run(&tool.join("target/debug/tool")), "tool");
    }
}
