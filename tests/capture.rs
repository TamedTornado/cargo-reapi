use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Barrier};

use serde_json::Value;
use tempfile::{TempDir, tempdir};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn write_fixture(root: &std::path::Path, binary: bool) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"capture-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .expect("fixture manifest");
    let source = if binary {
        "fn main() { println!(\"{}\", env!(\"CARGO_MANIFEST_DIR\")); }\n"
    } else {
        "pub fn answer() -> u8 { 42 }\n"
    };
    fs::write(
        root.join(if binary { "src/main.rs" } else { "src/lib.rs" }),
        source,
    )
    .expect("fixture source");
}

fn write_proc_macro_fixture(root: &Path) {
    fs::create_dir_all(root.join("macro/src")).expect("macro source directory");
    fs::create_dir_all(root.join("app/src")).expect("app source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['macro','app']\nresolver='3'\n",
    )
    .expect("workspace manifest");
    fs::write(
        root.join("macro/Cargo.toml"),
        "[package]\nname='identity-macro'\nversion='0.0.0'\nedition='2024'\n[lib]\nproc-macro=true\n",
    )
    .expect("macro manifest");
    fs::write(
        root.join("macro/src/lib.rs"),
        "extern crate proc_macro;\nuse proc_macro::TokenStream;\n#[proc_macro_attribute]\npub fn identity(_: TokenStream, item: TokenStream) -> TokenStream { item }\n",
    )
    .expect("macro source");
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname='macro-app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nidentity-macro={path='../macro'}\n",
    )
    .expect("app manifest");
    fs::write(
        root.join("app/src/main.rs"),
        "use identity_macro::identity;\n#[identity]\nfn answer() -> u8 { 42 }\nfn main() { println!(\"{}:{}\", env!(\"CARGO_MANIFEST_DIR\"), answer()); }\n",
    )
    .expect("app source");
}

fn write_library_app_fixture(root: &Path) {
    fs::create_dir_all(root.join("leaf/src")).expect("leaf source directory");
    fs::create_dir_all(root.join("app/src")).expect("app source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['leaf','app']\nresolver='3'\n",
    )
    .expect("workspace manifest");
    fs::write(
        root.join("leaf/Cargo.toml"),
        "[package]\nname='relocated-leaf'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("leaf manifest");
    fs::write(
        root.join("leaf/src/lib.rs"),
        "pub fn answer() -> u8 { 42 }\n",
    )
    .expect("leaf source");
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname='relocated-app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nrelocated-leaf={path='../leaf'}\n",
    )
    .expect("app manifest");
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}:{}\", env!(\"CARGO_MANIFEST_DIR\"), relocated_leaf::answer()); }\n",
    )
    .expect("app source");
}

fn write_companion_binary_fixture(root: &Path) {
    fs::create_dir_all(root.join(".cargo")).expect("Cargo config directory");
    fs::create_dir_all(root.join("src/bin")).expect("binary source directory");
    fs::create_dir_all(root.join("tests")).expect("integration test directory");
    fs::create_dir_all(root.join("fixture-input")).expect("config-relative input directory");
    fs::write(
        root.join(".cargo/config.toml"),
        "[env]\nCOMPANION_DATA = { value = \"fixture-input\", relative = true, force = true }\n",
    )
    .expect("Cargo config");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='companion-fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n",
    )
    .expect("fixture manifest");
    fs::write(
        root.join("build.rs"),
        concat!(
            "fn main() {\n",
            "    let manifest = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n",
            "    println!(\"cargo::rustc-env=BUILD_SCRIPT_DATA={manifest}/fixture-input\");\n",
            "    println!(\"cargo::rerun-if-changed=build.rs\");\n",
            "}\n",
        ),
    )
    .expect("build script");
    fs::write(
        root.join("src/bin/companion.rs"),
        concat!(
            "fn main() {\n",
            "    println!(\"companion-ok\");\n",
            "    println!(\"{}\", env!(\"COMPANION_DATA\"));\n",
            "    println!(\"{}\", env!(\"BUILD_SCRIPT_DATA\"));\n",
            "}\n",
        ),
    )
    .expect("companion source");
    fs::write(
        root.join("tests/spawns_companion.rs"),
        concat!(
            "use std::process::Command;\n",
            "use std::path::PathBuf;\n",
            "#[test]\n",
            "fn integration_test_can_spawn_its_cargo_binary() {\n",
            "    let companion = env!(\"CARGO_BIN_EXE_companion\");\n",
            "    let output = Command::new(companion)\n",
            "        .output()\n",
            "        .unwrap_or_else(|error| panic!(\"companion binary {companion} should exist: {error}\"));\n",
            "    assert!(output.status.success());\n",
            "    let stdout = String::from_utf8(output.stdout).expect(\"companion stdout\");\n",
            "    let mut lines = stdout.lines();\n",
            "    assert_eq!(lines.next(), Some(\"companion-ok\"));\n",
            "    let actual = PathBuf::from(lines.next().expect(\"config-relative path\"))\n",
            "        .canonicalize().expect(\"restored config-relative path exists\");\n",
            "    let build_script = PathBuf::from(lines.next().expect(\"build-script path\"))\n",
            "        .canonicalize().expect(\"restored build-script path exists\");\n",
            "    let expected = PathBuf::from(env!(\"CARGO_MANIFEST_DIR\"))\n",
            "        .join(\"fixture-input\").canonicalize().expect(\"consumer fixture input exists\");\n",
            "    assert_eq!(actual, expected);\n",
            "    assert_eq!(build_script, expected);\n",
            "}\n",
        ),
    )
    .expect("integration test source");
}

