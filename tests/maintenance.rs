use std::collections::{BTreeMap, BTreeSet};
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

/// A package whose library compile is cached and whose binary prints what
/// the library returns.
fn fixture_workspace(root: &Path, name: &str) -> PathBuf {
    let workspace = root.join(name);
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname='maintenance-fixture'\nversion='0.0.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn status() -> &'static str { \"complete\" }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() { println!(\"{}\", maintenance_fixture::status()); }\n",
    )
    .unwrap();
    workspace
}

fn cached_build_command(workspace: &Path, cache: &Path, action_log: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(workspace)
        .env_remove("CARGO_TARGET_DIR")
        .args(["--backend", "cache", "--cache-dir"])
        .arg(cache)
        .arg("--action-log")
        .arg(action_log)
        .args(["--", "build", "--offline"]);
    command
}

fn cached_build(workspace: &Path, cache: &Path, action_log: &Path) -> Output {
    cached_build_command(workspace, cache, action_log)
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

#[test]
fn collection_removes_directories_of_earlier_versions_and_keeps_action_entries() {
    let root = tempdir().unwrap();
    let cache = root.path().join("cache");
    let producer = fixture_workspace(root.path(), "producer");

    let built = cached_build(&producer, &cache, &root.path().join("producer.jsonl"));
    assert!(built.status.success(), "{built:?}");
    let reusable_bytes = tree_bytes(&cache);

    // Gate snapshots of an earlier version, which this one never reads.
    let obsolete = cache.join("gate-snapshots");
    fs::create_dir_all(obsolete.join("objects/earlier/target")).unwrap();
    fs::write(
        obsolete.join("objects/earlier/target/data"),
        vec![0_u8; 4 * 1024 * 1024],
    )
    .unwrap();

    // The budget fits every reusable entry, but not the obsolete directory.
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

    assert_eq!(report["removed_action_entries"], 0, "{report}");
    assert_eq!(report["removed_obsolete_directories"], 1, "{report}");
    assert_eq!(report["capacity_satisfied"], true, "{report}");
    assert!(!obsolete.exists());

    // A fresh workspace still restores the library from the surviving entry.
    let consumer = fixture_workspace(root.path(), "consumer");
    let consumer_log = root.path().join("consumer.jsonl");
    let reused = cached_build(&consumer, &cache, &consumer_log);
    assert!(reused.status.success(), "{reused:?}");
    let actions = fs::read_to_string(&consumer_log).unwrap();
    assert!(actions.contains("\"cache-hit\""), "{actions}");
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
        let workspace = fixture_workspace(root.path(), &format!("consumer-{index}"));
        let action_log = root.path().join(format!("actions-{index}.jsonl"));
        let output = File::create(root.path().join(format!("build-{index}.log"))).unwrap();
        let process = cached_build_command(&workspace, &cache, &action_log)
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        builds.push((workspace, OwnedChild(process)));
    }

    // Collectors run back to back while the builds proceed. However slowly
    // they run, each collector must finish; one that waits in a cycle of
    // file locks with the builds never would, and the lock table shows it.
    let mut collections = 0;
    loop {
        let mut gc = collector(&cache);
        loop {
            if let Some(status) = gc.0.try_wait().unwrap() {
                assert!(status.success(), "native collector failed");
                break;
            }
            if let Some(cycle) = lock_cycle() {
                panic!("collector/producer lock cycle: {cycle}");
            }
            thread::sleep(Duration::from_millis(50));
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

/// A cycle of processes descended from this test, each waiting for another:
/// for a file lock that one holds, from the kernel's lock table, or, as a
/// parent does, for its child to finish. `None` where there is none, or
/// where the platform has no lock table.
fn lock_cycle() -> Option<String> {
    let table = fs::read_to_string("/proc/locks").ok()?;
    let parents = descendant_parents();

    let mut holders: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    let mut waits: Vec<(u32, String)> = Vec::new();
    for line in table.lines() {
        let mut fields = line.split_whitespace().skip(1).peekable();
        let waiting = fields.next_if_eq(&"->").is_some();
        let fields = fields.collect::<Vec<_>>();
        let [kind, _, _, pid, file, ..] = fields[..] else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if kind != "FLOCK" || !parents.contains_key(&pid) {
            continue;
        }
        if waiting {
            waits.push((pid, file.to_owned()));
        } else {
            holders.entry(file.to_owned()).or_default().insert(pid);
        }
    }

    let mut waits_for: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for (waiter, file) in &waits {
        for holder in holders.get(file).into_iter().flatten() {
            if holder != waiter {
                waits_for.entry(*waiter).or_default().insert(*holder);
            }
        }
    }
    for (child, parent) in &parents {
        if parents.contains_key(parent) {
            waits_for.entry(*parent).or_default().insert(*child);
        }
    }
    waits_for
        .keys()
        .find_map(|start| cycle_from(*start, &waits_for, &mut Vec::new()))
}

fn cycle_from(
    process: u32,
    waits_for: &BTreeMap<u32, BTreeSet<u32>>,
    path: &mut Vec<u32>,
) -> Option<String> {
    if let Some(position) = path.iter().position(|seen| *seen == process) {
        let cycle = path[position..]
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>();
        return Some(format!(
            "processes {} wait on each other",
            cycle.join(" -> ")
        ));
    }
    path.push(process);
    let found = waits_for
        .get(&process)
        .into_iter()
        .flatten()
        .find_map(|next| cycle_from(*next, waits_for, path));
    path.pop();
    found
}

/// The parent of every running process descended from this test.
fn descendant_parents() -> BTreeMap<u32, u32> {
    let mut parents = BTreeMap::new();
    for entry in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The command name, in parentheses, may itself hold spaces.
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|parent| parent.parse::<u32>().ok());
        if let Some(parent) = parent {
            parents.insert(pid, parent);
        }
    }

    let this_test = std::process::id();
    let descends = |pid: u32| {
        let mut current = pid;
        while let Some(parent) = parents.get(&current) {
            if *parent == this_test {
                return true;
            }
            current = *parent;
        }
        false
    };
    parents
        .iter()
        .filter(|(pid, _)| descends(**pid))
        .map(|(pid, parent)| (*pid, *parent))
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn the_lock_cycle_check_sees_processes_waiting_on_each_other() {
    let root = tempdir().unwrap();
    let first = root.path().join("first.lock");
    let second = root.path().join("second.lock");
    let ready = |name: &str| root.path().join(name);

    // Each holds one lock, says so, waits until the other holds its own,
    // then waits for the other's lock.
    let holder = |held: &Path, wanted: &Path, mine: &str, theirs: &str| {
        let script = format!(
            "touch '{}'; while [ ! -e '{}' ]; do sleep 0.01; done; flock '{}' true",
            ready(mine).display(),
            ready(theirs).display(),
            wanted.display()
        );
        OwnedChild(
            // `-o`: the lock stays with `flock` itself, not the shell, so
            // stopping `flock` releases it.
            Command::new("flock")
                .arg("-o")
                .arg(held)
                .args(["sh", "-c", &script])
                .spawn()
                .expect("flock"),
        )
    };
    assert!(lock_cycle().is_none(), "a cycle before any lock is taken");
    let _first = holder(&first, &second, "first-ready", "second-ready");
    let _second = holder(&second, &first, "second-ready", "first-ready");

    let cycle = loop {
        if let Some(cycle) = lock_cycle() {
            break cycle;
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(cycle.contains("wait on each other"), "{cycle}");
}
