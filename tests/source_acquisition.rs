//! The strict sandbox runs Cargo offline, so cargo-reapi acquires the
//! dependency sources an invocation needs before it starts. These tests
//! drive real Cargo with a dependency absent from a private `CARGO_HOME`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

/// A project whose own workspace sits inside another one's directory, as a
/// vendored tool does, and depends on a crate never fetched before.
struct Fixture {
    root: tempfile::TempDir,
    outer: PathBuf,
    cargo_home: PathBuf,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir().expect("fixture");
        let source = root.path().join("upstream/fetched");
        let outer = root.path().join("outer");
        let cargo_home = root.path().join("cargo-home");
        let cache = root.path().join("cache");
        write_git_dependency(&source);
        write_outer_workspace(&outer, &source);
        fs::create_dir_all(&cargo_home).expect("private CARGO_HOME");

        Self {
            root,
            outer,
            cargo_home,
            cache,
        }
    }

    /// Runs the gated cache backend from the outer workspace, with the
    /// strict sandbox, into the outer target as `moria_cargo` does.
    fn cargo_reapi(&self, cargo_args: &[&str]) -> Output {
        self.command(cargo_args)
            .env("CARGO_TARGET_DIR", self.outer.join("target"))
            .output()
            .expect("run cargo-reapi")
    }

    /// Runs the gated cache backend from the outer workspace and lets Cargo
    /// choose the target directory and action log location.
    fn cargo_reapi_in_cargos_target(&self, cargo_args: &[&str]) -> Output {
        self.command(cargo_args)
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .expect("run cargo-reapi")
    }

    fn command(&self, cargo_args: &[&str]) -> Command {
        common::ensure_lockfile(&self.outer);
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
        command
            .current_dir(&self.outer)
            .env("CARGO_HOME", &self.cargo_home)
            .env_remove("CARGO_REAPI_ACTION_LOG")
            .env_remove("CARGO_REAPI_ACTION_CACHE_TEST_MODE")
            .args(["--backend", "cache", "--cache-dir"])
            .arg(&self.cache)
            .arg("--")
            .args(cargo_args);
        command
    }

    /// Writes the tool's lockfile, which a cached build needs. A throwaway
    /// `CARGO_HOME` resolves it, so the fixture's own stays without the
    /// dependency and the invocation still has to acquire it.
    fn lock_tool(&self) {
        let status = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(self.outer.join("tool"))
            .env("CARGO_HOME", self.root.path().join("lock-home"))
            .status()
            .expect("run cargo generate-lockfile");
        assert!(status.success(), "generate-lockfile failed");
    }

    fn checkouts(&self) -> PathBuf {
        self.cargo_home.join("git/checkouts")
    }
}

