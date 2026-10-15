use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

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
    // The paths are compiled into the library, which a later worktree
    // restores from the cache, so they must be relocated there.
    fs::write(
        root.join("src/lib.rs"),
        concat!(
            "pub fn companion_data() -> &'static str { env!(\"COMPANION_DATA\") }\n",
            "pub fn build_script_data() -> &'static str { env!(\"BUILD_SCRIPT_DATA\") }\n",
        ),
    )
    .expect("library source");
    fs::write(
        root.join("src/bin/companion.rs"),
        concat!(
            "fn main() {\n",
            "    println!(\"companion-ok\");\n",
            "    println!(\"{}\", companion_fixture::companion_data());\n",
            "    println!(\"{}\", companion_fixture::build_script_data());\n",
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

fn read_actions(action_log: &Path) -> Vec<Value> {
    fs::read_to_string(action_log)
        .expect("action log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect()
}

/// Runs Cargo through the cache backend, logging every compile to
/// `action_log`.
fn cached_build(
    root: &Path,
    cache_dir: &Path,
    action_log: &Path,
    cargo_args: &[&str],
    environment: &[(&str, &str)],
) -> ExitStatus {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .args(["--backend", "cache", "--action-log"])
        .arg(action_log)
        .arg("--cache-dir")
        .arg(cache_dir)
        .arg("--")
        .args(cargo_args);
    for (name, value) in environment {
        command.env(name, value);
    }
    command.status().expect("run cargo-reapi")
}

/// Whether a logged compile was served from the cache.
fn is_hit(action: &Value) -> bool {
    matches!(
        action["execution"].as_str(),
        Some("cache-hit" | "coalesced-hit")
    )
}

/// Whether a logged compile is a library compile (an rlib or its metadata).
fn is_library_compile(action: &Value) -> bool {
    action["arguments"].as_array().is_some_and(|arguments| {
        arguments
            .windows(2)
            .any(|pair| pair[0] == "--crate-type" && pair[1] == "lib")
    })
}

/// The crates whose compiles ran the compiler rather than being served
/// from the cache. Cargo's control probes are left out: they name no crate,
/// or the crate `___` when Cargo asks rustc about the target.
fn executed_crates(actions: &[Value]) -> std::collections::BTreeSet<String> {
    actions
        .iter()
        .filter(|action| !is_hit(action))
        .filter_map(|action| action["crate_name"].as_str().map(str::to_owned))
        .filter(|crate_name| crate_name != "___")
        .collect()
}

/// The executions logged for `crate_name`'s library compile.
fn library_executions(actions: &[Value], crate_name: &str) -> Vec<String> {
    actions
        .iter()
        .filter(|action| action["crate_name"] == crate_name && is_library_compile(action))
        .map(|action| action["execution"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn run_program(path: &Path) -> String {
    let output = Command::new(path)
        .output()
        .unwrap_or_else(|error| panic!("run {}: {error}", path.display()));
    assert!(output.status.success(), "{} failed", path.display());
    String::from_utf8(output.stdout)
        .expect("UTF-8 output")
        .trim()
        .to_owned()
}

#[test]
fn compiler_actions_use_the_drivers_selected_capacity() {
    let root = tempdir().expect("workspace");
    let cache = tempdir().expect("cache");
    write_fixture(root.path(), false);
    let log = root.path().join("target/cargo-reapi/actions.jsonl");

    let status = cached_build(
        root.path(),
        cache.path(),
        &log,
        &["check"],
        &[
            ("CARGO_REAPI_RESOURCE_CPU_CAPACITY", "1"),
            ("CARGO_REAPI_RESOURCE_MEMORY_GIB_CAPACITY", "7"),
        ],
    );

    assert!(status.success());
    let actions = read_actions(&log);
    assert!(!actions.is_empty());
    for action in actions {
        assert_eq!(
            action["resource_capacity"],
            serde_json::json!({"cpu": 1, "memory_gib": 7})
        );
    }
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

#[test]
fn mutation_rebuilds_only_leaf_and_dependents() {
    let roots = tempdir().expect("adversarial roots");
    let cache = tempdir().expect("shared cache");
    let producer = roots.path().join("producer");
    let consumer = roots.path().join("consumer-with-different-length");
    write_adversarial_workspace(&producer);
    write_adversarial_workspace(&consumer);
    let build = ["build", "--workspace"];

    let producer_log = roots.path().join("producer.jsonl");
    assert!(cached_build(&producer, cache.path(), &producer_log, &build, &[]).success());
    fs::remove_dir_all(&producer).expect("retire producer");

    // Every library compile is restored; only the binary's link step runs.
    let warm_log = roots.path().join("warm.jsonl");
    assert!(cached_build(&consumer, cache.path(), &warm_log, &build, &[]).success());
    assert_eq!(
        executed_crates(&read_actions(&warm_log)),
        ["adversarial_app"].map(str::to_owned).into()
    );

    fs::write(
        consumer.join("leaf/src/lib.rs"),
        "pub fn answer() -> u32 { 43 }\n",
    )
    .expect("mutate leaf");
    let mutated_log = roots.path().join("mutated.jsonl");
    assert!(cached_build(&consumer, cache.path(), &mutated_log, &build, &[]).success());

    // Cargo finds the restored artifacts of the unrelated crate fresh.
    assert_eq!(
        executed_crates(&read_actions(&mutated_log)),
        ["adversarial_app", "leaf", "mid"].map(str::to_owned).into()
    );
    assert_eq!(
        run_program(&consumer.join("target/debug/adversarial-app")),
        "43"
    );
}

/// A package whose library holds a value chosen by `cfg`, a feature and the
/// profile, and whose binary prints it.
fn write_flag_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("flag source");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='flag-fixture'\nversion='0.0.0'\nedition='2024'\n[features]\nvariant=[]\n",
    )
    .expect("flag manifest");
    fs::write(
        root.join("src/lib.rs"),
        "#![allow(unexpected_cfgs)]\n#[cfg(any(reapi_variant, feature=\"variant\"))] pub const VALUE: u32 = 43;\n#[cfg(not(any(reapi_variant, feature=\"variant\")))] pub const VALUE: u32 = 42;\npub const DEBUG: bool = cfg!(debug_assertions);\n",
    )
    .expect("flag library");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}:{}\", flag_fixture::VALUE, flag_fixture::DEBUG); }\n",
    )
    .expect("flag binary");
}

struct FlagExpectation<'a> {
    root: &'a Path,
    cache: &'a Path,
    log: &'a Path,
    cargo_args: &'a [&'a str],
    environment: &'a [(&'a str, &'a str)],
    binary: PathBuf,
    stdout: &'a str,
    /// Whether no earlier case compiled the library the same way, so this
    /// one must run its compiler.
    require_library_compile: bool,
}

fn assert_flag_invalidation(expectation: &FlagExpectation<'_>) {
    let status = cached_build(
        expectation.root,
        expectation.cache,
        expectation.log,
        expectation.cargo_args,
        expectation.environment,
    );

    assert!(status.success());
    if expectation.require_library_compile {
        assert_eq!(
            library_executions(&read_actions(expectation.log), "flag_fixture"),
            ["local-cache-miss"]
        );
    }
    assert_eq!(run_program(&expectation.binary), expectation.stdout);
}

fn run_config_flag_cases(roots: &TempDir, cache: &TempDir) {
    let config_consumer = roots.path().join("config-consumer");
    write_flag_fixture(&config_consumer);
    fs::create_dir_all(config_consumer.join(".cargo")).expect("cargo config directory");
    fs::write(
        config_consumer.join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg', 'reapi_variant']\n",
    )
    .expect("cargo config");
    assert_flag_invalidation(&FlagExpectation {
        root: &config_consumer,
        cache: cache.path(),
        log: &roots.path().join("config.jsonl"),
        cargo_args: &["build"],
        environment: &[],
        binary: config_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_library_compile: true,
    });
}

fn run_environment_flag_cases(roots: &TempDir, cache: &TempDir) {
    let environment_consumer = roots.path().join("environment-consumer");
    write_flag_fixture(&environment_consumer);
    assert_flag_invalidation(&FlagExpectation {
        root: &environment_consumer,
        cache: cache.path(),
        log: &roots.path().join("environment.jsonl"),
        cargo_args: &["build"],
        environment: &[("RUSTFLAGS", "--cfg reapi_variant")],
        binary: environment_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_library_compile: false,
    });

    let encoded_consumer = roots.path().join("encoded-consumer");
    write_flag_fixture(&encoded_consumer);
    assert_flag_invalidation(&FlagExpectation {
        root: &encoded_consumer,
        cache: cache.path(),
        log: &roots.path().join("encoded.jsonl"),
        cargo_args: &["build"],
        environment: &[("CARGO_ENCODED_RUSTFLAGS", "--cfg\u{1f}reapi_variant")],
        binary: encoded_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_library_compile: false,
    });
}

fn run_ancestor_and_cargo_home_flag_cases(roots: &TempDir, cache: &TempDir) {
    let ancestor_root = roots.path().join("ancestor");
    let ancestor_consumer = ancestor_root.join("consumer");
    write_flag_fixture(&ancestor_consumer);
    fs::create_dir_all(ancestor_root.join(".cargo")).expect("ancestor config directory");
    fs::write(
        ancestor_root.join(".cargo/config.toml"),
        "[build]\nrustflags=['--cfg', 'reapi_variant']\n",
    )
    .expect("ancestor cargo config");
    assert_flag_invalidation(&FlagExpectation {
        root: &ancestor_consumer,
        cache: cache.path(),
        log: &roots.path().join("ancestor.jsonl"),
        cargo_args: &["build"],
        environment: &[],
        binary: ancestor_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_library_compile: false,
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
    assert_flag_invalidation(&FlagExpectation {
        root: &cargo_home_consumer,
        cache: cache.path(),
        log: &roots.path().join("cargo-home.jsonl"),
        cargo_args: &["build"],
        environment: &[("CARGO_HOME", cargo_home_text.as_str())],
        binary: cargo_home_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_library_compile: false,
    });
}

fn run_profile_and_feature_flag_cases(roots: &TempDir, cache: &TempDir) {
    let profile_consumer = roots.path().join("profile-consumer");
    write_flag_fixture(&profile_consumer);
    fs::write(
        profile_consumer.join("Cargo.toml"),
        "[package]\nname='flag-fixture'\nversion='0.0.0'\nedition='2024'\n[features]\nvariant=[]\n[profile.dev]\ndebug-assertions=false\n",
    )
    .expect("profile manifest");
    assert_flag_invalidation(&FlagExpectation {
        root: &profile_consumer,
        cache: cache.path(),
        log: &roots.path().join("profile.jsonl"),
        cargo_args: &["build"],
        environment: &[],
        binary: profile_consumer.join("target/debug/flag-fixture"),
        stdout: "42:false",
        require_library_compile: true,
    });

    let feature_consumer = roots.path().join("feature-consumer");
    write_flag_fixture(&feature_consumer);
    assert_flag_invalidation(&FlagExpectation {
        root: &feature_consumer,
        cache: cache.path(),
        log: &roots.path().join("feature.jsonl"),
        cargo_args: &["build", "--features", "variant"],
        environment: &[],
        binary: feature_consumer.join("target/debug/flag-fixture"),
        stdout: "43:true",
        require_library_compile: true,
    });
}

fn run_target_flag_case(roots: &TempDir, cache: &TempDir) {
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
    assert_flag_invalidation(&FlagExpectation {
        root: &target_consumer,
        cache: cache.path(),
        log: &roots.path().join("target.jsonl"),
        cargo_args: &["build", "--target", &host],
        environment: &[],
        binary: target_consumer
            .join("target")
            .join(&host)
            .join("debug/flag-fixture"),
        stdout: "42:true",
        require_library_compile: false,
    });
}

#[test]
fn profile_environment_and_cargo_config_flags_all_invalidate() {
    let roots = tempdir().expect("flag roots");
    let cache = tempdir().expect("shared cache");
    let producer = roots.path().join("producer");
    write_flag_fixture(&producer);

    let producer_log = roots.path().join("producer.jsonl");
    assert!(cached_build(&producer, cache.path(), &producer_log, &["build"], &[]).success());

    run_config_flag_cases(&roots, &cache);
    run_environment_flag_cases(&roots, &cache);
    run_ancestor_and_cargo_home_flag_cases(&roots, &cache);
    run_profile_and_feature_flag_cases(&roots, &cache);
    run_target_flag_case(&roots, &cache);
}

/// A proc macro that reads a variable, a library that expands it, and a
/// binary that prints the library's value.
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
        root.join("app/src/lib.rs"),
        "pub const VALUE: u32 = environment_macro::environment_value!();\n",
    )
    .expect("macro app library");
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", environment_app::VALUE); }\n",
    )
    .expect("macro app binary");
}

