//! A cached compile is a hit only while every file rustc read is unchanged;
//! anything cargo-reapi cannot use runs as plain Cargo.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

/// Runs Cargo through the cache backend with a fresh action log at
/// `<root>/target/cargo-reapi/actions.jsonl`.
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

fn action_log(root: &Path) -> std::path::PathBuf {
    root.join("target/cargo-reapi/actions.jsonl")
}

fn actions(root: &Path) -> Vec<Value> {
    fs::read_to_string(action_log(root))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect()
}

/// The executions logged for `crate_name`'s library compile.
fn library_executions(root: &Path, crate_name: &str) -> Vec<String> {
    actions(root)
        .iter()
        .filter(|action| {
            action["crate_name"] == crate_name
                && action["arguments"]
                    .to_string()
                    .contains("\"--crate-type\",\"lib\"")
        })
        .map(|action| action["execution"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_succeeded(output: &Output) {
    assert!(output.status.success(), "{}", stderr(output));
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

/// A package whose library is `library` and whose binary prints the
/// library's `value()`.
fn write_package(root: &Path, name: &str, library: &str) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname='{name}'\nversion='0.0.0'\nedition='2024'\n"),
    )
    .expect("manifest");
    fs::write(root.join("src/lib.rs"), library).expect("library source");
    let crate_name = name.replace('-', "_");
    fs::write(
        root.join("src/main.rs"),
        format!("fn main() {{ println!(\"{{}}\", {crate_name}::value()); }}\n"),
    )
    .expect("binary source");
}

fn write_workspace_manifest(root: &Path, members: &str) {
    fs::create_dir_all(root).expect("workspace directory");
    fs::write(
        root.join("Cargo.toml"),
        format!("[workspace]\nmembers = [{members}]\nresolver = '3'\n"),
    )
    .expect("workspace manifest");
}

#[test]
fn a_file_included_from_outside_the_package_keys_the_compile() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let workspace = roots.path().join("workspace");
    write_workspace_manifest(&workspace, "'member'");
    write_package(
        &workspace.join("member"),
        "member",
        "include!(\"../../shared/body.rs\");\n",
    );
    fs::create_dir_all(workspace.join("shared")).expect("shared directory");
    let body = workspace.join("shared/body.rs");
    fs::write(&body, "pub fn value() -> u8 { 1 }\n").expect("body");

    assert_succeeded(&cargo_reapi(&workspace, cache.path(), &["check"], &[]));
    fs::write(&body, "compile_error!(\"changed\");\n").expect("break the body");
    let output = cargo_reapi(&workspace, cache.path(), &["check"], &[]);

    assert!(
        !output.status.success(),
        "a stale compile result: {}",
        stderr(&output)
    );
    assert!(stderr(&output).contains("changed"), "{}", stderr(&output));

    // With the body as it was, a fresh target is served from the cache.
    fs::write(&body, "pub fn value() -> u8 { 1 }\n").expect("restore the body");
    fs::remove_dir_all(workspace.join("target")).expect("retire the target");
    assert_succeeded(&cargo_reapi(&workspace, cache.path(), &["check"], &[]));
    assert_eq!(library_executions(&workspace, "member"), ["cache-hit"]);
}

#[cfg(unix)]
#[test]
fn dep_info_inputs_are_checked_as_rustc_read_them() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let workspace = roots.path().join("workspace");
    write_workspace_manifest(&workspace, "'member'");
    // `link/../value.txt` is `shared/value.txt`, not `member/src/value.txt`,
    // and `a\b.txt` is a name with a backslash, not `ab.txt`.
    write_package(
        &workspace.join("member"),
        "member",
        "pub fn value() -> String { format!(\"{}{}\", include_str!(\"link/../value.txt\"), include_str!(\"../../shared/a\\\\b.txt\")) }\n",
    );
    fs::create_dir_all(workspace.join("shared/subdir")).expect("shared directory");
    std::os::unix::fs::symlink("../../shared/subdir", workspace.join("member/src/link"))
        .expect("link");
    fs::write(workspace.join("member/src/value.txt"), "decoy").expect("decoy");
    fs::write(workspace.join("shared/ab.txt"), "decoy").expect("backslash decoy");
    let write = |linked: &str, backslashed: &str| {
        fs::write(workspace.join("shared/value.txt"), linked).expect("shared value");
        fs::write(workspace.join("shared").join("a\\b.txt"), backslashed).expect("backslash file");
    };
    let binary = workspace.join("target/debug/member");

    write("one", "uno");
    assert_succeeded(&cargo_reapi(&workspace, cache.path(), &["build"], &[]));
    assert_eq!(run_program(&binary), "oneuno");

    // Each input changes alone, so that the other cannot be what notices.
    write("two", "uno");
    assert_succeeded(&cargo_reapi(&workspace, cache.path(), &["build"], &[]));
    assert_eq!(run_program(&binary), "twouno");
    write("two", "dos");
    assert_succeeded(&cargo_reapi(&workspace, cache.path(), &["build"], &[]));
    assert_eq!(run_program(&binary), "twodos");
}

