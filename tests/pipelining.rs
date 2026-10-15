//! Cargo starts a pipelined dependent as soon as a dependency's compiler
//! writes its metadata, while that compile is still running and before
//! cargo-reapi has published it. These tests drive real pipelined Cargo
//! builds through the cache backend.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

/// How long the producer's compile keeps running after rustc has written its
/// metadata. Cargo builds the dependent in that window.
const PRODUCER_HOLD_SECONDS: u32 = 5;

struct Build {
    output: Output,
    actions: Vec<Value>,
}

impl Build {
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn action(&self, crate_name: &str) -> &Value {
        self.actions
            .iter()
            .find(|action| action["crate_name"] == crate_name)
            .unwrap_or_else(|| panic!("no action for {crate_name}: {}", self.stderr()))
    }

    fn position(&self, crate_name: &str) -> usize {
        self.actions
            .iter()
            .position(|action| action["crate_name"] == crate_name)
            .unwrap_or_else(|| panic!("no action for {crate_name}"))
    }

    /// The digest `crate_name`'s compile keyed the input at a logical path
    /// matching `prefix` and `suffix` with.
    fn input_digest(&self, crate_name: &str, prefix: &str, suffix: &str) -> String {
        let inputs = self.action(crate_name)["inputs"]
            .as_array()
            .expect("action inputs");
        inputs
            .iter()
            .find(|input| {
                let path = input["path"].as_str().unwrap_or_default();
                path.starts_with(prefix) && path.ends_with(suffix)
            })
            .and_then(|input| input["sha256"].as_str())
            .unwrap_or_else(|| panic!("{crate_name} has no input {prefix}*{suffix}"))
            .to_owned()
    }
}

/// A rustc that finishes the `producer` compile normally and then holds its
/// process open, as a large crate's code generation does after its metadata
/// is written. It lives outside the worktrees so the toolchain root it
/// implies covers neither of them.
fn write_holding_rustc(tools: &Path) -> (PathBuf, PathBuf) {
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("query rustc sysroot");
    assert!(sysroot.status.success());
    let real_rustc =
        PathBuf::from(String::from_utf8(sysroot.stdout).unwrap().trim()).join("bin/rustc");

    let shim = tools.join("bin/rustc");
    fs::create_dir_all(shim.parent().unwrap()).expect("shim directory");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\n\
             '{real}' \"$@\"\n\
             status=$?\n\
             case \" $* \" in\n\
             *' --print'*) ;;\n\
             *' --crate-name producer '*) sleep {PRODUCER_HOLD_SECONDS} ;;\n\
             esac\n\
             exit $status\n",
            real = real_rustc.display()
        ),
    )
    .expect("holding rustc");
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("executable shim");
    (shim, real_rustc)
}

/// A dependency outside every worktree, shared by them as a registry crate
/// is. Cargo compiles it with absolute paths, so its metadata names its own
/// package root, which a dependent's path mappings do not cover.
fn write_shared_dependency(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("producer source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='producer'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("producer manifest");
    fs::write(
        root.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n\n#[inline]\npub fn doubled() -> u32 { answer() * 2 }\n",
    )
    .expect("producer source");
}

fn write_worktree(root: &Path, dependency: &Path) {
    fs::create_dir_all(root.join("src")).expect("consumer source directory");
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname='consumer'\nversion='0.0.0'\nedition='2024'\n\n\
             [dependencies]\nproducer={{path='{}'}}\n",
            dependency.display()
        ),
    )
    .expect("consumer manifest");
    fs::write(
        root.join("src/lib.rs"),
        "pub fn answer() -> u32 { producer::doubled() / 2 }\n",
    )
    .expect("consumer source");
}

fn pipelined_build(root: &Path, cache: &Path, shim: &Path, real_rustc: &Path) -> Build {
    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_PIPELINING")
        .env("RUSTC", shim)
        .env("CARGO_REAPI_REAL_RUSTC", real_rustc)
        .args(["--backend", "cache", "--action-log"])
        .arg(&action_log)
        .arg("--cache-dir")
        .arg(cache)
        .args(["--", "build"])
        .output()
        .expect("run cargo-reapi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let actions = fs::read_to_string(&action_log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect();
    Build { output, actions }
}

#[test]
fn pipelined_dependent_keys_unpublished_metadata_with_its_published_digest() {
    let fixture = tempdir().expect("fixture");
    let cache = tempdir().expect("shared cache");
    let (shim, real_rustc) = write_holding_rustc(&fixture.path().join("tools"));
    let dependency = fixture.path().join("registry/producer");
    let first = fixture.path().join("first");
    let second = fixture.path().join("second-worktree");
    write_shared_dependency(&dependency);
    write_worktree(&first, &dependency);
    write_worktree(&second, &dependency);

    let cold = pipelined_build(&first, cache.path(), &shim, &real_rustc);
    assert_eq!(cold.action("producer")["execution"], "local-cache-miss");
    assert_eq!(cold.action("consumer")["execution"], "local-cache-miss");
    // The consumer finished while the producer was still compiling, so it
    // read the producer's metadata before cargo-reapi published it.
    assert!(
        cold.position("consumer") < cold.position("producer"),
        "the consumer did not overlap the producer's compile"
    );

    // In another worktree the producer is restored before the consumer
    // starts. The consumer reads the restored metadata and must key it with
    // the digest the first worktree's consumer computed early.
    let warm = pipelined_build(&second, cache.path(), &shim, &real_rustc);
    assert_eq!(warm.action("producer")["execution"], "cache-hit");
    let metadata = |build: &Build| build.input_digest("consumer", "target/", ".rmeta");
    assert_eq!(metadata(&cold), metadata(&warm));
    assert_eq!(warm.action("consumer")["execution"], "cache-hit");
}
