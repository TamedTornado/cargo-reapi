use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
    assert!(executions.contains(&"cache-hit"));
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
