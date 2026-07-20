use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::tempdir;

fn copy_fixture(destination: &Path) {
    fs::create_dir_all(destination.join("src")).expect("fixture source directory");
    fs::create_dir_all(destination.join("tests")).expect("fixture tests directory");
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "src/main.rs",
        "tests/runtime.rs",
    ] {
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("acceptance/bevy-fixture")
                .join(relative),
            destination.join(relative),
        )
        .expect("copy pinned Bevy fixture");
    }
}

fn cached_cargo(
    root: &Path,
    cache: &Path,
    log: &Path,
    trace: &Path,
    cargo_args: &[&str],
) -> (Duration, String) {
    let started = Instant::now();
    let real_rustc = Command::new("rustup")
        .args(["which", "rustc"])
        .output()
        .expect("resolve rustc through rustup");
    assert!(real_rustc.status.success());
    let real_rustc = PathBuf::from(String::from_utf8(real_rustc.stdout).unwrap().trim());
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    let status = command
        .current_dir(root)
        .env(
            "RUSTC",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("acceptance/rustc-observer/rustc"),
        )
        .env("CARGO_REAPI_REAL_RUSTC", real_rustc)
        .env("CARGO_REAPI_RUSTC_TRACE", trace)
        .args(["--backend", "cache", "--action-log"])
        .arg(log)
        .arg("--cache-dir")
        .arg(cache)
        .arg("--")
        .args(cargo_args)
        .status()
        .expect("build Bevy fixture");
    assert!(status.success());
    (
        started.elapsed(),
        fs::read_to_string(log).expect("read cached Cargo action log"),
    )
}

fn build_application_and_tests(
    root: &Path,
    cache: &Path,
    trace: &Path,
    label: &str,
) -> (Duration, Vec<String>) {
    let build_log = root.join(format!("target/cargo-reapi/{label}-build.jsonl"));
    let test_log = root.join(format!("target/cargo-reapi/{label}-test.jsonl"));
    let build = cached_cargo(root, cache, &build_log, trace, &["build", "--locked"]);
    let tests = cached_cargo(
        root,
        cache,
        &test_log,
        trace,
        &["test", "--no-run", "--locked"],
    );
    (build.0 + tests.0, vec![build.1, tests.1])
}

fn run_fixture(root: &Path) -> (String, String) {
    let output = Command::new(root.join("target/debug/cargo-reapi-bevy-fixture"))
        .output()
        .expect("run Bevy fixture");
    assert!(output.status.success());
    (
        String::from_utf8(output.stdout).expect("fixture stdout"),
        String::from_utf8(output.stderr).expect("fixture stderr"),
    )
}

fn test_binary(root: &Path) -> std::path::PathBuf {
    let mut candidates = fs::read_dir(root.join("target/debug/deps"))
        .expect("Bevy deps directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("runtime-") && !name.ends_with(".d"))
        })
        .filter(|path| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::metadata(path).is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                path.is_file()
            }
        })
        .collect::<Vec<_>>();
    candidates.sort();
    assert_eq!(
        candidates.len(),
        1,
        "unexpected test binaries: {candidates:?}"
    );
    candidates.pop().unwrap()
}

fn run_test_binary(binary: &Path, arguments: &[&str]) -> Output {
    Command::new(binary)
        .args(arguments)
        .output()
        .expect("run Bevy test binary")
}