#[test]
fn proc_macro_environment_change_invalidates_compiler_action() {
    let roots = tempdir().expect("proc macro roots");
    let cache = tempdir().expect("shared cache");
    let producer = roots.path().join("producer");
    let consumer = roots.path().join("consumer");
    write_environment_proc_macro_fixture(&producer);
    write_environment_proc_macro_fixture(&consumer);
    let build = ["build", "--workspace"];

    let producer_log = roots.path().join("producer.jsonl");
    let producer_environment = [
        ("REAPI_PROC_VALUE", "42"),
        ("CARGO_REAPI_DECLARED_ENV", "REAPI_PROC_VALUE"),
    ];
    assert!(
        cached_build(
            &producer,
            cache.path(),
            &producer_log,
            &build,
            &producer_environment
        )
        .success()
    );
    fs::remove_dir_all(&producer).expect("retire producer");

    let consumer_log = roots.path().join("consumer.jsonl");
    let consumer_environment = [
        ("REAPI_PROC_VALUE", "43"),
        ("CARGO_REAPI_DECLARED_ENV", "REAPI_PROC_VALUE"),
    ];
    assert!(
        cached_build(
            &consumer,
            cache.path(),
            &consumer_log,
            &build,
            &consumer_environment
        )
        .success()
    );

    assert_eq!(
        library_executions(&read_actions(&consumer_log), "environment_app"),
        ["local-cache-miss"]
    );
    assert_eq!(
        run_program(&consumer.join("target/debug/environment-app")),
        "43"
    );
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
fn path_dependency_outside_worktree_invalidates_its_compile() {
    let roots = tempdir().expect("external dependency roots");
    let cache = tempdir().expect("shared cache");
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

    let producer_log = roots.path().join("producer.jsonl");
    assert!(cached_build(&producer, cache.path(), &producer_log, &["build"], &[]).success());
    fs::remove_dir_all(&producer).expect("retire producer");
    fs::write(
        dependency.join("src/lib.rs"),
        "pub fn answer() -> u32 { 43 }\n",
    )
    .expect("mutate path dependency");

    let consumer_log = roots.path().join("consumer.jsonl");
    assert!(cached_build(&consumer, cache.path(), &consumer_log, &["build"], &[]).success());

    assert_eq!(
        library_executions(&read_actions(&consumer_log), "shared_dependency"),
        ["local-cache-miss"]
    );
    assert_eq!(
        run_program(&consumer.join("target/debug/external-app")),
        "43"
    );
}

#[test]
fn clippy_preserves_its_nested_wrapper_chain() {
    let root = tempdir().expect("clippy fixture");
    let cache = tempdir().expect("clippy cache");
    write_fixture(root.path(), false);
    let log = root.path().join("actions.jsonl");

    let status = cached_build(
        root.path(),
        cache.path(),
        &log,
        &["clippy", "--all-targets", "--", "-D", "warnings"],
        &[],
    );

    assert!(status.success(), "cargo clippy failed through cargo-reapi");
    let actions = read_actions(&log);
    assert!(
        actions
            .iter()
            .any(|action| action["crate_name"] == "capture_fixture"
                && action["compiler"]
                    .as_str()
                    .is_some_and(|compiler| compiler.contains("clippy-driver"))),
        "the fixture crate was not compiled by clippy-driver through the wrapper"
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
        .args(["--backend", "capture", "--action-log"])
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
fn a_linked_binary_always_runs_its_link_step_and_runs_in_a_later_worktree() {
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

    for action in [&first_action, second_action] {
        assert_eq!(action["execution"], "local-ineligible", "{action:#}");
        assert_eq!(action["cache_eligibility"]["eligible"], false);
        assert!(
            action["cache_eligibility"]["reasons"]
                .to_string()
                .contains("a link step"),
            "{action:#}"
        );
    }
    let embedded_manifest = run_program(&second.path().join("target/debug/capture-fixture"));
    assert_eq!(
        fs::canonicalize(embedded_manifest).expect("embedded consumer manifest path"),
        fs::canonicalize(second.path()).expect("consumer manifest path")
    );
}

/// A library built as both a `cdylib` and an `rlib`, and a binary in the
/// same package that links the `rlib`.
fn write_two_crate_type_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"capture-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [lib]\nname = \"both_kinds\"\ncrate-type = [\"cdylib\", \"rlib\"]\n",
    )
    .expect("fixture manifest");
    fs::write(root.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n").expect("library source");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", both_kinds::answer()); }\n",
    )
    .expect("binary source");
}

