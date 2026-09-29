use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

fn build(workspace: &Path, cache: &Path, profile: &Path, agent: bool, meaning: &str) -> Vec<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(workspace)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_REAPI_BUILD_ENVIRONMENT", profile)
        .env("HOME", if agent { "/agent-home" } else { "/scratch" })
        .env("BUILD_MEANING", meaning)
        .args(["--backend", "cache", "--cache-dir"])
        .arg(cache)
        .args(["--", "build", "--offline"]);

    // Keep the workstation's explicit toolchain home independent of synthetic HOME.
    let rustup_home = std::env::var_os("RUSTUP_HOME").map_or_else(
        || Path::new(&std::env::var_os("HOME").expect("home")).join(".rustup"),
        Into::into,
    );
    command.env("RUSTUP_HOME", rustup_home);
    if agent {
        command.env("CODEX_HOME", "/agent-home");
    } else {
        command.env_remove("CODEX_HOME");
    }

    let output = command.output().expect("execute real Cargo ReAPI");
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log = fs::read_to_string(workspace.join("target/cargo-reapi/actions.jsonl"))
        .expect("complete action evidence");
    log.lines()
        .map(|line| serde_json::from_str(line).expect("action JSON"))
        .collect()
}

#[test]
fn project_environment_reuses_agent_build_and_still_invalidates_semantic_changes() {
    let root = tempdir().expect("fixture");
    let workspace = root.path().join("workspace");
    let cache = root.path().join("cache");
    let home = root.path().join("project-home");
    let profile = root.path().join("build-environment.json");
    fs::create_dir_all(workspace.join("src")).expect("sources");
    fs::create_dir(&home).expect("project home");
    fs::write(
        &profile,
        serde_json::to_vec(&serde_json::json!({
            "schema": "cargo-reapi.build-environment/v1",
            "set": {"HOME": home.to_str().expect("home path")},
            "remove": ["CODEX_HOME"]
        }))
        .expect("profile JSON"),
    )
    .expect("profile");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname='environment-proof'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/main.rs"), "fn main() { println!(\"{}:{}\", env!(\"HOME\"), env!(\"BUILD_MEANING\")); assert!(option_env!(\"CODEX_HOME\").is_none()); }\n").expect("source");

    let initial = build(&workspace, &cache, &profile, true, "one");
    assert!(
        initial
            .iter()
            .any(|action| action["execution"] == "local-cache-miss")
    );
    fs::remove_dir_all(workspace.join("target")).expect("remove private target");
    let reused = build(&workspace, &cache, &profile, false, "one");
    assert!(
        reused
            .iter()
            .any(|action| action["execution"] == "gate-snapshot-hit")
    );
    assert!(
        !reused
            .iter()
            .any(|action| action["execution"] == "local-cache-miss")
    );

    fs::remove_dir_all(workspace.join("target")).expect("remove restored target");
    let changed = build(&workspace, &cache, &profile, false, "two");
    assert!(
        changed
            .iter()
            .any(|action| action["execution"] == "local-cache-miss")
    );
    let output = Command::new(workspace.join("target/debug/environment-proof"))
        .output()
        .expect("execute real linked binary");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8"),
        format!("{}:two\n", home.display())
    );
}
