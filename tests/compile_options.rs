//! A compile is cached only with the outputs and options cargo-reapi
//! supports: anything that writes an undeclared output, depends on the
//! machine or hides arguments from analysis runs uncached.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

/// Runs Cargo through the cache backend with a fresh action log.
fn cargo_reapi(
    root: &Path,
    cache: &Path,
    cargo_args: &[&str],
    environment: &[(&str, &str)],
) -> Output {
    let action_log = action_log(root);
    let _ = fs::remove_file(&action_log);

    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .args(["--backend", "cache", "--action-log"])
        .arg(&action_log)
        .arg("--cache-dir")
        .arg(cache)
        .arg("--")
        .args(cargo_args);
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("run cargo-reapi")
}

fn action_log(root: &Path) -> PathBuf {
    root.join("target/cargo-reapi/actions.jsonl")
}

/// The executions logged for the package's library compile.
fn library_executions(root: &Path) -> Vec<String> {
    fs::read_to_string(action_log(root))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid JSON action"))
        .filter(|action| {
            action["crate_name"] == "options"
                && action["arguments"]
                    .to_string()
                    .contains("\"--crate-type\",\"lib\"")
        })
        .map(|action| action["execution"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn assert_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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

/// A package `options` whose library is `library` and whose binary prints
/// the library's `value()`.
fn write_package(root: &Path, library: &str) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='options'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("manifest");
    fs::write(root.join("src/lib.rs"), library).expect("library source");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", options::value()); }\n",
    )
    .expect("binary source");
}

const LIBRARY: &str = "pub fn value() -> u32 { 42 }\n";

#[test]
fn a_compile_for_the_native_cpu_runs_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let root = roots.path().join("package");
    write_package(&root, LIBRARY);

    let output = cargo_reapi(
        &root,
        cache.path(),
        &["build"],
        &[("RUSTFLAGS", "-C target-cpu=native")],
    );

    assert_succeeded(&output);
    assert_eq!(library_executions(&root), ["local-ineligible"]);
    assert_eq!(run_program(&root.join("target/debug/options")), "42");
}

#[test]
fn unpacked_split_debuginfo_runs_uncached_and_packed_is_cached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let root = roots.path().join("package");
    write_package(&root, LIBRARY);

    // Unpacked debuginfo leaves `.dwo` files beside the declared outputs.
    let unpacked = cargo_reapi(
        &root,
        cache.path(),
        &["build"],
        &[("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "unpacked")],
    );
    assert_succeeded(&unpacked);
    assert_eq!(library_executions(&root), ["local-ineligible"]);

    // Packed debuginfo stays inside the rlib.
    let packed = cargo_reapi(
        &root,
        cache.path(),
        &["build"],
        &[("CARGO_PROFILE_DEV_SPLIT_DEBUGINFO", "packed")],
    );
    assert_succeeded(&packed);
    assert_eq!(library_executions(&root), ["local-cache-miss"]);
}

#[test]
fn an_emit_kind_the_cache_does_not_declare_runs_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let root = roots.path().join("package");
    write_package(&root, LIBRARY);

    let output = cargo_reapi(
        &root,
        cache.path(),
        &[
            "rustc",
            "--lib",
            "--",
            "--emit=dep-info,metadata,link,llvm-ir",
        ],
        &[],
    );

    assert_succeeded(&output);
    assert_eq!(library_executions(&root), ["local-ineligible"]);
    let ir = fs::read_dir(root.join("target/debug/deps"))
        .expect("output directory")
        .filter_map(Result::ok)
        .any(|entry| entry.path().extension().is_some_and(|ext| ext == "ll"));
    assert!(ir, "the requested LLVM IR was written");
}

#[test]
fn arguments_from_a_response_file_run_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let root = roots.path().join("package");
    write_package(
        &root,
        "#[cfg(answer)]\npub fn value() -> u32 { 42 }\n\
         #[cfg(not(answer))]\npub fn value() -> u32 { 0 }\n",
    );
    // Inside the package the file itself is keyed; the arguments it holds
    // are still not analysed.
    let response_file = root.join("flags.rsp");
    fs::write(&response_file, "--cfg\nanswer\n").expect("response file");
    let flags = format!("@{}", response_file.display());

    let output = cargo_reapi(&root, cache.path(), &["build"], &[("RUSTFLAGS", &flags)]);

    assert_succeeded(&output);
    assert_eq!(library_executions(&root), ["local-ineligible"]);
    assert_eq!(run_program(&root.join("target/debug/options")), "42");
}

#[test]
fn a_codegen_option_outside_the_supported_list_runs_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let root = roots.path().join("package");
    write_package(&root, LIBRARY);

    // `save-temps` keeps intermediate files beside the declared outputs.
    let output = cargo_reapi(
        &root,
        cache.path(),
        &["build"],
        &[("RUSTFLAGS", "-C save-temps")],
    );

    assert_succeeded(&output);
    assert_eq!(library_executions(&root), ["local-ineligible"]);
    assert_eq!(run_program(&root.join("target/debug/options")), "42");
}