fn run(
    root: &std::path::Path,
    cargo_command: &str,
    backend: &str,
    cache_dir: Option<&std::path::Path>,
) -> Vec<Value> {
    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_REAPI_ACTION_CACHE_TEST_MODE", "1")
        .args(["--backend", backend, "--action-log"])
        .arg(&action_log);
    if let Some(cache_dir) = cache_dir {
        command.arg("--cache-dir").arg(cache_dir);
    }
    let status = command
        .args(["--", cargo_command])
        .env("CARGO_REGISTRY_TOKEN", "do-not-record")
        .status()
        .expect("run cargo-reapi");
    assert!(status.success());
    fs::read_to_string(action_log)
        .expect("action log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect()
}

fn capture(root: &std::path::Path, cargo_command: &str) -> Vec<Value> {
    run(root, cargo_command, "capture", None)
}

fn fixture_action(actions: &[Value]) -> &Value {
    actions
        .iter()
        .find(|action| action["crate_name"] == "capture_fixture")
        .expect("fixture compiler action")
}

fn cache_command(root: &Path, cache_dir: &Path) -> (Command, PathBuf) {
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

#[test]
fn strict_sandbox_uses_the_configured_temporary_directory() {
    let root = tempdir().expect("workspace");
    let cache = tempdir().expect("cache");
    let temporary = tempdir().expect("caller temporary storage");
    write_fixture(root.path(), false);
    let log = root.path().join("target/cargo-reapi/actions.jsonl");
    assert!(
        run_snapshot_gate_with_environment(
            root.path(),
            cache.path(),
            &log,
            &["check"],
            &[("TMPDIR", temporary.path().to_str().unwrap())],
            None,
        )
        .success()
    );
    let control = temporary
        .path()
        .join("cargo-reapi-srt")
        .canonicalize()
        .expect("sandbox control storage beneath caller TMPDIR");
    let policy: Value = serde_json::from_slice(
        &fs::read(root.path().join("target/cargo-reapi/srt-policy-v1.json"))
            .expect("strict policy"),
    )
    .expect("policy JSON");
    assert_eq!(
        policy["network"]["allowUnixSockets"],
        serde_json::json!([control])
    );
    assert!(
        read_actions(&log)
            .iter()
            .any(|action| action["crate_name"] == "capture_fixture")
    );
}

#[test]
fn strict_sandbox_reports_an_overlong_temporary_socket_path() {
    let root = tempdir().expect("workspace");
    let cache = tempdir().expect("cache");
    let temporary = root.path().join("long-temporary-directory-".repeat(5));
    fs::create_dir_all(&temporary).unwrap();
    write_fixture(root.path(), false);
    let (mut command, _) = cache_command(root.path(), cache.path());
    let output = command
        .env("TMPDIR", &temporary)
        .output()
        .expect("run cargo-reapi");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("set TMPDIR to a shorter writable directory")
    );
}

#[test]
fn separate_project_caches_share_the_explicit_ledger_in_strict_sandboxes() {
    let roots = tempdir().expect("project roots");
    let ledger = roots.path().join("shared-physical-actions");
    let ledger_text = ledger.to_str().expect("ledger path");
    let mut projects = Vec::new();
    for name in ["left", "right"] {
        let workspace = roots.path().join(name);
        let cache = roots.path().join(format!("{name}-cache"));
        let log = workspace.join("target/cargo-reapi/actions.jsonl");
        write_fixture(&workspace, false);
        projects.push((workspace, cache, log));
    }
    std::thread::scope(|scope| {
        let workers: Vec<_> = projects
            .iter()
            .map(|(workspace, cache, log)| {
                scope.spawn(move || {
                    assert!(
                        run_snapshot_gate_with_environment(
                            workspace,
                            cache,
                            log,
                            &["check"],
                            &[("CARGO_REAPI_RESOURCE_LEDGER", ledger_text)],
                            None,
                        )
                        .success()
                    );
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("strict Cargo worker");
        }
    });
    assert!(
        fs::read_dir(&ledger)
            .expect("shared ledger")
            .next()
            .is_some()
    );
    let canonical_ledger = ledger.canonicalize().expect("canonical ledger");
    for (workspace, cache, _) in projects {
        assert!(
            !cache.join("resource-ledger-v1").exists(),
            "the driver ignored the external ledger"
        );
        let policy: Value = serde_json::from_slice(
            &fs::read(workspace.join("target/cargo-reapi/srt-policy-v1.json"))
                .expect("strict policy"),
        )
        .expect("policy JSON");
        let allowed = Value::String(canonical_ledger.to_string_lossy().into_owned());
        assert!(
            policy["filesystem"]["allowWrite"]
                .as_array()
                .unwrap()
                .contains(&allowed)
        );
        assert!(
            policy["filesystem"]["allowRead"]
                .as_array()
                .unwrap()
                .contains(&allowed)
        );
    }
}

fn read_actions(action_log: &Path) -> Vec<Value> {
    fs::read_to_string(action_log)
        .expect("action log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect()
}

fn run_snapshot_gate(root: &Path, cache_dir: &Path, action_log: &Path, cargo_args: &[&str]) {
    let status =
        run_snapshot_gate_with_environment(root, cache_dir, action_log, cargo_args, &[], None);
    assert!(status.success());
}

#[test]
fn strict_compiler_actions_use_the_drivers_selected_capacity() {
    let root = tempdir().expect("workspace");
    let cache = tempdir().expect("cache");
    write_fixture(root.path(), false);
    let log = root.path().join("target/cargo-reapi/actions.jsonl");
    assert!(
        run_snapshot_gate_with_environment(
            root.path(),
            cache.path(),
            &log,
            &["check"],
            &[
                ("CARGO_REAPI_RESOURCE_CPU_CAPACITY", "1"),
                ("CARGO_REAPI_RESOURCE_MEMORY_GIB_CAPACITY", "7")
            ],
            None,
        )
        .success()
    );
    let actions = read_actions(&log);
    assert!(!actions.is_empty());
    for action in actions {
        assert_eq!(
            action["resource_capacity"],
            serde_json::json!({"cpu": 1, "memory_gib": 7})
        );
    }
}

fn run_snapshot_gate_with_environment(
    root: &Path,
    cache_dir: &Path,
    action_log: &Path,
    cargo_args: &[&str],
    environment: &[(&str, &str)],
    trace: Option<&Path>,
) -> ExitStatus {
    run_snapshot_gate_with_inputs(
        root,
        cache_dir,
        action_log,
        cargo_args,
        environment,
        trace,
        &[],
    )
}

fn run_snapshot_gate_with_inputs(
    root: &Path,
    cache_dir: &Path,
    action_log: &Path,
    cargo_args: &[&str],
    environment: &[(&str, &str)],
    trace: Option<&Path>,
    declared_inputs: &[&Path],
) -> ExitStatus {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .args(["--backend", "cache", "--action-log"])
        .arg(action_log)
        .arg("--cache-dir")
        .arg(cache_dir);
    for input in declared_inputs {
        command.arg("--declared-input").arg(input);
    }
    command.arg("--").args(cargo_args);
    for (name, value) in environment {
        command.env(name, value);
    }
    if let Some(trace) = trace {
        fs::create_dir_all(trace).expect("rustc observer trace directory");
        let sysroot = Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .expect("query rustc sysroot");
        assert!(sysroot.status.success());
        let real_rustc =
            PathBuf::from(String::from_utf8(sysroot.stdout).unwrap().trim()).join("bin/rustc");
        command
            .env("RUSTC", &real_rustc)
            .env("CARGO_REAPI_REAL_RUSTC", &real_rustc)
            .env("CARGO_REAPI_RUSTC_TRACE", trace);
    }
    command.status().expect("run whole-gate snapshot build")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn write_locked_git_dependency_fixture(root: &Path) -> PathBuf {
    let dependency = root.join("dependency");
    fs::create_dir_all(dependency.join("src")).expect("dependency source directory");
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname='cold-dependency'\nversion='0.1.0'\nedition='2024'\n",
    )
    .expect("dependency manifest");
    fs::write(
        dependency.join("src/lib.rs"),
        "pub fn answer() -> u8 { 42 }\n",
    )
    .expect("dependency source");
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "cargo-reapi@example.invalid"],
        vec!["config", "user.name", "cargo-reapi test"],
        vec!["add", "."],
        vec!["commit", "-qm", "seed dependency"],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&dependency)
            .status()
            .expect("prepare dependency repository");
        assert!(status.success());
    }
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&dependency)
        .output()
        .expect("resolve dependency revision");
    assert!(revision.status.success());
    let revision = String::from_utf8(revision.stdout)
        .expect("utf8 dependency revision")
        .trim()
        .to_owned();

    let consumer = root.join("consumer");
    fs::create_dir_all(consumer.join("src")).expect("consumer source directory");
    let dependency_url = format!("file://{}", dependency.display());
    fs::write(
        consumer.join("Cargo.toml"),
        format!(
            "[package]\nname='cold-consumer'\nversion='0.1.0'\nedition='2024'\n\
             [dependencies]\ncold-dependency={{git='{dependency_url}',rev='{revision}'}}\n"
        ),
    )
    .expect("consumer manifest");
    fs::write(
        consumer.join("src/lib.rs"),
        "pub fn answer() -> u8 { cold_dependency::answer() }\n",
    )
    .expect("consumer source");

    let lock_home = root.join("lock-home");
    fs::create_dir_all(&lock_home).expect("lock cargo home");
    let lock_status = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&consumer)
        .env("CARGO_HOME", &lock_home)
        .status()
        .expect("generate locked dependency graph");
    assert!(lock_status.success());
    consumer
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn strict_snapshot_first_use_provisions_a_locked_git_dependency() {
    let roots = tempdir().expect("first-use roots");
    let consumer = write_locked_git_dependency_fixture(roots.path());

    let offline_home = roots.path().join("offline-home");
    fs::create_dir_all(&offline_home).expect("offline cargo home");
    let offline_home_text = offline_home.to_string_lossy().into_owned();
    let offline_status = run_snapshot_gate_with_environment(
        &consumer,
        &roots.path().join("offline-cache"),
        &roots.path().join("offline-actions.jsonl"),
        &["check", "--locked"],
        &[
            ("CARGO_HOME", offline_home_text.as_str()),
            ("CARGO_NET_OFFLINE", "true"),
        ],
        None,
    );
    assert!(
        !offline_status.success(),
        "cargo-reapi must preserve an explicit caller offline policy"
    );
    assert!(
        !offline_home.join("git/checkouts").exists(),
        "an explicitly offline invocation unexpectedly acquired a dependency"
    );

    let cold_home = roots.path().join("cold-home");
    fs::create_dir_all(&cold_home).expect("cold cargo home");
    let cold_home_text = cold_home.to_string_lossy().into_owned();
    let cache = roots.path().join("cache");
    let action_log = roots.path().join("actions.jsonl");
    let status = run_snapshot_gate_with_environment(
        &consumer,
        &cache,
        &action_log,
        &["check", "--locked"],
        &[("CARGO_HOME", cold_home_text.as_str())],
        None,
    );

    assert!(
        status.success(),
        "a first strict-snapshot invocation must acquire locked sources before entering the network-denied build sandbox"
    );
    assert!(
        cold_home.join("git/checkouts").is_dir(),
        "the invocation did not populate the initially empty Cargo home"
    );
}

fn observed_crates(trace: &Path, worktree: &Path) -> Vec<String> {
    let worktree = fs::canonicalize(worktree).expect("canonical worktree");
    let mut crates = Vec::new();
    for entry in fs::read_dir(trace).expect("rustc trace") {
        let entry = entry.expect("trace entry");
        let record = fs::read_to_string(entry.path()).expect("trace record");
        if !record.lines().any(|line| line == "kind=compile") {
            continue;
        }
        let cwd = record
            .lines()
            .find_map(|line| line.strip_prefix("cwd="))
            .expect("trace cwd");
        if !Path::new(cwd).starts_with(&worktree) {
            continue;
        }
        if !fs::canonicalize(cwd)
            .expect("canonical trace cwd")
            .starts_with(&worktree)
        {
            continue;
        }
        crates.push(
            record
                .lines()
                .find_map(|line| line.strip_prefix("crate_name="))
                .expect("trace crate")
                .to_owned(),
        );
    }
    crates
}

fn write_adversarial_workspace(root: &Path) {
    fs::create_dir_all(root).expect("workspace root");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['leaf','mid','app','unrelated']\nresolver='3'\n",
    )
    .expect("workspace manifest");
    for package in ["leaf", "mid", "app", "unrelated"] {
        fs::create_dir_all(root.join(package).join("src")).expect("package source");
    }
    for (relative, contents) in [
        (
            "leaf/Cargo.toml",
            "[package]\nname='leaf'\nversion='0.0.0'\nedition='2024'\n",
        ),
        ("leaf/src/lib.rs", "pub fn answer() -> u32 { 42 }\n"),
        (
            "mid/Cargo.toml",
            "[package]\nname='mid'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nleaf={path='../leaf'}\n",
        ),
        (
            "mid/src/lib.rs",
            "pub fn answer() -> u32 { leaf::answer() }\n",
        ),
        (
            "app/Cargo.toml",
            "[package]\nname='adversarial-app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nmid={path='../mid'}\n",
        ),
        (
            "app/src/main.rs",
            "fn main() { println!(\"{}\", mid::answer()); }\n",
        ),
        (
            "unrelated/Cargo.toml",
            "[package]\nname='unrelated'\nversion='0.0.0'\nedition='2024'\n",
        ),
        ("unrelated/src/lib.rs", "pub fn value() -> u32 { 7 }\n"),
    ] {
        fs::write(root.join(relative), contents).expect("fixture file");
    }
}

fn copy_adversarial_workspace(source: &Path, destination: &Path) {
    write_adversarial_workspace(destination);
    fs::copy(source.join("Cargo.toml"), destination.join("Cargo.toml"))
        .expect("copy workspace manifest");
    for relative in [
        "leaf/Cargo.toml",
        "leaf/src/lib.rs",
        "mid/Cargo.toml",
        "mid/src/lib.rs",
        "app/Cargo.toml",
        "app/src/main.rs",
        "unrelated/Cargo.toml",
        "unrelated/src/lib.rs",
    ] {
        fs::copy(source.join(relative), destination.join(relative)).expect("copy fixture file");
    }
}