fn normalized_test_stdout(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .expect("test stdout")
        .lines()
        .map(|line| {
            line.strip_prefix("test result:")
                .and_then(|result| result.split_once("; finished in "))
                .map_or_else(
                    || line.to_owned(),
                    |(result, _)| format!("test result:{result}"),
                )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn output_record(output: &Output) -> serde_json::Value {
    serde_json::json!({
        "exit_code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr)
    })
}

fn phased_bevy_root() -> PathBuf {
    std::env::var_os("CARGO_REAPI_BEVY_ROOT")
        .map(PathBuf::from)
        .expect("CARGO_REAPI_BEVY_ROOT is required")
}

#[cfg(target_os = "linux")]
fn record_elf_integrity(binary: &Path, label: &str, root: &Path) -> serde_json::Value {
    let mut commands = serde_json::Map::new();
    for (name, program, arguments) in [
        ("readelf_header", "readelf", vec!["-h"]),
        ("readelf_program_headers", "readelf", vec!["-l"]),
        ("readelf_sections", "readelf", vec!["-S"]),
        ("readelf_debug", "readelf", vec!["--debug-dump=decodedline"]),
        ("objdump_headers", "objdump", vec!["-h"]),
        ("ldd", "ldd", Vec::new()),
        ("elflint", "eu-elflint", vec!["--gnu-ld"]),
        ("strings", "strings", Vec::new()),
    ] {
        let output = Command::new(program)
            .args(arguments)
            .arg(binary)
            .output()
            .unwrap_or_else(|error| panic!("run {program} for {label}: {error}"));
        let path = root.join(format!("{label}-{name}.txt"));
        fs::write(
            &path,
            [output.stdout.as_slice(), output.stderr.as_slice()].concat(),
        )
        .expect("write ELF evidence");
        commands.insert(
            name.to_owned(),
            serde_json::json!({"exit_code": output.status.code(), "evidence": path}),
        );
    }
    serde_json::Value::Object(commands)
}

#[cfg(not(target_os = "linux"))]
fn record_elf_integrity(_binary: &Path, _label: &str, _root: &Path) -> serde_json::Value {
    serde_json::json!({})
}

#[test]
#[ignore = "run by the phased Bevy acceptance runner"]
fn bevy_phase_prepare_deleted_producer() {
    let root = phased_bevy_root();
    let producer = root.join("producer");
    let consumer = root.join("consumer-with-a-deliberately-different-path-length");
    let cache = root.join("cache");
    let trace = root.join("producer-wrapper-trace");
    fs::create_dir_all(&trace).expect("producer trace");
    copy_fixture(&producer);
    copy_fixture(&consumer);
    let (_, logs) = build_application_and_tests(&producer, &cache, &trace, "producer");
    assert!(logs.iter().all(|log| !log.contains("gate-snapshot-hit")));
    let producer_behavior = run_fixture(&producer);
    let producer_test = test_binary(&producer);
    let producer_test_list = run_test_binary(&producer_test, &["--list"]);
    let producer_test_behavior = run_test_binary(&producer_test, &["--nocapture"]);
    assert!(producer_test_list.status.success() && producer_test_behavior.status.success());
    verify_signature(&producer.join("target/debug/cargo-reapi-bevy-fixture"));
    verify_signature(&producer_test);
    fs::write(
        root.join("producer-result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "application":{"stdout":producer_behavior.0,"stderr":producer_behavior.1},
            "test_list":output_record(&producer_test_list),
            "test_behavior":output_record(&producer_test_behavior),
            "passed":true
        }))
        .unwrap(),
    )
    .expect("producer result");
    fs::remove_dir_all(&producer).expect("delete producer");
    assert!(!producer.exists());
}

#[test]
#[ignore = "run by the phased Bevy acceptance runner"]
fn bevy_phase_restore_under_os_observation() {
    let root = phased_bevy_root();
    let consumer = root.join("consumer-with-a-deliberately-different-path-length");
    let cache = root.join("cache");
    let trace = root.join("consumer-wrapper-trace");
    fs::create_dir_all(&trace).expect("consumer trace");
    let (elapsed, logs) = build_application_and_tests(&consumer, &cache, &trace, "consumer");
    assert!(elapsed <= Duration::from_secs(60));
    assert_eq!(observed_compiler_actions(&trace, &consumer), 0);
    for log in &logs {
        let actions = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["execution"], "gate-snapshot-hit");
    }
    let application = run_fixture(&consumer);
    let (embedded_path, answer) = application.0.trim().rsplit_once(':').unwrap();
    assert_eq!(answer, "42");
    assert_eq!(
        fs::canonicalize(embedded_path).unwrap(),
        fs::canonicalize(&consumer).unwrap()
    );
    let test = test_binary(&consumer);
    let test_list = run_test_binary(&test, &["--list"]);
    let test_behavior = run_test_binary(&test, &["--nocapture"]);
    assert!(test_list.status.success() && test_behavior.status.success());
    verify_signature(&consumer.join("target/debug/cargo-reapi-bevy-fixture"));
    verify_signature(&test);
    let elf = serde_json::json!({
        "application":record_elf_integrity(&consumer.join("target/debug/cargo-reapi-bevy-fixture"),"restored-application",&root),
        "test":record_elf_integrity(&test,"restored-test",&root)
    });
    fs::write(
        root.join("restored-result.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "warm_elapsed_ms":elapsed.as_millis(),
            "wrapper_compile_events":0,
            "application":{"stdout":application.0,"stderr":application.1},
            "test_list":output_record(&test_list),
            "test_behavior":{"exit_code":test_behavior.status.code(),"stdout":normalized_test_stdout(&test_behavior.stdout),"stderr":String::from_utf8_lossy(&test_behavior.stderr)},
            "action_logs":logs,
            "platform_signatures_valid":true,
            "elf":elf,
            "passed":true
        }))
        .unwrap(),
    )
    .expect("restored result");
}

