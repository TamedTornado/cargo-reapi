use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::Value;
use tempfile::tempdir;
use walkdir::WalkDir;

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().expect("inspect owned child").is_none() {
            self.0.kill().expect("stop owned child");
        }
        self.0.wait().expect("reap owned child");
    }
}

fn lock_file(root: &Path, name: &str) -> File {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(name))
        .unwrap()
}

fn collector(cache: &Path) -> OwnedChild {
    OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
            .args(["cache", "gc", "--cache-dir"])
            .arg(cache)
            .args(["--max-bytes", "0", "--json"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn fixture_workspace(root: &Path, name: &str) -> PathBuf {
    let workspace = root.join(name);
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname='maintenance-fixture'\nversion='0.0.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() { println!(\"complete\"); }\n",
    )
    .unwrap();
    workspace
}

fn cached_build(workspace: &Path, cache: &Path, action_log: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(workspace)
        .env_remove("CARGO_TARGET_DIR")
        .args(["--backend", "cache", "--cache-dir"])
        .arg(cache)
        .arg("--action-log")
        .arg(action_log)
        .args(["--", "build", "--offline"])
        .output()
        .unwrap()
}

fn tree_bytes(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.metadata().unwrap().len())
        .sum()
}

fn published_gates(cache: &Path) -> Vec<String> {
    fs::read_dir(cache.join("gate-snapshots/objects"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect()
}

#[test]
fn collection_reclaims_abandoned_gate_staging_and_keeps_reusable_snapshots() {
    let root = tempdir().unwrap();
    let cache = root.path().join("cache");
    let producer = fixture_workspace(root.path(), "producer");

    let built = cached_build(&producer, &cache, &root.path().join("producer.jsonl"));
    assert!(built.status.success(), "{built:?}");
    let published = published_gates(&cache);
    assert_eq!(
        published.len(),
        1,
        "the successful build publishes its gate"
    );
    let reusable_bytes = tree_bytes(&cache);

    // A producer killed during publication never runs its staging cleanup.
    let abandoned = cache.join("gate-snapshots/objects/.gate-killed");
    fs::create_dir_all(abandoned.join("target")).unwrap();
    fs::write(abandoned.join("target/data"), vec![0_u8; 4 * 1024 * 1024]).unwrap();

    // The budget fits every reusable entry, but not the abandoned staging.
    let collected = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .args(["cache", "gc", "--cache-dir"])
        .arg(&cache)
        .arg("--max-bytes")
        .arg((reusable_bytes + 1024 * 1024).to_string())
        .arg("--json")
        .output()
        .unwrap();
    assert!(collected.status.success(), "{collected:?}");
    let report: Value = serde_json::from_slice(&collected.stdout).unwrap();

    assert_eq!(report["removed_gate_entries"], 0, "{report}");
    assert_eq!(report["removed_action_entries"], 0, "{report}");
    assert_eq!(report["capacity_satisfied"], true, "{report}");
    assert_eq!(report["removed_abandoned_staging_entries"], 1, "{report}");
    assert!(!abandoned.exists());
    assert_eq!(published_gates(&cache), published);

    // A fresh workspace still reuses the surviving gate snapshot.
    let consumer = fixture_workspace(root.path(), "consumer");
    let consumer_log = root.path().join("consumer.jsonl");
    let reused = cached_build(&consumer, &cache, &consumer_log);
    assert!(reused.status.success(), "{reused:?}");
    let actions = fs::read_to_string(&consumer_log).unwrap();
    assert!(actions.contains("\"gate-snapshot-hit\""), "{actions}");
    let output = Command::new(consumer.join("target/debug/maintenance-fixture"))
        .output()
        .unwrap();
    assert_eq!(output.stdout, b"complete\n");
}

#[test]
fn terminated_collector_releases_admission_without_stale_marker_recovery() {
    let root = tempdir().unwrap();
    let active = lock_file(root.path(), ".maintenance.lock");
    FileExt::lock_shared(&active).unwrap();
    let admission = lock_file(root.path(), ".maintenance-admission.lock");
    let mut gc = collector(root.path());
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        match FileExt::try_lock_exclusive(&admission) {
            Ok(()) => FileExt::unlock(&admission).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("observe admission: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "collector never closed admission"
        );
        thread::sleep(Duration::from_millis(1));
    }

    gc.0.kill().unwrap();
    gc.0.wait().unwrap();
    FileExt::try_lock_exclusive(&admission).expect("OS releases the dead collector's admission");
    FileExt::unlock(&admission).unwrap();
    FileExt::unlock(&active).unwrap();
    assert!(collector(root.path()).0.wait().unwrap().success());
}

#[test]
fn real_parallel_cargo_producers_and_coalescers_finish_with_repeated_collection() {
    let root = tempdir().unwrap();
    let cache = root.path().join("cache");
    fs::create_dir(&cache).unwrap();
    let mut builds = Vec::new();

    for index in 0..3 {
        let workspace = root.path().join(format!("consumer-{index}"));
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname='maintenance-fixture'\nversion='0.0.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(
            workspace.join("src/main.rs"),
            "fn main() { println!(\"complete\"); }\n",
        )
        .unwrap();
        let output = File::create(root.path().join(format!("build-{index}.log"))).unwrap();
        let process = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
            .current_dir(&workspace)
            .env_remove("CARGO_TARGET_DIR")
            .args(["--backend", "cache", "--cache-dir"])
            .arg(&cache)
            .arg("--action-log")
            .arg(root.path().join(format!("actions-{index}.jsonl")))
            .args(["--", "build", "--offline"])
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        builds.push((workspace, OwnedChild(process)));
    }

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut collections = 0;
    loop {
        let mut gc = collector(&cache);
        loop {
            if let Some(status) = gc.0.try_wait().unwrap() {
                assert!(status.success(), "native collector failed");
                break;
            }
            assert!(Instant::now() < deadline, "collector/producer lock cycle");
            thread::sleep(Duration::from_millis(5));
        }
        collections += 1;
        let mut all_finished = true;
        for (workspace, process) in &mut builds {
            match process.0.try_wait().unwrap() {
                Some(status) => {
                    assert!(status.success(), "Cargo failed in {}", workspace.display());
                }
                None => all_finished = false,
            }
        }
        if all_finished && collections >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "parallel Cargo builds did not finish"
        );
        thread::sleep(Duration::from_millis(10));
    }

    for (workspace, _) in builds {
        let result = Command::new(workspace.join("target/debug/maintenance-fixture"))
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, b"complete\n");
    }
}