#[test]
fn mutation_rebuilds_only_leaf_and_dependents_under_external_observation() {
    let roots = tempdir().expect("adversarial roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let producer = roots.path().join("producer");
    let consumer = roots.path().join("consumer-with-different-length");
    write_adversarial_workspace(&producer);
    copy_adversarial_workspace(&producer, &consumer);
    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            cache.path(),
            &roots.path().join("producer.jsonl"),
            &["build", "--workspace"],
            &[],
            Some(trace.path()),
        )
        .success()
    );
    let warm_log = roots.path().join("warm.jsonl");
    assert!(
        run_snapshot_gate_with_environment(
            &consumer,
            cache.path(),
            &warm_log,
            &["build", "--workspace"],
            &[],
            Some(trace.path()),
        )
        .success()
    );
    assert!(
        fs::read_to_string(warm_log)
            .unwrap()
            .contains("gate-snapshot-hit")
    );
    fs::write(
        consumer.join("leaf/src/lib.rs"),
        "pub fn answer() -> u32 { 43 }\n",
    )
    .expect("mutate leaf");
    fs::OpenOptions::new()
        .write(true)
        .open(consumer.join("leaf/src/lib.rs"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .expect("backdate changed leaf in the restored target");
    fs::remove_dir_all(&producer).expect("retire producer");
    assert!(
        run_snapshot_gate_with_environment(
            &consumer,
            cache.path(),
            &roots.path().join("consumer.jsonl"),
            &["build", "--workspace"],
            &[],
            Some(trace.path()),
        )
        .success()
    );
    let crates = observed_crates(trace.path(), &consumer)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let producer_actions = read_actions(&roots.path().join("producer.jsonl"));
    let consumer_actions = read_actions(&roots.path().join("consumer.jsonl"));
    let producer_unrelated = producer_actions
        .iter()
        .find(|action| action["crate_name"] == "unrelated")
        .expect("producer unrelated action");
    let consumer_unrelated = consumer_actions
        .iter()
        .find(|action| action["crate_name"] == "unrelated")
        .expect("consumer unrelated action");
    for field in [
        "arguments",
        "keyed_environment",
        "inputs",
        "toolchain",
        "platform",
        "working_directory",
        "output_files",
    ] {
        if producer_unrelated[field] != consumer_unrelated[field] {
            eprintln!(
                "unrelated action differs in {field}:\nproducer={}\nconsumer={}",
                producer_unrelated[field], consumer_unrelated[field]
            );
        }
    }
    assert_eq!(
        crates,
        ["leaf", "mid", "adversarial_app"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        "the independently observed rebuild set must be exact"
    );
    let output = Command::new(consumer.join("target/debug/adversarial-app"))
        .output()
        .expect("run mutated binary");
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "43");
}

fn acceptance_mutation_root() -> PathBuf {
    std::env::var_os("CARGO_REAPI_MUTATION_ROOT")
        .map(PathBuf::from)
        .expect("CARGO_REAPI_MUTATION_ROOT is required")
}

#[test]
#[ignore = "run by the dedicated OS-attribution acceptance runner"]
fn exact_mutation_prepare_for_os_observation() {
    let root = acceptance_mutation_root();
    let producer = root.join("producer");
    let consumer = root.join("consumer-with-different-length");
    let cache = root.join("cache");
    let trace = root.join("producer-wrapper-trace");
    fs::create_dir_all(&root).expect("mutation root");
    fs::create_dir_all(&trace).expect("producer trace");
    write_adversarial_workspace(&producer);
    copy_adversarial_workspace(&producer, &consumer);
    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            &cache,
            &root.join("producer-actions.jsonl"),
            &["build", "--workspace"],
            &[],
            Some(&trace),
        )
        .success()
    );
    fs::write(
        consumer.join("leaf/src/lib.rs"),
        "pub fn answer() -> u32 { 43 }\n",
    )
    .expect("mutate leaf");
    fs::remove_dir_all(&producer).expect("retire producer");
    assert!(!producer.exists());
    fs::write(
        root.join("consumer-root.txt"),
        consumer.display().to_string(),
    )
    .expect("consumer root evidence");
}

#[test]
#[ignore = "run by the dedicated OS-attribution acceptance runner"]
fn exact_mutation_consumer_under_os_observation() {
    let root = acceptance_mutation_root();
    let consumer = root.join("consumer-with-different-length");
    let cache = root.join("cache");
    let trace = root.join("consumer-wrapper-trace");
    fs::create_dir_all(&trace).expect("consumer trace");
    assert!(
        run_snapshot_gate_with_environment(
            &consumer,
            &cache,
            &root.join("consumer-actions.jsonl"),
            &["build", "--workspace"],
            &[],
            Some(&trace),
        )
        .success()
    );
    let crates = observed_crates(&trace, &consumer)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        crates,
        ["leaf", "mid", "adversarial_app"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        "wrapper attribution is a cross-check, not acceptance authority"
    );
    let output = Command::new(consumer.join("target/debug/adversarial-app"))
        .output()
        .expect("run mutated binary");
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "43");
}

#[test]
fn poisoned_dependency_makes_the_restored_gate_say_no() {
    let roots = tempdir().expect("adversarial roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let producer = roots.path().join("producer");
    let consumer = roots.path().join("consumer");
    write_adversarial_workspace(&producer);
    copy_adversarial_workspace(&producer, &consumer);
    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            cache.path(),
            &roots.path().join("producer.jsonl"),
            &["test", "--workspace"],
            &[],
            Some(trace.path()),
        )
        .success()
    );
    fs::write(
        consumer.join("leaf/src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n#[test]\nfn deliberate_poison() { assert!(false, \"deliberate poison\"); }\n",
    )
    .expect("poison dependency");
    fs::remove_dir_all(&producer).expect("retire producer");
    let consumer_log = roots.path().join("consumer.jsonl");
    let status = run_snapshot_gate_with_environment(
        &consumer,
        cache.path(),
        &consumer_log,
        &["test", "--workspace"],
        &[],
        Some(trace.path()),
    );
    assert!(!status.success(), "poisoned gate returned success");
    assert!(
        observed_crates(trace.path(), &consumer)
            .iter()
            .any(|name| name == "leaf")
    );
    let actions = fs::read_to_string(consumer_log).expect("consumer actions");
    assert!(!actions.contains("gate-snapshot-hit"));
}

fn write_flag_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("flag source");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='flag-fixture'\nversion='0.0.0'\nedition='2024'\n[features]\nvariant=[]\n",
    )
    .expect("flag manifest");
    fs::write(
        root.join("src/main.rs"),
        "#![allow(unexpected_cfgs)]\n#[cfg(any(reapi_variant, feature=\"variant\"))] const VALUE: u32 = 43;\n#[cfg(not(any(reapi_variant, feature=\"variant\")))] const VALUE: u32 = 42;\nfn main() { println!(\"{VALUE}:{}\", cfg!(debug_assertions)); }\n",
    )
    .expect("flag source");
}

struct FlagExpectation<'a> {
    root: &'a Path,
    cache: &'a Path,
    trace: &'a Path,
    log: &'a Path,
    cargo_args: &'a [&'a str],
    environment: &'a [(&'a str, &'a str)],
    binary: PathBuf,
    stdout: &'a str,
    require_observed_compile: bool,
}

fn assert_flag_invalidation(expectation: FlagExpectation<'_>) {
    assert!(
        run_snapshot_gate_with_environment(
            expectation.root,
            expectation.cache,
            expectation.log,
            expectation.cargo_args,
            expectation.environment,
            Some(expectation.trace),
        )
        .success()
    );
    if expectation.require_observed_compile {
        assert!(
            observed_crates(expectation.trace, expectation.root)
                .iter()
                .any(|name| name == "flag_fixture")
        );
    }
    let output = Command::new(expectation.binary)
        .output()
        .expect("run invalidated flag fixture");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        expectation.stdout
    );
}

fn run_config_flag_cases(roots: &TempDir, cache: &TempDir, trace: &TempDir) {
    let config_consumer = roots.path().join("config-consumer");
    write_flag_fixture(&config_consumer);
    fs::create_dir_all(config_consumer.join(".cargo")).expect("cargo config directory");
    fs::write(
        config_consumer.join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg', 'reapi_variant']\n",
    )
    .expect("cargo config");
    assert_flag_invalidation(FlagExpectation {
        root: &config_consumer,
        cache: cache.path(),
        trace: trace.path(),
        log: &roots.path().join("config.jsonl"),
        cargo_args: &["build"],
        environment: &[],
        binary: config_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_observed_compile: true,
    });
}

fn run_environment_flag_cases(roots: &TempDir, cache: &TempDir, trace: &TempDir) {
    let environment_consumer = roots.path().join("environment-consumer");
    write_flag_fixture(&environment_consumer);
    assert_flag_invalidation(FlagExpectation {
        root: &environment_consumer,
        cache: cache.path(),
        trace: trace.path(),
        log: &roots.path().join("environment.jsonl"),
        cargo_args: &["build"],
        environment: &[("RUSTFLAGS", "--cfg reapi_variant")],
        binary: environment_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_observed_compile: true,
    });

    let encoded_consumer = roots.path().join("encoded-consumer");
    write_flag_fixture(&encoded_consumer);
    assert_flag_invalidation(FlagExpectation {
        root: &encoded_consumer,
        cache: cache.path(),
        trace: trace.path(),
        log: &roots.path().join("encoded.jsonl"),
        cargo_args: &["build"],
        environment: &[("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}reapi_variant")],
        binary: encoded_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_observed_compile: false,
    });
}

