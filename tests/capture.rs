use std::fs;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn cargo_driver_captures_real_rustc_actions_without_credentials() {
    let fixture = tempdir().expect("fixture directory");
    fs::create_dir(fixture.path().join("src")).expect("source directory");
    fs::write(
        fixture.path().join("Cargo.toml"),
        "[package]\nname = \"capture-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("fixture manifest");
    fs::write(
        fixture.path().join("src/lib.rs"),
        "pub fn answer() -> u8 { 42 }\n",
    )
    .expect("fixture source");

    let action_log = fixture.path().join("actions.jsonl");
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(fixture.path())
        .args(["--backend", "capture", "--action-log"])
        .arg(&action_log)
        .args(["--", "check"])
        .env("CARGO_REGISTRY_TOKEN", "do-not-record")
        .status()
        .expect("run cargo-reapi capture");

    assert!(status.success());
    let log = fs::read_to_string(action_log).expect("action log");
    let actions: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect();
    assert!(!actions.is_empty());
    assert!(
        actions
            .iter()
            .any(|action| action["crate_name"] == "capture_fixture")
    );
    assert!(actions.iter().all(|action| {
        action["environment"]
            .as_object()
            .is_some_and(|environment| !environment.contains_key("CARGO_REGISTRY_TOKEN"))
    }));
}
