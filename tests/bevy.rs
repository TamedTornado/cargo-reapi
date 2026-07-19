use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::tempdir;

fn copy_fixture(destination: &Path) {
    fs::create_dir_all(destination.join("src")).expect("fixture source directory");
    for relative in ["Cargo.toml", "Cargo.lock", "src/main.rs"] {
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("acceptance/bevy-fixture")
                .join(relative),
            destination.join(relative),
        )
        .expect("copy pinned Bevy fixture");
    }
}

fn build(root: &Path, cache: &Path, log: &Path) -> Duration {
    let started = Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(root)
        .args(["--backend", "cache", "--action-log"])
        .arg(log)
        .arg("--cache-dir")
        .arg(cache)
        .args(["--", "build", "--locked"])
        .status()
        .expect("build Bevy fixture");
    assert!(status.success());
    started.elapsed()
}

#[test]
#[ignore = "explicit pinned-Bevy acceptance proof"]
fn bevy_linked_artifact_restores_after_producer_deletion() {
    let cache = tempdir().expect("shared cache");
    let worktrees = tempdir().expect("worktree parent");
    let producer = worktrees.path().join("p");
    let consumer = worktrees
        .path()
        .join("consumer-with-a-deliberately-different-path-length");
    copy_fixture(&producer);
    copy_fixture(&consumer);

    let producer_log = worktrees.path().join("producer-actions.jsonl");
    let consumer_log = worktrees.path().join("consumer-actions.jsonl");
    build(&producer, cache.path(), &producer_log);
    let producer_actions = fs::read_to_string(&producer_log).expect("producer action log");
    let producer_actions = producer_actions
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("producer action"))
        .collect::<Vec<_>>();
    assert_eq!(
        producer_actions
            .iter()
            .find(|action| action["crate_name"] == "cargo_reapi_bevy_fixture")
            .expect("producer Bevy link")["execution"],
        "local-cache-miss"
    );
    fs::remove_dir_all(&producer).expect("delete producer before consumer");
    let warm_elapsed = build(&consumer, cache.path(), &consumer_log);
    assert!(
        warm_elapsed <= Duration::from_secs(60),
        "pinned Bevy whole-gate restore took {warm_elapsed:?}; contract allows 60s"
    );
    let consumer_actions = fs::read_to_string(&consumer_log).expect("consumer action log");
    let consumer_actions = consumer_actions
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("consumer action"))
        .collect::<Vec<_>>();
    assert_eq!(consumer_actions.len(), 1);
    assert_eq!(consumer_actions[0]["execution"], "gate-snapshot-hit");

    let output = Command::new(consumer.join("target/debug/cargo-reapi-bevy-fixture"))
        .output()
        .expect("run restored Bevy fixture");
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).expect("fixture output");
    let (embedded_path, answer) = output
        .trim()
        .rsplit_once(':')
        .expect("fixture output fields");
    assert_eq!(answer, "42");
    assert_eq!(
        fs::canonicalize(embedded_path).expect("embedded consumer path"),
        fs::canonicalize(&consumer).expect("consumer path")
    );
}