fn run_ancestor_and_cargo_home_flag_cases(roots: &TempDir, cache: &TempDir, trace: &TempDir) {
    let ancestor_root = roots.path().join("ancestor");
    let ancestor_consumer = ancestor_root.join("consumer");
    write_flag_fixture(&ancestor_consumer);
    fs::create_dir_all(ancestor_root.join(".cargo")).expect("ancestor config directory");
    fs::write(
        ancestor_root.join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg', 'reapi_variant']\n",
    )
    .expect("ancestor cargo config");
    assert_flag_invalidation(FlagExpectation {
        root: &ancestor_consumer,
        cache: cache.path(),
        trace: trace.path(),
        log: &roots.path().join("ancestor.jsonl"),
        cargo_args: &["build"],
        environment: &[],
        binary: ancestor_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_observed_compile: false,
    });

    let cargo_home = roots.path().join("cargo-home");
    fs::create_dir_all(&cargo_home).expect("custom cargo home");
    fs::write(
        cargo_home.join("config.toml"),
        "[build]\nrustflags=['--cfg', 'reapi_variant']\n",
    )
    .expect("cargo home config");
    let cargo_home_text = cargo_home.to_string_lossy().to_string();
    let cargo_home_consumer = roots.path().join("cargo-home-consumer");
    write_flag_fixture(&cargo_home_consumer);
    assert_flag_invalidation(FlagExpectation {
        root: &cargo_home_consumer,
        cache: cache.path(),
        trace: trace.path(),
        log: &roots.path().join("cargo-home.jsonl"),
        cargo_args: &["build"],
        environment: &[("CARGO_HOME", cargo_home_text.as_str())],
        binary: cargo_home_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_observed_compile: false,
    });
}

fn run_profile_and_feature_flag_cases(roots: &TempDir, cache: &TempDir, trace: &TempDir) {
    let profile_consumer = roots.path().join("profile-consumer");
    write_flag_fixture(&profile_consumer);
    fs::write(
        profile_consumer.join("Cargo.toml"),
        "[package]\nname='flag-fixture'\nversion='0.0.0'\nedition='2024'\n[features]\nvariant=[]\n[profile.dev]\ndebug-assertions=false\n",
    )
    .expect("profile manifest");
    assert_flag_invalidation(FlagExpectation {
        root: &profile_consumer,
        cache: cache.path(),
        trace: trace.path(),
        log: &roots.path().join("profile.jsonl"),
        cargo_args: &["build"],
        environment: &[],
        binary: profile_consumer.join("target/debug/flag-fixture"),
        stdout: "42:false",
        require_observed_compile: true,
    });

    let feature_consumer = roots.path().join("feature-consumer");
    write_flag_fixture(&feature_consumer);
    assert_flag_invalidation(FlagExpectation {
        root: &feature_consumer,
        cache: cache.path(),
        trace: trace.path(),
        log: &roots.path().join("feature.jsonl"),
        cargo_args: &["build", "--features", "variant"],
        environment: &[],
        binary: feature_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_observed_compile: false,
    });
}

fn run_target_flag_case(roots: &TempDir, cache: &TempDir, trace: &TempDir) {
    let host = Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("query host target");
    let host = String::from_utf8(host.stdout)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .unwrap()
        .to_owned();
    let target_consumer = roots.path().join("target-consumer");
    write_flag_fixture(&target_consumer);
    assert_flag_invalidation(FlagExpectation {
        root: &target_consumer,
        cache: cache.path(),
        trace: trace.path(),
        log: &roots.path().join("target.jsonl"),
        cargo_args: &["build", "--target", &host],
        environment: &[],
        binary: target_consumer
            .join("target")
            .join(&host)
            .join("debug/flag-fixture"),
        stdout: "42:true",
        require_observed_compile: false,
    });
}

#[test]
fn profile_environment_and_cargo_config_flags_all_invalidate() {
    let roots = tempdir().expect("flag roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let producer = roots.path().join("producer");
    write_flag_fixture(&producer);
    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            cache.path(),
            &roots.path().join("producer.jsonl"),
            &["build"],
            &[],
            Some(trace.path()),
        )
        .success()
    );
    run_config_flag_cases(&roots, &cache, &trace);
    run_environment_flag_cases(&roots, &cache, &trace);
    run_ancestor_and_cargo_home_flag_cases(&roots, &cache, &trace);
    run_profile_and_feature_flag_cases(&roots, &cache, &trace);
    run_target_flag_case(&roots, &cache, &trace);
}

fn write_build_script_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("build-script source");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='build-input-fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n",
    )
    .expect("build-script manifest");
    fs::write(
        root.join("build.rs"),
        r#"use std::{env, fs, path::PathBuf};
fn main() {
    let input = env::var("REAPI_EXTERNAL_FILE").unwrap();
    println!("cargo:rerun-if-changed={input}");
    println!("cargo:rerun-if-env-changed=REAPI_BUILD_VALUE");
    let value = fs::read_to_string(&input).unwrap();
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("value.rs");
    fs::write(out, format!("pub const VALUE: u32 = {};", value.trim())).unwrap();
}
"#,
    )
    .expect("build script");
    fs::write(
        root.join("src/main.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/value.rs\"));\nfn main() { println!(\"{VALUE}\"); }\n",
    )
    .expect("build-script application");
}

#[test]
fn declared_external_build_script_input_invalidates_snapshot() {
    let roots = tempdir().expect("build-script roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let external = roots.path().join("external-value.txt");
    fs::write(&external, "42\n").expect("external input");
    let external_text = external.to_string_lossy().to_string();
    let producer = roots.path().join("producer");
    let consumer = roots.path().join("consumer");
    write_build_script_fixture(&producer);
    write_build_script_fixture(&consumer);
    let environment = [
        ("REAPI_EXTERNAL_FILE", external_text.as_str()),
        ("REAPI_BUILD_VALUE", "stable"),
    ];
    assert!(
        run_snapshot_gate_with_inputs(
            &producer,
            cache.path(),
            &roots.path().join("producer.jsonl"),
            &["build"],
            &environment,
            Some(trace.path()),
            &[&external],
        )
        .success()
    );
    fs::write(&external, "43\n").expect("mutate external input");
    fs::remove_dir_all(&producer).expect("retire producer");
    assert!(
        run_snapshot_gate_with_inputs(
            &consumer,
            cache.path(),
            &roots.path().join("consumer.jsonl"),
            &["build"],
            &environment,
            Some(trace.path()),
            &[&external],
        )
        .success()
    );
    let output = Command::new(consumer.join("target/debug/build-input-fixture"))
        .output()
        .expect("run build-input fixture");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "43",
        "consumer actions:\n{}",
        fs::read_to_string(roots.path().join("consumer.jsonl")).unwrap()
    );
    let crates = observed_crates(trace.path(), &consumer);
    assert!(
        crates.iter().any(|name| name == "build_input_fixture"),
        "{crates:?}"
    );
}

#[test]
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn strict_snapshot_rejects_unpinned_sandbox_provider_without_publishing() {
    let workspace = tempdir().expect("provider fixture");
    let cache = tempdir().expect("provider cache");
    let fake_provider = tempdir().expect("fake provider");
    write_fixture(workspace.path(), false);
    fs::write(
        fake_provider.path().join("package.json"),
        r#"{"name":"@anthropic-ai/sandbox-runtime","version":"0.0.65"}"#,
    )
    .expect("fake provider package");
    let fake_srt = fake_provider.path().join("srt");
    fs::write(&fake_srt, "#!/bin/sh\nexit 0\n").expect("fake provider executable");
    #[cfg(unix)]
    fs::set_permissions(&fake_srt, fs::Permissions::from_mode(0o755))
        .expect("fake provider permissions");

    let status = run_snapshot_gate_with_environment(
        workspace.path(),
        cache.path(),
        &workspace.path().join("target/actions.jsonl"),
        &["check"],
        &[("CARGO_REAPI_SRT", fake_srt.to_string_lossy().as_ref())],
        None,
    );
    assert!(
        !status.success(),
        "strict mode accepted an unpinned provider"
    );
    let objects = cache.path().join("gate-snapshots/objects");
    assert!(
        !objects.exists() || fs::read_dir(objects).unwrap().next().is_none(),
        "provider rejection published a snapshot"
    );
}

#[test]
fn undeclared_external_build_script_read_fails_closed_without_publishing() {
    let roots = tempdir().expect("undeclared roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let external = roots.path().join("secret.txt");
    fs::write(&external, "42\n").expect("external input");
    let external_text = external.to_string_lossy().to_string();
    let workspace = roots.path().join("workspace");
    write_build_script_fixture(&workspace);
    let status = run_snapshot_gate_with_environment(
        &workspace,
        cache.path(),
        &workspace.join("target/actions.jsonl"),
        &["build"],
        &[
            ("REAPI_EXTERNAL_FILE", external_text.as_str()),
            ("REAPI_BUILD_VALUE", "stable"),
        ],
        Some(trace.path()),
    );
    assert!(
        !status.success(),
        "undeclared read escaped the strict sandbox"
    );
    let objects = cache.path().join("gate-snapshots/objects");
    assert!(
        !objects.exists() || fs::read_dir(objects).unwrap().next().is_none(),
        "a failed undeclared-input gate published a snapshot"
    );
}

fn write_filesystem_proc_macro_fixture(root: &Path) {
    fs::create_dir_all(root.join("macro/src")).expect("macro source");
    fs::create_dir_all(root.join("app/src")).expect("app source");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['macro','app']\nresolver='3'\n",
    )
    .expect("macro workspace");
    fs::write(
        root.join("macro/Cargo.toml"),
        "[package]\nname='filesystem-macro'\nversion='0.0.0'\nedition='2024'\n[lib]\nproc-macro=true\n",
    )
    .expect("macro manifest");
    fs::write(
        root.join("macro/src/lib.rs"),
        r#"extern crate proc_macro;
use proc_macro::TokenStream;
#[proc_macro]
pub fn external_value(_: TokenStream) -> TokenStream {
    let path = std::env::var("REAPI_PROC_FILE").unwrap();
    std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
}
"#,
    )
    .expect("macro implementation");
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname='filesystem-app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nfilesystem-macro={path='../macro'}\n",
    )
    .expect("macro app manifest");
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", filesystem_macro::external_value!()); }\n",
    )
    .expect("macro app source");
}

