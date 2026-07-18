use std::fs;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

fn write_fixture(root: &std::path::Path, binary: bool) {
    fs::create_dir(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"capture-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("fixture manifest");
    let source = if binary {
        "fn main() { println!(\"{}\", 42); }\n"
    } else {
        "pub fn answer() -> u8 { 42 }\n"
    };
    fs::write(
        root.join(if binary { "src/main.rs" } else { "src/lib.rs" }),
        source,
    )
    .expect("fixture source");
}

fn capture(root: &std::path::Path, cargo_command: &str) -> Vec<Value> {
    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(root)
        .args(["--backend", "capture", "--action-log"])
        .arg(&action_log)
        .args(["--", cargo_command])
        .env("CARGO_REGISTRY_TOKEN", "do-not-record")
        .status()
        .expect("run cargo-reapi capture");
    assert!(status.success());
    fs::read_to_string(action_log)
        .expect("action log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect()
}

fn fixture_action(actions: &[Value]) -> &Value {
    actions
        .iter()
        .find(|action| action["crate_name"] == "capture_fixture")
        .expect("fixture compiler action")
}

#[test]
fn cargo_driver_captures_real_rustc_actions_without_credentials() {
    let fixture = tempdir().expect("fixture directory");
    write_fixture(fixture.path(), false);
    let actions = capture(fixture.path(), "check");
    assert!(!actions.is_empty());
    let action = fixture_action(&actions);
    assert_eq!(action["schema_version"], 2);
    assert_eq!(action["remote_eligibility"]["eligible"], true);
    assert!(
        action["action_key"]
            .as_str()
            .is_some_and(|key| key.len() == 64)
    );
    assert!(
        action["working_directory"]
            .as_str()
            .is_some_and(|path| path == "workspace")
    );
    assert!(actions.iter().all(|action| {
        action["environment"]
            .as_object()
            .is_some_and(|environment| !environment.contains_key("CARGO_REGISTRY_TOKEN"))
    }));
}

#[test]
fn identical_projects_in_different_worktrees_have_the_same_action_key() {
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    write_fixture(first.path(), false);
    write_fixture(second.path(), false);

    let first_actions = capture(first.path(), "check");
    let second_actions = capture(second.path(), "check");
    let first_action = fixture_action(&first_actions);
    let second_action = fixture_action(&second_actions);

    assert_eq!(first_action["action_key"], second_action["action_key"]);
    assert_eq!(first_action["inputs"], second_action["inputs"]);
    assert_eq!(first_action["arguments"], second_action["arguments"]);
}

#[test]
fn linked_binary_fails_remote_eligibility_closed() {
    let fixture = tempdir().expect("fixture directory");
    write_fixture(fixture.path(), true);
    let actions = capture(fixture.path(), "build");
    let action = fixture_action(&actions);

    assert_eq!(action["remote_eligibility"]["eligible"], false);
    assert!(
        action["remote_eligibility"]["reasons"]
            .as_array()
            .is_some_and(
                |reasons| reasons.iter().any(|reason| reason.as_str().is_some_and(
                    |reason| reason.contains("link action input discovery is incomplete")
                ))
            )
    );
}