#[test]
#[ignore = "run by the phased Bevy acceptance runner"]
fn bevy_phase_fresh_control_and_compare() {
    let root = phased_bevy_root();
    let consumer = root.join("consumer-with-a-deliberately-different-path-length");
    let restored: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("restored-result.json")).unwrap()).unwrap();
    fs::remove_dir_all(consumer.join("target")).expect("remove restored target");
    fresh_control(&consumer);
    let application = run_fixture(&consumer);
    let test = test_binary(&consumer);
    let test_list = run_test_binary(&test, &["--list"]);
    let test_behavior = run_test_binary(&test, &["--nocapture"]);
    verify_signature(&consumer.join("target/debug/cargo-reapi-bevy-fixture"));
    verify_signature(&test);
    let fresh = serde_json::json!({
        "application":{"stdout":application.0,"stderr":application.1},
        "test_list":output_record(&test_list),
        "test_behavior":{"exit_code":test_behavior.status.code(),"stdout":normalized_test_stdout(&test_behavior.stdout),"stderr":String::from_utf8_lossy(&test_behavior.stderr)},
        "platform_signatures_valid":true,
        "elf":{
            "application":record_elf_integrity(&consumer.join("target/debug/cargo-reapi-bevy-fixture"),"fresh-application",&root),
            "test":record_elf_integrity(&test,"fresh-test",&root)
        }
    });
    assert_eq!(restored["application"], fresh["application"]);
    assert_eq!(restored["test_list"], fresh["test_list"]);
    assert_eq!(restored["test_behavior"], fresh["test_behavior"]);
    #[cfg(target_os = "linux")]
    for binary in ["application", "test"] {
        for check in [
            "readelf_header",
            "readelf_program_headers",
            "readelf_sections",
            "readelf_debug",
            "objdump_headers",
            "ldd",
        ] {
            assert_eq!(restored["elf"][binary][check]["exit_code"], 0);
            assert_eq!(fresh["elf"][binary][check]["exit_code"], 0);
        }
        assert_eq!(
            restored["elf"][binary]["elflint"]["exit_code"],
            fresh["elf"][binary]["elflint"]["exit_code"]
        );
        let strings_path = restored["elf"][binary]["strings"]["evidence"]
            .as_str()
            .unwrap();
        let strings = fs::read_to_string(strings_path).expect("restored ELF strings evidence");
        assert!(!strings.contains("/producer"));
        assert!(strings.contains(&consumer.display().to_string()));
    }
    fs::write(
        root.join("bevy-integrity.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version":2,
            "restored":restored,
            "fresh":fresh,
            "application_parity":true,
            "test_parity":true,
            "consumer_paths_only":true,
            "passed":true
        }))
        .unwrap(),
    )
    .expect("Bevy integrity report");
}

fn fresh_control(root: &Path) {
    for arguments in [
        &["build", "--locked"][..],
        &["test", "--no-run", "--locked"][..],
    ] {
        let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
            .current_dir(root)
            .env("CARGO_NET_OFFLINE", "true")
            .args(["--backend", "local", "--snapshot-policy", "off", "--"])
            .args(arguments)
            .status()
            .expect("build fresh Bevy control");
        assert!(status.success());
    }
}

fn observed_compiler_actions(trace: &Path, root: &Path) -> usize {
    fs::read_dir(trace)
        .expect("rustc trace")
        .filter_map(Result::ok)
        .filter(|entry| {
            fs::read_to_string(entry.path())
                .is_ok_and(|record| record.lines().any(|line| line == "kind=compile"))
        })
        .filter(|entry| {
            fs::read_to_string(entry.path()).is_ok_and(|record| {
                record.lines().any(|line| {
                    line.strip_prefix("cwd=")
                        .is_some_and(|cwd| Path::new(cwd).starts_with(root))
                })
            })
        })
        .count()
}

#[cfg(target_os = "macos")]
fn verify_signature(executable: &Path) {
    assert!(
        Command::new("codesign")
            .args(["--verify", "--strict"])
            .arg(executable)
            .status()
            .expect("verify executable signature")
            .success()
    );
}