fn write_git_dependency(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("dependency source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='fetched'\nversion='0.1.0'\nedition='2024'\n",
    )
    .expect("dependency manifest");
    fs::write(root.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n")
        .expect("dependency source");

    for arguments in [
        &["init", "--quiet"][..],
        &["add", "."],
        &["commit", "--quiet", "--message", "Initial"],
    ] {
        let status = Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "user.name=cargo-reapi",
                "-c",
                "user.email=cargo-reapi@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(arguments)
            .status()
            .expect("run git");
        assert!(status.success(), "git {arguments:?} failed");
    }
}

/// `outer` is a workspace that excludes `outer/tool`, a separate workspace
/// built with `--manifest-path`.
fn write_outer_workspace(outer: &Path, dependency: &Path) {
    fs::create_dir_all(outer.join("src")).expect("outer source directory");
    fs::write(
        outer.join("Cargo.toml"),
        "[package]\nname='outer'\nversion='0.0.0'\nedition='2024'\n\n\
         [workspace]\nexclude=['tool']\n",
    )
    .expect("outer manifest");
    fs::write(outer.join("src/main.rs"), "fn main() {}\n").expect("outer source");

    let tool = outer.join("tool");
    fs::create_dir_all(tool.join("src")).expect("tool source directory");
    fs::write(
        tool.join("Cargo.toml"),
        format!(
            "[package]\nname='tool'\nversion='0.0.0'\nedition='2024'\n\n\
             [dependencies]\nfetched={{git='file://{}'}}\n\n[workspace]\n",
            dependency.display()
        ),
    )
    .expect("tool manifest");
    fs::write(
        tool.join("src/main.rs"),
        "fn main() { println!(\"{}\", fetched::answer()); }\n",
    )
    .expect("tool source");
}

fn assert_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_selected_manifest_acquires_its_new_dependencies_before_the_sandbox() {
    let fixture = Fixture::new();
    fixture.lock_tool();

    let build =
        fixture.cargo_reapi_in_cargos_target(&["build", "--manifest-path", "tool/Cargo.toml"]);

    assert_succeeded(&build);
    assert!(!String::from_utf8_lossy(&build.stderr).contains("not caching"));
    let tool = Command::new(fixture.outer.join("tool/target/debug/tool"))
        .output()
        .expect("run tool");
    assert_eq!(String::from_utf8_lossy(&tool.stdout).trim(), "42");
}

/// Plain Cargo builds a `--manifest-path` workspace into that workspace's
/// own target directory, and so must the sandbox and gate.
#[test]
fn a_selected_manifest_builds_into_its_own_target_directory() {
    let fixture = Fixture::new();
    fixture.lock_tool();
    let tool = fixture.outer.join("tool");

    let build =
        fixture.cargo_reapi_in_cargos_target(&["build", "--manifest-path", "tool/Cargo.toml"]);

    assert_succeeded(&build);
    assert!(!String::from_utf8_lossy(&build.stderr).contains("not caching"));
    let binary = Command::new(tool.join("target/debug/tool"))
        .output()
        .expect("run tool from its own target");
    assert_eq!(String::from_utf8_lossy(&binary.stdout).trim(), "42");
    assert!(tool.join("target/cargo-reapi/actions.jsonl").is_file());
    assert!(!fixture.outer.join("target").exists());
}

#[test]
fn fetch_with_a_selected_manifest_acquires_its_dependencies() {
    let fixture = Fixture::new();

    let fetch = fixture.cargo_reapi(&["fetch", "--manifest-path", "tool/Cargo.toml"]);

    assert_succeeded(&fetch);
    let checkouts = fs::read_dir(fixture.checkouts())
        .map(Iterator::count)
        .unwrap_or_default();
    assert_eq!(checkouts, 1, "the git dependency was not checked out");
}

#[test]
fn a_locked_invocation_keeps_cargos_lockfile_policy() {
    let fixture = Fixture::new();

    // The tool has no lockfile, so plain Cargo refuses `--locked`; acquiring
    // its sources must not write one first.
    let build = fixture.cargo_reapi(&["build", "--manifest-path", "tool/Cargo.toml", "--locked"]);

    assert!(!build.status.success(), "a --locked build wrote a lockfile");
    assert!(!fixture.outer.join("tool/Cargo.lock").exists());
}

/// Compiles run in their package's directory. A dependency outside the
/// workspace compiles elsewhere, and the default action log must still
/// collect it.
#[test]
fn the_default_action_log_collects_compiles_run_outside_the_workspace() {
    let root = tempdir().expect("fixture");
    let dependency = root.path().join("shared/helper");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(dependency.join("src")).expect("helper source directory");
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname='helper'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("helper manifest");
    fs::write(dependency.join("src/lib.rs"), "pub fn one() -> u32 { 1 }\n").expect("helper source");
    fs::create_dir_all(workspace.join("src")).expect("app source directory");
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname='app'\nversion='0.0.0'\nedition='2024'\n\n\
             [dependencies]\nhelper={{path='{}'}}\n",
            dependency.display()
        ),
    )
    .expect("app manifest");
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() { assert_eq!(helper::one(), 1); }\n",
    )
    .expect("app source");

    common::ensure_lockfile(&workspace);
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(&workspace)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_REAPI_ACTION_LOG")
        .env("CARGO_REAPI_ACTION_CACHE_TEST_MODE", "1")
        .args(["--backend", "cache", "--cache-dir"])
        .arg(root.path().join("cache"))
        .args(["--", "build"])
        .output()
        .expect("run cargo-reapi");
    assert_succeeded(&output);

    let crates = fs::read_to_string(workspace.join("target/cargo-reapi/actions.jsonl"))
        .expect("default action log")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid JSON action"))
        .filter_map(|action| action["crate_name"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(crates.iter().any(|name| name == "helper"), "{crates:?}");
    assert!(crates.iter().any(|name| name == "app"), "{crates:?}");
    assert!(!dependency.join("target").exists());
}