#[test]
fn undeclared_proc_macro_filesystem_read_fails_closed() {
    let roots = tempdir().expect("proc macro roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let external = roots.path().join("macro-secret.txt");
    fs::write(&external, "42\n").expect("external macro input");
    let external_text = external.to_string_lossy().to_string();
    let workspace = roots.path().join("workspace");
    write_filesystem_proc_macro_fixture(&workspace);
    let status = run_snapshot_gate_with_environment(
        &workspace,
        cache.path(),
        &workspace.join("target/actions.jsonl"),
        &["build", "--workspace"],
        &[("REAPI_PROC_FILE", external_text.as_str())],
        Some(trace.path()),
    );
    assert!(
        !status.success(),
        "proc macro read an undeclared external file"
    );
}

fn write_network_build_script_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("network fixture source");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='network-input-fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n",
    )
    .expect("network fixture manifest");
    fs::write(
        root.join("build.rs"),
        r#"use std::{env, io::Read, net::TcpStream};
fn main() {
    let mut stream = TcpStream::connect(env::var("REAPI_NETWORK_ADDR").unwrap()).unwrap();
    let mut value = String::new();
    stream.read_to_string(&mut value).unwrap();
    println!("cargo:rustc-env=NETWORK_VALUE={}", value.trim());
}
"#,
    )
    .expect("network build script");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", env!(\"NETWORK_VALUE\")); }\n",
    )
    .expect("network fixture app");
}

#[cfg(target_os = "linux")]
fn write_native_cpp_build_script_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("native fixture source");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='native-cpp-fixture'\nversion='0.0.0'\nedition='2024'\nbuild='build.rs'\n",
    )
    .expect("native fixture manifest");
    fs::write(
        root.join("native.cpp"),
        "extern \"C\" int native_answer() { return 42; }\n",
    )
    .expect("native C++ source");
    fs::write(
        root.join("build.rs"),
        r#"use std::{env, path::PathBuf, process::Command};

fn checked(command: &mut Command, tool: &str) {
    let status = command.status().unwrap_or_else(|error| panic!("failed to execute {tool}: {error}"));
    assert!(status.success(), "{tool} failed with {status}");
}

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let object = out.join("native.o");
    let archive = out.join("libnative_fixture.a");
    checked(Command::new("c++").args(["-c", "native.cpp", "-o"]).arg(&object), "c++");
    checked(Command::new("ar").arg("rcs").arg(&archive).arg(&object), "ar");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=native_fixture");
    println!("cargo:rerun-if-changed=native.cpp");
}
"#,
    )
    .expect("native build script");
    fs::write(
        root.join("src/main.rs"),
        r#"unsafe extern "C" { fn native_answer() -> i32; }
fn main() {
    let answer = unsafe { native_answer() };
    assert_eq!(answer, 42);
    println!("{answer}");
}
"#,
    )
    .expect("native fixture app");
}

#[test]
#[cfg(target_os = "linux")]
fn strict_snapshot_build_script_can_execute_native_cpp_toolchain() {
    let workspace = tempdir().expect("native fixture");
    let cache = tempdir().expect("native fixture cache");
    write_native_cpp_build_script_fixture(workspace.path());

    let host_cpp = Command::new("c++")
        .arg("--version")
        .status()
        .expect("the Linux qualification host must provide c++");
    assert!(host_cpp.success(), "the host c++ toolchain is broken");

    let status = run_snapshot_gate_with_environment(
        workspace.path(),
        cache.path(),
        &workspace.path().join("target/actions.jsonl"),
        &["build"],
        &[],
        None,
    );
    assert!(
        status.success(),
        "strict snapshot hid the host native C++ toolchain from build.rs"
    );

    let output = Command::new(workspace.path().join("target/debug/native-cpp-fixture"))
        .output()
        .expect("execute native fixture");
    assert!(output.status.success(), "restored native fixture failed");
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "42");
}

#[test]
fn deterministic_local_network_input_is_rejected_and_not_published() {
    let roots = tempdir().expect("network roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("local test server");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().unwrap().to_string();
    let workspace = roots.path().join("workspace");
    write_network_build_script_fixture(&workspace);
    let status = run_snapshot_gate_with_environment(
        &workspace,
        cache.path(),
        &workspace.join("target/actions.jsonl"),
        &["build"],
        &[("REAPI_NETWORK_ADDR", address.as_str())],
        Some(trace.path()),
    );
    assert!(!status.success(), "network-dependent build was cacheable");
    assert!(
        listener.accept().is_err(),
        "the sandbox allowed the local network dependency"
    );
    let objects = cache.path().join("gate-snapshots/objects");
    assert!(!objects.exists() || fs::read_dir(objects).unwrap().next().is_none());
}

fn write_environment_proc_macro_fixture(root: &Path) {
    fs::create_dir_all(root.join("macro/src")).expect("macro source");
    fs::create_dir_all(root.join("app/src")).expect("app source");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['macro','app']\nresolver='3'\n",
    )
    .expect("macro workspace");
    fs::write(
        root.join("macro/Cargo.toml"),
        "[package]\nname='environment-macro'\nversion='0.0.0'\nedition='2024'\n[lib]\nproc-macro=true\n",
    )
    .expect("macro manifest");
    fs::write(
        root.join("macro/src/lib.rs"),
        r#"extern crate proc_macro;
use proc_macro::TokenStream;
#[proc_macro]
pub fn environment_value(_: TokenStream) -> TokenStream {
    std::env::var("REAPI_PROC_VALUE").unwrap().parse().unwrap()
}
"#,
    )
    .expect("macro implementation");
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname='environment-app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nenvironment-macro={path='../macro'}\n",
    )
    .expect("macro app manifest");
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", environment_macro::environment_value!()); }\n",
    )
    .expect("macro app source");
}

#[test]
fn proc_macro_environment_change_invalidates_compiler_action() {
    let roots = tempdir().expect("proc macro roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let producer = roots.path().join("producer");
    let consumer = roots.path().join("consumer");
    write_environment_proc_macro_fixture(&producer);
    write_environment_proc_macro_fixture(&consumer);
    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            cache.path(),
            &roots.path().join("producer.jsonl"),
            &["build", "--workspace"],
            &[("REAPI_PROC_VALUE", "42")],
            Some(trace.path()),
        )
        .success()
    );
    fs::remove_dir_all(&producer).expect("retire producer");
    assert!(
        run_snapshot_gate_with_environment(
            &consumer,
            cache.path(),
            &roots.path().join("consumer.jsonl"),
            &["build", "--workspace"],
            &[("REAPI_PROC_VALUE", "43")],
            Some(trace.path()),
        )
        .success()
    );
    assert!(
        observed_crates(trace.path(), &consumer)
            .iter()
            .any(|name| name == "environment_app")
    );
    let output = Command::new(consumer.join("target/debug/environment-app"))
        .output()
        .expect("run proc-macro fixture");
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "43");
}

fn write_external_dependency_app(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("external app source");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='external-app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nshared-dependency={path='../shared-dependency'}\n",
    )
    .expect("external app manifest");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", shared_dependency::answer()); }\n",
    )
    .expect("external app source");
}

#[test]
fn path_dependency_outside_worktree_invalidates_snapshot() {
    let roots = tempdir().expect("external dependency roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let dependency = roots.path().join("shared-dependency");
    fs::create_dir_all(dependency.join("src")).expect("dependency source");
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname='shared-dependency'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("dependency manifest");
    fs::write(
        dependency.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("dependency source");
    let producer = roots.path().join("producer");
    let consumer = roots.path().join("consumer");
    write_external_dependency_app(&producer);
    write_external_dependency_app(&consumer);
    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            cache.path(),
            &roots.path().join("producer.jsonl"),
            &["build"],
            &[],
            Some(trace.path()),
        )
        .success()
    );
    fs::write(
        dependency.join("src/lib.rs"),
        "pub fn answer() -> u32 { 43 }\n",
    )
    .expect("mutate path dependency");
    fs::remove_dir_all(&producer).expect("retire producer");
    assert!(
        run_snapshot_gate_with_environment(
            &consumer,
            cache.path(),
            &roots.path().join("consumer.jsonl"),
            &["build"],
            &[],
            Some(trace.path()),
        )
        .success()
    );
    assert!(
        observed_crates(trace.path(), &consumer)
            .iter()
            .any(|name| name == "external_app")
    );
    let output = Command::new(consumer.join("target/debug/external-app"))
        .output()
        .expect("run external dependency app");
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "43");
}

#[test]
fn identical_cold_gates_have_one_external_producer_and_one_waiter() {
    let roots = tempdir().expect("coalescing roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let first = roots.path().join("first");
    let second = roots.path().join("second-with-different-length");
    write_fixture(&first, true);
    write_fixture(&second, true);
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for (id, worktree) in [("first", first.clone()), ("second", second.clone())] {
        let barrier = Arc::clone(&barrier);
        let cache = cache.path().to_path_buf();
        let trace = trace.path().to_path_buf();
        let action_log = roots.path().join(format!("{id}.jsonl"));
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let status = run_snapshot_gate_with_environment(
                &worktree,
                &cache,
                &action_log,
                &["build"],
                &[],
                Some(&trace),
            );
            (worktree, action_log, status)
        }));
    }
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("coalescing worker"))
        .collect::<Vec<_>>();
    assert!(results.iter().all(|(_, _, status)| status.success()));
    let compile_counts = results
        .iter()
        .map(|(worktree, _, _)| observed_crates(trace.path(), worktree).len())
        .collect::<Vec<_>>();
    assert_eq!(
        compile_counts.iter().filter(|count| **count > 0).count(),
        1,
        "{compile_counts:?}"
    );
    assert_eq!(
        compile_counts.iter().filter(|count| **count == 0).count(),
        1,
        "{compile_counts:?}"
    );
    let logs = results
        .iter()
        .map(|(_, log, _)| fs::read_to_string(log).expect("coalescing log"))
        .collect::<Vec<_>>();
    assert_eq!(
        logs.iter()
            .filter(|log| log.contains("coalesced-gate-hit"))
            .count(),
        1,
        "the waiter must be distinguished from a pre-existing cache hit"
    );
    for (worktree, _, _) in &results {
        let output = Command::new(worktree.join("target/debug/capture-fixture"))
            .output()
            .expect("run coalesced restored binary");
        assert!(output.status.success());
        assert_eq!(
            fs::canonicalize(String::from_utf8(output.stdout).unwrap().trim())
                .expect("coalesced binary embedded path"),
            fs::canonicalize(worktree).expect("coalesced worktree path")
        );
    }
}