#[test]
fn an_action_entry_of_an_older_schema_is_a_miss_not_an_error() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let project = roots.path().join("project");
    write_package(
        &project,
        "project",
        "pub fn value() -> &'static str { \"rebuilt\" }\n",
    );
    assert_succeeded(&cargo_reapi(&project, cache.path(), &["build"], &[]));
    let key = actions(&project)
        .iter()
        .find(|action| action["execution"] == "local-cache-miss")
        .and_then(|action| action["action_key"].as_str().map(str::to_owned))
        .expect("a published action");

    // Mark the entry as another version wrote it. It still parses, and its
    // inputs still match, so only the version can make it a miss.
    let manifest = cache.path().join("actions").join(format!("{key}.json"));
    let mut entry: Value =
        serde_json::from_slice(&fs::read(&manifest).expect("manifest")).expect("JSON");
    entry["schema_version"] = 1.into();
    fs::write(&manifest, serde_json::to_vec(&entry).expect("JSON")).expect("old manifest");
    fs::remove_dir_all(project.join("target")).expect("retire the target");

    let output = cargo_reapi(&project, cache.path(), &["build"], &[]);

    assert_succeeded(&output);
    assert_eq!(
        library_executions(&project, "project"),
        ["local-cache-miss"]
    );
    assert_eq!(
        run_program(&project.join("target/debug/project")),
        "rebuilt"
    );
}

#[test]
fn a_source_directory_named_target_is_hashed() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let project = roots.path().join("project");
    let output = roots.path().join("output");
    let output_text = output.to_string_lossy().into_owned();
    write_package(
        &project,
        "project",
        "pub fn value() -> &'static str { include_str!(\"target/message.txt\") }\n",
    );
    fs::create_dir_all(project.join("src/target")).expect("source target directory");
    let message = project.join("src/target/message.txt");
    let environment = [("CARGO_TARGET_DIR", output_text.as_str())];

    fs::write(&message, "original").expect("message");
    assert_succeeded(&cargo_reapi(
        &project,
        cache.path(),
        &["build"],
        &environment,
    ));
    assert_eq!(run_program(&output.join("debug/project")), "original");

    // A fresh output directory, so only the action key can tell.
    fs::remove_dir_all(&output).expect("retire the output");
    fs::write(&message, "edited").expect("edit message");
    assert_succeeded(&cargo_reapi(
        &project,
        cache.path(),
        &["build"],
        &environment,
    ));

    assert_eq!(run_program(&output.join("debug/project")), "edited");
}

/// A package whose build script builds `libanswer.a` in its output
/// directory and links its library against it.
fn write_out_dir_archive_package(root: &Path) {
    write_package(
        root,
        "archive-user",
        "unsafe extern \"C\" { fn answer() -> i32; }\npub fn value() -> i32 { unsafe { answer() } }\n",
    );
    fs::write(
        root.join("build.rs"),
        r#"use std::process::Command;
fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("answer.c"), "int answer(void) { return 42; }\n").unwrap();
    // Relative names keep the archive identical in every worktree.
    assert!(Command::new("cc").current_dir(&out).args(["-c", "answer.c", "-o", "answer.o"]).status().unwrap().success());
    let _ = std::fs::remove_file(out.join("libanswer.a"));
    assert!(Command::new("ar").current_dir(&out).args(["rcs", "libanswer.a", "answer.o"]).status().unwrap().success());
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=answer");
}
"#,
    )
    .expect("build script");
}

#[cfg(unix)]
#[test]
fn a_library_linking_an_archive_its_build_script_built_stays_cached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let first = roots.path().join("first");
    let second = roots.path().join("second");
    write_out_dir_archive_package(&first);
    write_out_dir_archive_package(&second);

    assert_succeeded(&cargo_reapi(&first, cache.path(), &["build"], &[]));
    assert_eq!(
        library_executions(&first, "archive_user"),
        ["local-cache-miss"]
    );
    let output = cargo_reapi(&second, cache.path(), &["build"], &[]);

    assert_succeeded(&output);
    assert_eq!(library_executions(&second, "archive_user"), ["cache-hit"]);
    assert_eq!(run_program(&second.join("target/debug/archive-user")), "42");
}

/// Asserts that a build ran as plain Cargo for `reason`, logging nothing.
fn assert_declined(project: &Path, output: &Output, reason: &str) {
    assert_succeeded(output);
    let stderr = stderr(output);
    assert!(
        stderr.contains("cargo-reapi: not caching (") && stderr.contains(reason),
        "{stderr}"
    );
    assert!(
        actions(project).is_empty(),
        "a declined build logged compiles"
    );
    assert_eq!(run_program(&project.join("target/debug/project")), "plain");
}

#[test]
fn a_compiler_wrapper_the_user_configured_runs_cargo_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("cache");
    let project = roots.path().join("project");
    write_package(
        &project,
        "project",
        "pub fn value() -> &'static str { \"plain\" }\n",
    );
    // `env` runs the compiler it is given, so the wrapper is harmless.
    let wrapper = "env";

    let output = cargo_reapi(
        &project,
        cache.path(),
        &["build"],
        &[("RUSTC_WRAPPER", wrapper)],
    );
    assert_declined(&project, &output, "RUSTC_WRAPPER");

    let configured = format!("build.rustc-wrapper = '{wrapper}'");
    let output = cargo_reapi(
        &project,
        cache.path(),
        &["build", "--config", &configured],
        &[],
    );
    assert_declined(&project, &output, "build.rustc-wrapper");

    fs::create_dir_all(project.join(".cargo")).expect("configuration directory");
    fs::write(
        project.join(".cargo/config.toml"),
        format!("[build]\nrustc-workspace-wrapper = '{wrapper}'\n"),
    )
    .expect("configuration");
    let output = cargo_reapi(&project, cache.path(), &["build"], &[]);
    assert_declined(&project, &output, "build.rustc-workspace-wrapper");
}
