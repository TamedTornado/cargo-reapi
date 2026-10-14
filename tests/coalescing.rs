//! Two identical cached builds coalesce on the action lock. The test needs
//! the producer to wait until the coalescer is waiting, which the
//! `test-hooks` feature compiles in; run it with
//! `cargo test --features test-hooks`.

mod common;

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

fn write_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"capture-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("fixture manifest");
    fs::write(root.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n").expect("fixture source");
}

fn fixture_action(actions: &[Value]) -> &Value {
    actions
        .iter()
        .find(|action| action["crate_name"] == "capture_fixture")
        .expect("fixture compiler action")
}

fn cache_command(root: &Path, cache_dir: &Path) -> (Command, PathBuf) {
    common::ensure_lockfile(root);
    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_REAPI_ACTION_CACHE_TEST_MODE", "1")
        .args(["--backend", "cache", "--action-log"])
        .arg(&action_log)
        .arg("--cache-dir")
        .arg(cache_dir)
        .args(["--", "check"]);
    (command, action_log)
}

fn read_actions(action_log: &Path) -> Vec<Value> {
    fs::read_to_string(action_log)
        .expect("action log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect()
}

/// Waits for every child to succeed. A failed child fails the test at once
/// and stops the others, which may be waiting for it.
fn wait_for_all(children: &mut [std::process::Child]) {
    let mut statuses = vec![None; children.len()];
    while statuses.contains(&None) {
        for (child, status) in children.iter_mut().zip(&mut statuses) {
            if status.is_none() {
                *status = child.try_wait().expect("inspect child");
            }
        }

        let failure = statuses
            .iter()
            .flatten()
            .find(|status| !status.success())
            .copied();
        if let Some(failure) = failure {
            for child in children.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            panic!("a cached Cargo action failed: {failure}");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn concurrent_identical_actions_execute_once_and_coalesce_on_the_lock() {
    let cache = tempdir().expect("shared cache directory");
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    for fixture in [first.path(), second.path()] {
        write_fixture(fixture);
        let mut source = String::new();
        for index in 0..8_000 {
            writeln!(source, "pub fn value_{index}() -> usize {{ {index} }}")
                .expect("write generated fixture source");
        }
        fs::write(fixture.join("src/lib.rs"), source).expect("large fixture source");
    }

    // Whichever build takes the action's lock holds it until the other
    // signals that it is waiting on it.
    let signal = cache.path().join("coalescer-waiting");
    let (mut first_command, first_log) = cache_command(first.path(), cache.path());
    let (mut second_command, second_log) = cache_command(second.path(), cache.path());
    for command in [&mut first_command, &mut second_command] {
        command.env("CARGO_REAPI_ACTION_CACHE_TEST_COALESCE_SIGNAL", &signal);
    }
    let mut builds = [
        first_command
            .spawn()
            .expect("start first cached Cargo action"),
        second_command
            .spawn()
            .expect("start second cached Cargo action"),
    ];
    wait_for_all(&mut builds);

    let first_actions = read_actions(&first_log);
    let second_actions = read_actions(&second_log);
    let executions = [
        fixture_action(&first_actions)["execution"]
            .as_str()
            .expect("first execution"),
        fixture_action(&second_actions)["execution"]
            .as_str()
            .expect("second execution"),
    ];
    assert!(executions.contains(&"local-cache-miss"));
    assert!(executions.contains(&"coalesced-hit"));
}