#[cfg(not(target_os = "macos"))]
fn verify_signature(_executable: &Path) {}

#[cfg(target_os = "macos")]
struct OsCompilerObserver {
    child: Child,
    events: PathBuf,
    proof: PathBuf,
}

#[cfg(target_os = "macos")]
fn start_os_compiler_observer(root: &Path) -> OsCompilerObserver {
    let events = root.join("warm-os-events.jsonl");
    let proof = root.join("warm-os-proof.json");
    let stdout = fs::File::create(&events).expect("create warm eslogger evidence");
    let stderr =
        fs::File::create(root.join("warm-os-events.stderr")).expect("create warm eslogger stderr");
    let child = Command::new("perl")
        .args(["-MPOSIX=setsid", "-e", "setsid(); exec @ARGV"])
        .args([
            "sudo",
            "-n",
            "/usr/bin/eslogger",
            "--format",
            "json",
            "exec",
        ])
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .expect("start warm eslogger observer");
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        Command::new("kill")
            .args(["-0", &child.id().to_string()])
            .status()
            .expect("probe warm eslogger")
            .success(),
        "warm eslogger exited before the restored consumer"
    );
    OsCompilerObserver {
        child,
        events,
        proof,
    }
}

#[cfg(target_os = "macos")]
fn stop_os_compiler_observer(mut observer: OsCompilerObserver) -> PathBuf {
    Command::new("kill")
        .args(["-TERM", &observer.child.id().to_string()])
        .status()
        .expect("stop warm eslogger");
    observer.child.wait().expect("wait for warm eslogger");
    let rustc = Command::new("rustup")
        .args(["which", "rustc"])
        .output()
        .expect("resolve observed rustc");
    let clang = Command::new("/usr/bin/xcrun")
        .args(["--find", "clang"])
        .output()
        .expect("resolve observed clang");
    assert!(rustc.status.success() && clang.status.success());
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi-auditor"))
        .args(["eslog", "--events"])
        .arg(&observer.events)
        .arg("--select")
        .arg(String::from_utf8(rustc.stdout).unwrap().trim())
        .arg("--select")
        .arg(String::from_utf8(clang.stdout).unwrap().trim())
        .args(["--expected", "zero", "--report"])
        .arg(&observer.proof)
        .status()
        .expect("audit warm eslogger evidence");
    assert!(status.success(), "warm OS compiler/linker proof failed");
    observer.proof
}

#[cfg(not(target_os = "macos"))]
fn start_os_compiler_observer(_root: &Path) {}

#[cfg(not(target_os = "macos"))]
fn stop_os_compiler_observer(_observer: ()) -> PathBuf {
    PathBuf::new()
}