#[test]
#[ignore = "run by the dedicated OS-observed coalescing acceptance runner"]
fn exact_coalescing_under_os_observation() {
    let root = std::env::var_os("CARGO_REAPI_COALESCING_ROOT")
        .map(PathBuf::from)
        .expect("CARGO_REAPI_COALESCING_ROOT is required");
    let cache = root.join("cache");
    let trace = root.join("wrapper-trace");
    let first = root.join("first");
    let second = root.join("second-with-different-length");
    fs::create_dir_all(&trace).expect("wrapper trace");
    write_fixture(&first, true);
    write_fixture(&second, true);
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for (id, worktree) in [("first", first.clone()), ("second", second.clone())] {
        let barrier = Arc::clone(&barrier);
        let cache = cache.clone();
        let trace = trace.clone();
        let action_log = root.join(format!("{id}-actions.jsonl"));
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let status = run_snapshot_gate_with_environment(
                &worktree,
                &cache,
                &action_log,
                &["build"],
                &[],
                Some(&trace),
            );
            (id, worktree, action_log, status)
        }));
    }
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("coalescing worker"))
        .collect::<Vec<_>>();
    assert!(results.iter().all(|(_, _, _, status)| status.success()));
    let mut report_members = Vec::new();
    for (id, worktree, action_log, _) in &results {
        let compile_count = observed_crates(&trace, worktree).len();
        let actions = fs::read_to_string(action_log).expect("coalescing actions");
        let coalesced = actions.contains("coalesced-gate-hit");
        let output = Command::new(worktree.join("target/debug/capture-fixture"))
            .output()
            .expect("run coalesced binary");
        assert!(output.status.success());
        assert_eq!(
            fs::canonicalize(String::from_utf8(output.stdout).unwrap().trim()).unwrap(),
            fs::canonicalize(worktree).unwrap()
        );
        report_members.push(serde_json::json!({
            "id": id,
            "worktree": worktree,
            "action_log": action_log,
            "wrapper_compile_count": compile_count,
            "coalesced": coalesced,
            "behavior_passed": true
        }));
    }
    assert_eq!(
        report_members
            .iter()
            .filter(|member| member["wrapper_compile_count"].as_u64().unwrap() > 0)
            .count(),
        1
    );
    assert_eq!(
        report_members
            .iter()
            .filter(|member| member["coalesced"] == true)
            .count(),
        1
    );
    fs::write(
        root.join("coalescing-result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "members": report_members,
            "passed": true
        }))
        .unwrap(),
    )
    .expect("coalescing result");
}

#[test]
fn failing_simultaneous_gates_all_fail_and_publish_nothing() {
    let roots = tempdir().expect("failing coalescing roots");
    let cache = tempdir().expect("shared cache");
    let trace = tempdir().expect("rustc trace");
    let first = roots.path().join("first");
    let second = roots.path().join("second-with-different-length");
    for worktree in [&first, &second] {
        write_fixture(worktree, true);
        fs::write(
            worktree.join("src/main.rs"),
            "compile_error!(\"deliberate coalesced producer failure\");\nfn main() {}\n",
        )
        .expect("poison coalescing fixture");
    }
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for (id, worktree) in [("first", first), ("second", second)] {
        let barrier = Arc::clone(&barrier);
        let cache = cache.path().to_path_buf();
        let trace = trace.path().to_path_buf();
        let action_log = roots.path().join(format!("failing-{id}.jsonl"));
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            run_snapshot_gate_with_environment(
                &worktree,
                &cache,
                &action_log,
                &["build"],
                &[],
                Some(&trace),
            )
        }));
    }
    let statuses = workers
        .into_iter()
        .map(|worker| worker.join().expect("failing coalescing worker"))
        .collect::<Vec<_>>();
    assert!(statuses.iter().all(|status| !status.success()));
    let objects = cache.path().join("gate-snapshots/objects");
    assert!(
        !objects.exists() || fs::read_dir(objects).unwrap().next().is_none(),
        "failed producer published a partial snapshot"
    );
}

#[test]
fn changed_source_in_an_existing_target_cannot_reuse_stale_cargo_freshness() {
    for restore in [false, true] {
        let root = tempdir().expect("workspace");
        let cache = tempdir().expect("cache");
        write_fixture(root.path(), false);
        let target = root.path().join("target");
        let log = target.join("cargo-reapi/actions.jsonl");
        run_snapshot_gate(root.path(), cache.path(), &log, &["check"]);
        if restore {
            fs::remove_dir_all(&target).expect("retire target");
            run_snapshot_gate(root.path(), cache.path(), &log, &["check"]);
            assert!(
                fs::read_to_string(&log)
                    .unwrap()
                    .contains("gate-snapshot-hit")
            );
        }
        let source = root.path().join("src/lib.rs");
        let original = fs::read(&source).expect("original source");
        fs::write(&source, "compile_error!(\"changed source must fail\");\n")
            .expect("poison source");
        // Snapshot timestamps can be newer than the filesystem's next write
        // timestamp. A content-key miss must not trust Cargo's old freshness.
        fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))
            .expect("simulate source timestamp older than cached fingerprints");
        let outcome = run_snapshot_gate_with_environment(
            root.path(),
            cache.path(),
            &log,
            &["check"],
            &[],
            None,
        );
        assert!(
            !outcome.success(),
            "stale target returned success; restored={restore}"
        );
        assert_eq!(
            fs::read_dir(cache.path().join("gate-snapshots/objects"))
                .unwrap()
                .count(),
            1,
            "a failed content state must not publish a gate snapshot"
        );
        fs::write(&source, original).expect("revert failed edit");
        fs::remove_file(&log).expect("retire action log");
        run_snapshot_gate(root.path(), cache.path(), &log, &["check"]);
        let actions = read_actions(&log);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["execution"], "gate-snapshot-hit");
        assert_eq!(
            actions[0]["snapshot_marker_hit"], false,
            "a failed replan must invalidate the old target marker"
        );
    }
}

#[test]
fn whole_gate_snapshot_restores_cargo_state_after_producer_deletion() {
    let roots = tempdir().expect("snapshot worktrees");
    let cache = tempdir().expect("snapshot cache");
    let producer = roots.path().join("p");
    let consumer = roots
        .path()
        .join("consumer-with-a-deliberately-different-path-length");
    write_fixture(&producer, true);
    write_fixture(&consumer, true);
    let producer_log = roots.path().join("producer-actions.jsonl");
    let consumer_log = roots.path().join("consumer-actions.jsonl");
    let producer_trace = roots.path().join("producer-rustc-trace");
    let consumer_trace = roots.path().join("consumer-rustc-trace");

    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            cache.path(),
            &producer_log,
            &["build", "--all-targets"],
            &[],
            Some(&producer_trace),
        )
        .success()
    );
    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            cache.path(),
            &producer_log,
            &["check", "--all-targets"],
            &[],
            Some(&producer_trace),
        )
        .success()
    );
    assert!(producer_log.is_file(), "cold seed must perform actions");
    let consumer_source = consumer.join("src/main.rs");
    let unchanged_source = fs::read(&consumer_source).expect("read consumer source");
    fs::write(&consumer_source, unchanged_source)
        .expect("make consumer source newer than snapshot");
    fs::remove_dir_all(&producer).expect("retire snapshot producer");
    assert!(
        run_snapshot_gate_with_environment(
            &consumer,
            cache.path(),
            &consumer_log,
            &["build", "--all-targets"],
            &[],
            Some(&consumer_trace),
        )
        .success()
    );
    assert!(
        run_snapshot_gate_with_environment(
            &consumer,
            cache.path(),
            &consumer_log,
            &["check", "--all-targets"],
            &[],
            Some(&consumer_trace),
        )
        .success()
    );
    let consumer_actions = read_actions(&consumer_log);
    assert_eq!(consumer_actions.len(), 2);
    assert!(
        consumer_actions
            .iter()
            .all(|action| action["execution"] == "gate-snapshot-hit")
    );
    assert!(
        fs::read_dir(&consumer_trace)
            .expect("consumer rustc trace")
            .next()
            .is_none(),
        "a restored whole-gate hit must not execute rustc, including Cargo metadata probes"
    );

    let output = Command::new(consumer.join("target/debug/capture-fixture"))
        .output()
        .expect("run snapshot-restored executable");
    assert!(
        output.status.success(),
        "snapshot executable failed: status={}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::canonicalize(String::from_utf8(output.stdout).unwrap().trim())
            .expect("embedded manifest path"),
        fs::canonicalize(&consumer).expect("consumer path")
    );
}

#[test]
fn whole_gate_snapshot_restores_across_container_hostnames() {
    let roots = tempdir().expect("hostname worktrees");
    let cache = tempdir().expect("hostname cache");
    let producer = roots.path().join("producer");
    let consumer = roots.path().join("consumer");
    write_fixture(&producer, false);
    write_fixture(&consumer, false);
    let producer_trace = roots.path().join("producer-trace");
    let consumer_trace = roots.path().join("consumer-trace");
    let producer_actions = roots.path().join("producer-actions.jsonl");
    let consumer_actions = roots.path().join("consumer-actions.jsonl");

    assert!(
        run_snapshot_gate_with_environment(
            &producer,
            cache.path(),
            &producer_actions,
            &["check", "--all-targets"],
            &[("HOSTNAME", "cold-container")],
            Some(&producer_trace),
        )
        .success()
    );
    assert!(
        run_snapshot_gate_with_environment(
            &consumer,
            cache.path(),
            &consumer_actions,
            &["check", "--all-targets"],
            &[("HOSTNAME", "warm-container")],
            Some(&consumer_trace),
        )
        .success()
    );

    let actions = read_actions(&consumer_actions);
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["execution"], "gate-snapshot-hit");
    assert_eq!(
        fs::read_dir(&consumer_trace)
            .expect("consumer compiler trace")
            .count(),
        0,
        "cross-container restore executed a physical compiler"
    );
}

