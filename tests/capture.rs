use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

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

fn run_snapshot_gate(root: &Path, cache_dir: &Path, action_log: &Path, cargo_args: &[&str]) {
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(root)
        .args(["--backend", "cache", "--action-log"])
        .arg(action_log)
        .arg("--cache-dir")
        .arg(cache_dir)
        .arg("--")
        .args(cargo_args)
        .status()
        .expect("run whole-gate snapshot build");
    assert!(status.success());
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

    run_snapshot_gate(
        &producer,
        cache.path(),
        &producer_log,
        &["build", "--all-targets"],
    );
    run_snapshot_gate(
        &producer,
        cache.path(),
        &producer_log,
        &["check", "--all-targets"],
    );
    assert!(producer_log.is_file(), "cold seed must perform actions");
    let consumer_source = consumer.join("src/main.rs");
    let unchanged_source = fs::read(&consumer_source).expect("read consumer source");
    fs::write(&consumer_source, unchanged_source)
        .expect("make consumer source newer than snapshot");
    fs::remove_dir_all(&producer).expect("retire snapshot producer");
    run_snapshot_gate(
        &consumer,
        cache.path(),
        &consumer_log,
        &["build", "--all-targets"],
    );
    run_snapshot_gate(
        &consumer,
        cache.path(),
        &consumer_log,
        &["check", "--all-targets"],
    );
    let consumer_actions = read_actions(&consumer_log);
    assert_eq!(consumer_actions.len(), 2);
    assert!(
        consumer_actions
            .iter()
            .all(|action| action["execution"] == "gate-snapshot-hit")
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

    assert_eq!(first_action["execution"], "local-cache-miss");
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
    assert!(log.contains("--platform=OSFamily=macos,Arch=aarch64,toolchain_sha256="));
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