#[test]
#[ignore = "explicit pinned-Bevy acceptance proof"]
fn bevy_linked_artifact_restores_after_producer_deletion() {
    let acceptance_report = std::env::var_os("CARGO_REAPI_ACCEPTANCE_REPORT").map(PathBuf::from);
    let cache = tempdir().expect("shared cache");
    let worktrees = tempdir().expect("worktree parent");
    let trace = tempdir().expect("external rustc trace");
    let producer = worktrees.path().join("p");
    let consumer = worktrees
        .path()
        .join("consumer-with-a-deliberately-different-path-length");
    copy_fixture(&producer);
    copy_fixture(&consumer);

    let (_, producer_logs) =
        build_application_and_tests(&producer, cache.path(), trace.path(), "producer");
    assert!(
        producer_logs
            .iter()
            .all(|log| !log.contains("gate-snapshot-hit"))
    );
    let producer_behavior = run_fixture(&producer);
    let producer_test = test_binary(&producer);
    let producer_test_list = run_test_binary(&producer_test, &["--list"]);
    let producer_test_behavior = run_test_binary(&producer_test, &["--nocapture"]);
    assert!(producer_test_list.status.success());
    assert!(producer_test_behavior.status.success());
    verify_signature(&producer.join("target/debug/cargo-reapi-bevy-fixture"));
    verify_signature(&producer_test);
    fs::remove_dir_all(&producer).expect("delete producer before consumer");
    let observer_root = acceptance_report
        .as_ref()
        .and_then(|report| report.parent())
        .unwrap_or_else(|| worktrees.path());
    fs::create_dir_all(observer_root).expect("OS observer evidence directory");
    let os_observer = start_os_compiler_observer(observer_root);
    let (warm_elapsed, consumer_logs) =
        build_application_and_tests(&consumer, cache.path(), trace.path(), "consumer");
    let os_proof = stop_os_compiler_observer(os_observer);
    assert!(
        warm_elapsed <= Duration::from_secs(60),
        "pinned Bevy whole-gate restore took {warm_elapsed:?}; contract allows 60s"
    );
    for actions in &consumer_logs {
        let actions = actions
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("consumer action"))
            .collect::<Vec<_>>();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0]["execution"], "gate-snapshot-hit");
    }
    assert_eq!(observed_compiler_actions(trace.path(), &consumer), 0);

    let consumer_behavior = run_fixture(&consumer);
    let consumer_test = test_binary(&consumer);
    let restored_test_list = run_test_binary(&consumer_test, &["--list"]);
    let restored_test_behavior = run_test_binary(&consumer_test, &["--nocapture"]);
    assert!(restored_test_list.status.success());
    assert!(restored_test_behavior.status.success());
    assert_eq!(producer_behavior.1, consumer_behavior.1);
    let output = &consumer_behavior.0;
    let (embedded_path, answer) = output
        .trim()
        .rsplit_once(':')
        .expect("fixture output fields");
    assert_eq!(answer, "42");
    assert_eq!(
        fs::canonicalize(embedded_path).expect("embedded consumer path"),
        fs::canonicalize(&consumer).expect("consumer path")
    );
    let producer_answer = producer_behavior
        .0
        .trim()
        .rsplit_once(':')
        .expect("producer output fields")
        .1;
    assert_eq!(producer_answer, answer);
    verify_signature(&consumer.join("target/debug/cargo-reapi-bevy-fixture"));
    verify_signature(&consumer_test);

    fs::remove_dir_all(consumer.join("target")).expect("remove restored target before control");
    fresh_control(&consumer);
    let fresh_behavior = run_fixture(&consumer);
    let fresh_test = test_binary(&consumer);
    let fresh_test_list = run_test_binary(&fresh_test, &["--list"]);
    let fresh_test_behavior = run_test_binary(&fresh_test, &["--nocapture"]);
    assert_eq!(consumer_behavior, fresh_behavior);
    assert_eq!(restored_test_list.status, fresh_test_list.status);
    assert_eq!(restored_test_list.stdout, fresh_test_list.stdout);
    assert_eq!(restored_test_list.stderr, fresh_test_list.stderr);
    assert_eq!(restored_test_behavior.status, fresh_test_behavior.status);
    assert_eq!(
        normalized_test_stdout(&restored_test_behavior.stdout),
        normalized_test_stdout(&fresh_test_behavior.stdout)
    );
    assert_eq!(restored_test_behavior.stderr, fresh_test_behavior.stderr);
    assert_eq!(producer_test_list.stdout, fresh_test_list.stdout);
    verify_signature(&consumer.join("target/debug/cargo-reapi-bevy-fixture"));
    verify_signature(&fresh_test);

    if let Some(report) = acceptance_report {
        if let Some(parent) = report.parent() {
            fs::create_dir_all(parent).expect("Bevy acceptance report directory");
        }
        fs::write(
            &report,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1,
                "kind": "bevy-integrity",
                "warm_elapsed_ms": warm_elapsed.as_millis(),
                "consumer_wrapper_compile_events": observed_compiler_actions(trace.path(), &consumer),
                "os_compiler_linker_events": 0,
                "os_proof": os_proof,
                "producer_application": {"stdout": producer_behavior.0, "stderr": producer_behavior.1},
                "restored_application": {"stdout": consumer_behavior.0, "stderr": consumer_behavior.1},
                "fresh_application": {"stdout": fresh_behavior.0, "stderr": fresh_behavior.1},
                "restored_test_list": String::from_utf8(restored_test_list.stdout).unwrap(),
                "fresh_test_list": String::from_utf8(fresh_test_list.stdout).unwrap(),
                "restored_test_stdout": normalized_test_stdout(&restored_test_behavior.stdout),
                "fresh_test_stdout": normalized_test_stdout(&fresh_test_behavior.stdout),
                "restored_test_stderr": String::from_utf8(restored_test_behavior.stderr).unwrap(),
                "fresh_test_stderr": String::from_utf8(fresh_test_behavior.stderr).unwrap(),
                "consumer_action_logs": consumer_logs,
                "restored_signatures_valid": true,
                "fresh_signatures_valid": true,
                "passed": true
            }))
            .expect("serialize Bevy acceptance report"),
        )
        .expect("write Bevy acceptance report");
    }
}