#[test]
fn check_clippy_and_test_publish_three_exact_gate_snapshots() {
    let root = tempdir().expect("gate fixture");
    let cache = tempdir().expect("gate cache");
    write_fixture(root.path(), false);
    let action_log = root.path().join("actions.jsonl");
    run_snapshot_gate(root.path(), cache.path(), &action_log, &["check"]);
    run_snapshot_gate(
        root.path(),
        cache.path(),
        &action_log,
        &["clippy", "--", "-D", "warnings"],
    );
    run_snapshot_gate(root.path(), cache.path(), &action_log, &["test"]);
    let objects = fs::read_dir(cache.path().join("gate-snapshots/objects"))
        .expect("snapshot objects")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .count();
    assert_eq!(
        objects, 3,
        "each logical Cargo command needs an exact snapshot"
    );
}

#[test]
fn clippy_preserves_its_nested_wrapper_chain_while_tracing_physical_compilers() {
    let root = tempdir().expect("clippy fixture");
    let cache = tempdir().expect("clippy cache");
    let trace = tempdir().expect("compiler trace");
    write_fixture(root.path(), false);
    let status = run_snapshot_gate_with_environment(
        root.path(),
        cache.path(),
        &root.path().join("actions.jsonl"),
        &["clippy", "--all-targets", "--", "-D", "warnings"],
        &[],
        Some(trace.path()),
    );
    assert!(status.success(), "cargo clippy failed through cargo-reapi");
    assert!(
        observed_crates(trace.path(), root.path())
            .iter()
            .any(|name| name == "capture_fixture"),
        "the physical compiler trace omitted the fixture crate"
    );
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
            .is_some_and(|path| path == "package")
    );
    assert!(actions.iter().all(|action| {
        action["environment"]
            .as_object()
            .is_some_and(|environment| !environment.contains_key("CARGO_REGISTRY_TOKEN"))
    }));
}

#[test]
fn cargo_driver_scrubs_container_hostname_from_builds() {
    let fixture = tempdir().expect("hostname fixture");
    write_fixture(fixture.path(), false);
    fs::write(
        fixture.path().join("src/lib.rs"),
        concat!(
            "pub const BUILD_HOSTNAME: Option<&str> = option_env!(\"HOSTNAME\");\n",
            "#[test]\n",
            "fn hostname_is_not_a_build_input() { assert!(BUILD_HOSTNAME.is_none()); }\n",
        ),
    )
    .expect("hostname fixture source");
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(fixture.path())
        .env_remove("CARGO_TARGET_DIR")
        .env("HOSTNAME", "volatile-container-id")
        .args(["--backend", "capture", "--action-log"])
        .arg(fixture.path().join("actions.jsonl"))
        .args(["--", "test"])
        .status()
        .expect("run hostname fixture");
    assert!(
        status.success(),
        "Cargo build observed the container hostname"
    );
}

#[test]
fn inherited_target_containing_driver_fails_with_actionable_diagnostic() {
    let fixture = tempdir().expect("fixture directory");
    write_fixture(fixture.path(), false);
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_cargo-reapi"))
        .canonicalize()
        .expect("canonical cargo-reapi test executable");
    let target_root = executable
        .parent()
        .and_then(Path::parent)
        .expect("cargo-reapi executable below target root");
    let output = Command::new(&executable)
        .current_dir(fixture.path())
        .env("CARGO_TARGET_DIR", target_root)
        .args(["--backend", "capture", "--action-log"])
        .arg(fixture.path().join("actions.jsonl"))
        .args(["--", "check"])
        .output()
        .expect("run cargo-reapi with leaked target");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("CARGO_TARGET_DIR leaked"), "{stderr}");
    assert!(stderr.contains("Unset CARGO_TARGET_DIR"), "{stderr}");
}

#[test]
fn explicit_external_cargo_target_dir_remains_supported() {
    let fixture = tempdir().expect("fixture directory");
    let external_target = tempdir().expect("external Cargo target directory");
    write_fixture(fixture.path(), false);
    let action_log = fixture.path().join("actions.jsonl");
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(fixture.path())
        .env("CARGO_TARGET_DIR", external_target.path())
        .args([
            "--backend",
            "capture",
            "--snapshot-policy",
            "off",
            "--action-log",
        ])
        .arg(&action_log)
        .args(["--", "check"])
        .status()
        .expect("run cargo-reapi with intentional external target");

    assert!(status.success());
    assert!(external_target.path().join("debug").is_dir());
    assert!(!read_actions(&action_log).is_empty());
}

#[test]
fn identical_projects_in_different_worktrees_have_the_same_action_key() {
    let root = tempdir().expect("fixture parent directory");
    let first = root.path().join("a");
    let second = root.path().join("worktree-with-a-deliberately-longer-name");
    fs::create_dir(&first).expect("first fixture directory");
    fs::create_dir(&second).expect("second fixture directory");
    write_fixture(&first, false);
    write_fixture(&second, false);

    let first_actions = capture(&first, "check");
    let second_actions = capture(&second, "check");
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

#[test]
fn linked_binary_is_durably_cached_and_runs_in_a_later_worktree() {
    let cache = tempdir().expect("shared cache directory");
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    write_fixture(first.path(), true);
    write_fixture(second.path(), true);

    let first_actions = run(first.path(), "build", "cache", Some(cache.path()));
    let first_action = fixture_action(&first_actions).clone();
    drop(first);
    let second_actions = run(second.path(), "build", "cache", Some(cache.path()));
    let second_action = fixture_action(&second_actions);

    assert_eq!(
        first_action["execution"], "local-cache-miss",
        "first linked action: {first_action:#}"
    );
    assert_eq!(second_action["execution"], "cache-hit");
    assert_eq!(first_action["action_key"], second_action["action_key"]);
    assert_eq!(second_action["cache_eligibility"]["eligible"], true);

    let output = Command::new(second.path().join("target/debug/capture-fixture"))
        .output()
        .expect("execute restored binary");
    assert!(output.status.success());
    let embedded_manifest = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        fs::canonicalize(embedded_manifest.trim()).expect("embedded consumer manifest path"),
        fs::canonicalize(second.path()).expect("consumer manifest path")
    );
}

#[test]
fn restored_action_relocates_integration_test_companion_binary_to_consumer_target() {
    let worktrees = tempdir().expect("worktree parent");
    let cache = tempdir().expect("shared cache directory");
    let producer = worktrees.path().join("producer");
    let consumer = worktrees
        .path()
        .join("consumer-with-a-different-path-length");
    fs::create_dir_all(&producer).expect("producer worktree");
    fs::create_dir_all(&consumer).expect("consumer worktree");
    write_companion_binary_fixture(&producer);
    write_companion_binary_fixture(&consumer);

    let producer_actions = run(&producer, "test", "cache", Some(cache.path()));
    let producer_companion = producer_actions
        .iter()
        .find(|action| {
            action["crate_name"] == "companion"
                && !action["arguments"]
                    .as_array()
                    .is_some_and(|arguments| arguments.iter().any(|argument| argument == "--test"))
        })
        .expect("producer companion compiler action");
    assert_eq!(producer_companion["execution"], "local-cache-miss");
    fs::remove_dir_all(&producer).expect("remove producer before consumer");

    let consumer_action_log = consumer.join("target/cargo-reapi/actions.jsonl");
    let consumer_status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(&consumer)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_REAPI_ACTION_CACHE_TEST_MODE", "1")
        .env("CARGO_REGISTRY_TOKEN", "do-not-record")
        .args(["--backend", "cache", "--action-log"])
        .arg(&consumer_action_log)
        .arg("--cache-dir")
        .arg(cache.path())
        .args(["--", "test"])
        .status()
        .expect("run consumer cargo-reapi test");
    let consumer_actions = read_actions(&consumer_action_log);
    let companion_action = consumer_actions
        .iter()
        .find(|action| {
            action["crate_name"] == "companion"
                && !action["arguments"]
                    .as_array()
                    .is_some_and(|arguments| arguments.iter().any(|argument| argument == "--test"))
        })
        .expect("consumer companion compiler action");
    assert!(
        consumer_status.success(),
        "restored integration test could not spawn its companion binary; restored action: {companion_action:#}",
    );
    assert_eq!(
        producer_companion["arguments"], companion_action["arguments"],
        "normalized companion arguments differ"
    );
    assert_eq!(
        producer_companion["keyed_environment"], companion_action["keyed_environment"],
        "normalized companion environment differs"
    );
    assert_eq!(
        producer_companion["inputs"], companion_action["inputs"],
        "normalized companion inputs differ"
    );
    assert_eq!(
        companion_action["execution"], "cache-hit",
        "the fixture must exercise partial action reuse; producer_key={} consumer_key={}",
        producer_companion["action_key"], companion_action["action_key"],
    );
    assert!(
        consumer.join("target/debug/companion").is_file(),
        "consumer target is missing the Cargo-visible companion binary"
    );
}

#[test]
fn restored_proc_macro_signature_does_not_invalidate_its_consumer() {
    let cache = tempdir().expect("shared cache directory");
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    write_proc_macro_fixture(first.path());
    write_proc_macro_fixture(second.path());

    let first_actions = run(first.path(), "build", "cache", Some(cache.path()));
    assert!(
        first_actions
            .iter()
            .any(|action| action["crate_name"] == "identity_macro")
    );
    drop(first);
    let second_actions = run(second.path(), "build", "cache", Some(cache.path()));
    assert!(second_actions.iter().all(|action| {
        matches!(
            action["execution"].as_str(),
            Some("cache-hit" | "local-ineligible")
        )
    }));

    let output = Command::new(second.path().join("target/debug/macro-app"))
        .output()
        .expect("execute restored proc-macro consumer");
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).expect("consumer output");
    let (manifest, answer) = output.trim().rsplit_once(':').expect("output fields");
    assert_eq!(answer, "42");
    assert_eq!(
        fs::canonicalize(manifest).expect("embedded manifest"),
        fs::canonicalize(second.path().join("app")).expect("consumer app path")
    );
}