#[test]
fn a_library_that_is_also_a_cdylib_always_runs() {
    let cache = tempdir().expect("shared cache directory");
    let first = tempdir().expect("first fixture directory");
    let second = tempdir().expect("second fixture directory");
    write_two_crate_type_fixture(first.path());
    write_two_crate_type_fixture(second.path());

    let library = |actions: &[Value]| {
        actions
            .iter()
            .find(|action| action["crate_name"] == "both_kinds")
            .expect("library compiler action")
            .clone()
    };
    let first_action = library(&run(first.path(), "build", "cache", Some(cache.path())));
    drop(first);
    let second_action = library(&run(second.path(), "build", "cache", Some(cache.path())));

    // Linking the cdylib makes the whole compile a link step.
    assert_eq!(first_action["execution"], "local-ineligible");
    assert_eq!(second_action["execution"], "local-ineligible");
    assert_eq!(
        run_program(&second.path().join("target/debug/capture-fixture")),
        "42"
    );
}

#[test]
fn a_restored_library_relocates_paths_for_an_integration_test_in_a_later_worktree() {
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
    let library = |actions: &[Value]| {
        actions
            .iter()
            .find(|action| {
                action["crate_name"] == "companion_fixture" && is_library_compile(action)
            })
            .expect("library compiler action")
            .clone()
    };

    let producer_library = library(&run(&producer, "test", "cache", Some(cache.path())));
    assert_eq!(producer_library["execution"], "local-cache-miss");
    fs::remove_dir_all(&producer).expect("remove producer before consumer");

    // `run` asserts that the consumer's tests pass: the companion binary
    // prints paths from the restored library, which must name the consumer.
    let consumer_library = library(&run(&consumer, "test", "cache", Some(cache.path())));

    assert_eq!(consumer_library["execution"], "cache-hit");
    for field in ["arguments", "keyed_environment", "inputs"] {
        assert_eq!(
            producer_library[field], consumer_library[field],
            "normalized library {field} differ"
        );
    }
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
        first_app["execution"], "local-ineligible",
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
    assert_eq!(second_app["execution"], "local-ineligible");
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