#[test]
fn restored_rlib_keeps_the_downstream_link_action_key() {
    let cache = tempdir().expect("shared cache directory");
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    write_library_app_fixture(first.path());
    write_library_app_fixture(second.path());

    let first_actions = run(first.path(), "build", "cache", Some(cache.path()));
    let first_app = first_actions
        .iter()
        .find(|action| action["crate_name"] == "relocated_app")
        .expect("first app link action");
    assert_eq!(
        first_app["execution"], "local-cache-miss",
        "first application link action: {first_app:#}"
    );
    let first_key = first_app["action_key"].clone();
    drop(first);

    let second_actions = run(second.path(), "build", "cache", Some(cache.path()));
    let second_app = second_actions
        .iter()
        .find(|action| action["crate_name"] == "relocated_app")
        .expect("second app link action");
    assert_eq!(second_app["action_key"], first_key);
    assert_eq!(second_app["execution"], "cache-hit");
    assert!(second_actions.iter().all(|action| {
        matches!(
            action["execution"].as_str(),
            Some("cache-hit" | "local-ineligible")
        )
    }));

    let output = Command::new(second.path().join("target/debug/relocated-app"))
        .output()
        .expect("execute restored rlib consumer");
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).expect("consumer output");
    let (manifest, answer) = output.trim().rsplit_once(':').expect("output fields");
    assert_eq!(answer, "42");
    assert_eq!(
        fs::canonicalize(manifest).expect("embedded manifest"),
        fs::canonicalize(second.path().join("app")).expect("consumer app path")
    );
}

#[test]
fn shared_cache_restores_identical_action_outputs_across_worktrees() {
    let cache = tempdir().expect("shared cache directory");
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    write_fixture(first.path(), false);
    write_fixture(second.path(), false);

    let first_actions = run(first.path(), "check", "cache", Some(cache.path()));
    let second_actions = run(second.path(), "check", "cache", Some(cache.path()));
    let first_action = fixture_action(&first_actions);
    let second_action = fixture_action(&second_actions);

    assert_eq!(first_action["action_key"], second_action["action_key"]);
    assert_eq!(first_action["execution"], "local-cache-miss");
    assert_eq!(second_action["execution"], "cache-hit");

    let dep_info = fs::read_dir(second.path().join("target/debug/deps"))
        .expect("second dependency output directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().is_some_and(|extension| extension == "d")
                && path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("capture_fixture-"))
        })
        .expect("restored dep-info output");
    let dep_info = fs::read_to_string(dep_info).expect("read restored dep-info");
    assert!(!dep_info.contains(&first.path().to_string_lossy().to_string()));
    assert!(dep_info.contains(&second.path().to_string_lossy().to_string()));
}

#[test]
fn concurrent_identical_actions_execute_once_and_coalesce_on_the_lock() {
    let cache = tempdir().expect("shared cache directory");
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    for fixture in [first.path(), second.path()] {
        write_fixture(fixture, false);
        let mut source = String::new();
        for index in 0..8_000 {
            writeln!(source, "pub fn value_{index}() -> usize {{ {index} }}")
                .expect("write generated fixture source");
        }
        fs::write(fixture.join("src/lib.rs"), source).expect("large fixture source");
    }

    let (mut first_command, first_log) = cache_command(first.path(), cache.path());
    let (mut second_command, second_log) = cache_command(second.path(), cache.path());
    for command in [&mut first_command, &mut second_command] {
        command.env("CARGO_REAPI_ACTION_CACHE_TEST_HOLD_LOCK_MS", "1500");
    }
    let mut first_child = first_command
        .spawn()
        .expect("start first cached Cargo action");
    let mut second_child = second_command
        .spawn()
        .expect("start second cached Cargo action");
    assert!(first_child.wait().expect("wait for first action").success());
    assert!(
        second_child
            .wait()
            .expect("wait for second action")
            .success()
    );

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

#[test]
fn corrupt_cached_blob_is_rejected_and_rebuilt_locally() {
    let cache = tempdir().expect("shared cache directory");
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    let third = tempdir().expect("third fixture directory");
    for fixture in [first.path(), second.path(), third.path()] {
        write_fixture(fixture, false);
    }

    let first_actions = run(first.path(), "check", "cache", Some(cache.path()));
    assert_eq!(
        fixture_action(&first_actions)["execution"],
        "local-cache-miss"
    );
    let blob = fs::read_dir(cache.path().join("blobs"))
        .expect("cache blobs")
        .next()
        .expect("at least one cache blob")
        .expect("cache blob entry")
        .path();
    fs::write(blob, b"corrupt").expect("corrupt cached blob");

    let second_actions = run(second.path(), "check", "cache", Some(cache.path()));
    assert_eq!(
        fixture_action(&second_actions)["execution"],
        "local-cache-miss"
    );
    let third_actions = run(third.path(), "check", "cache", Some(cache.path()));
    assert_eq!(fixture_action(&third_actions)["execution"], "cache-hit");
}

#[test]
fn dependency_package_outside_workspace_is_narrowly_mapped_and_eligible() {
    let fixture = tempdir().expect("fixture directory");
    let app = fixture.path().join("app");
    let dependency = fixture.path().join("external-dependency");
    fs::create_dir_all(app.join("src")).expect("application source directory");
    fs::create_dir_all(dependency.join("src")).expect("dependency source directory");
    fs::write(
        app.join("Cargo.toml"),
        "[package]\nname='app'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nexternal-dependency={path='../external-dependency'}\n",
    )
    .expect("application manifest");
    fs::write(
        app.join("src/lib.rs"),
        "pub use external_dependency::answer;\n",
    )
    .expect("application source");
    fs::write(
        dependency.join("Cargo.toml"),
        "[package]\nname='external-dependency'\nversion='0.1.0'\nedition='2024'\n",
    )
    .expect("dependency manifest");
    fs::write(
        dependency.join("src/lib.rs"),
        "pub fn answer() -> u8 { 42 }\n",
    )
    .expect("dependency source");

    let actions = capture(&app, "check");
    let action = actions
        .iter()
        .find(|action| action["crate_name"] == "external_dependency")
        .expect("external dependency action");
    assert_eq!(action["remote_eligibility"]["eligible"], true);
    assert!(
        action["inputs"]
            .as_array()
            .is_some_and(|inputs| inputs.iter().all(|input| input["path"]
                .as_str()
                .is_some_and(|path| path.starts_with("package/"))))
    );
}

#[cfg(unix)]
#[test]
fn reapi_backend_stages_explicit_inputs_and_materializes_fake_rewrapper_outputs() {
    let fixture = tempdir().expect("fixture directory");
    let tools = tempdir().expect("fake reclient tools");
    let staging = tempdir().expect("reclient staging root");
    write_fixture(fixture.path(), false);
    let rewrapper = tools.path().join("rewrapper");
    let cfg = tools.path().join("rewrapper.cfg");
    let rewrapper_log = tools.path().join("rewrapper.log");
    fs::write(&cfg, "server_address=unix:///tmp/fake-reproxy.sock\n")
        .expect("fake rewrapper config");
    fs::write(
        &rewrapper,
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" >> \"$FAKE_REWRAPPER_LOG\"\nexec_root=\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --exec_root=*) exec_root=${1#--exec_root=} ;;\n    --) shift; break ;;\n  esac\n  shift\ndone\ncd \"$exec_root\"\nexec \"$@\"\n",
    )
    .expect("fake rewrapper");
    fs::set_permissions(&rewrapper, fs::Permissions::from_mode(0o755))
        .expect("executable fake rewrapper");

    let action_log = fixture.path().join("target/cargo-reapi/actions.jsonl");
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(fixture.path())
        .env_remove("CARGO_TARGET_DIR")
        .args(["--backend", "reapi", "--action-log"])
        .arg(&action_log)
        .arg("--rewrapper")
        .arg(&rewrapper)
        .arg("--rewrapper-cfg")
        .arg(&cfg)
        .arg("--reclient-staging-dir")
        .arg(staging.path())
        .args([
            "--reclient-platform",
            "OSFamily={os},Arch={arch},toolchain_sha256={toolchain_sha256}",
        ])
        .args(["--", "check"])
        .env("FAKE_REWRAPPER_LOG", &rewrapper_log)
        .status()
        .expect("run fake REAPI Cargo action");
    assert!(status.success());

    let actions = read_actions(&action_log);
    let action = fixture_action(&actions);
    assert_eq!(action["execution"], "reapi");
    let log = fs::read_to_string(rewrapper_log).expect("fake rewrapper log");
    assert!(log.contains("--inputs="), "{log}");
    assert!(log.contains("Cargo.toml"), "{log}");
    assert!(log.contains("src/lib.rs"), "{log}");
    assert!(log.contains("--output_files=target/"));
    let expected_platform = format!(
        "--platform=OSFamily={},Arch={},toolchain_sha256=",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    assert!(log.contains(&expected_platform), "{log}");
    assert!(!log.contains("{toolchain_sha256}"));
}

#[cfg(unix)]
#[test]
fn reapi_backend_never_sends_link_actions_to_rewrapper() {
    let fixture = tempdir().expect("fixture directory");
    let tools = tempdir().expect("fake reclient tools");
    let staging = tempdir().expect("reclient staging root");
    write_fixture(fixture.path(), true);
    let rewrapper = tools.path().join("rewrapper");
    let cfg = tools.path().join("rewrapper.cfg");
    fs::write(&cfg, "server_address=unix:///tmp/fake-reproxy.sock\n")
        .expect("fake rewrapper config");
    fs::write(&rewrapper, "#!/bin/sh\nexit 99\n").expect("rejecting fake rewrapper");
    fs::set_permissions(&rewrapper, fs::Permissions::from_mode(0o755))
        .expect("executable fake rewrapper");

    let action_log = fixture.path().join("target/cargo-reapi/actions.jsonl");
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(fixture.path())
        .env_remove("CARGO_TARGET_DIR")
        .args(["--backend", "reapi", "--action-log"])
        .arg(&action_log)
        .arg("--rewrapper")
        .arg(&rewrapper)
        .arg("--rewrapper-cfg")
        .arg(&cfg)
        .arg("--reclient-staging-dir")
        .arg(staging.path())
        .args([
            "--reclient-platform",
            "OSFamily={os},Arch={arch},toolchain_sha256={toolchain_sha256}",
        ])
        .args(["--", "build"])
        .status()
        .expect("run link-ineligible REAPI Cargo action");
    assert!(status.success());
    let actions = read_actions(&action_log);
    assert_eq!(fixture_action(&actions)["execution"], "local-ineligible");
}
