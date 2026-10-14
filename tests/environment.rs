//! A cached compile runs with, and is keyed on, only the environment it can
//! read. These tests drive real Cargo through the cache backend.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::tempdir;

struct Build {
    output: Output,
    actions: Vec<Value>,
}

impl Build {
    fn succeeded(&self) -> bool {
        self.output.status.success()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    /// Every recorded execution of `crate_name`'s compiles, in order.
    fn executions(&self, crate_name: &str) -> Vec<String> {
        self.actions_for(crate_name)
            .map(|action| action["execution"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn final_action(&self, crate_name: &str) -> &Value {
        self.actions_for(crate_name)
            .last()
            .unwrap_or_else(|| panic!("no action for {crate_name}"))
    }

    fn actions_for(&self, crate_name: &str) -> impl Iterator<Item = &Value> {
        self.actions
            .iter()
            .filter(move |action| action["crate_name"] == crate_name)
    }
}

/// Runs `cargo build` through the cache backend with only the action cache:
/// whole-gate snapshots are disabled so every compile is visible.
fn cached_build(root: &Path, cache: &Path, environment: &[(&str, &str)]) -> Build {
    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_REAPI_DECLARED_ENV")
        .env_remove("CARGO_REAPI_REPORT_STRIPPED_ENV")
        .env("CARGO_REAPI_ACTION_CACHE_TEST_MODE", "1")
        .args(["--backend", "cache", "--action-log"])
        .arg(&action_log)
        .arg("--cache-dir")
        .arg(cache)
        .args(["--", "build"]);
    for (name, value) in environment {
        command.env(name, value);
    }

    let output = command.output().expect("run cargo-reapi");
    let actions = fs::read_to_string(&action_log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect();
    Build { output, actions }
}

fn run_binary(path: &Path) -> String {
    let output = Command::new(path)
        .output()
        .unwrap_or_else(|error| panic!("run {}: {error}", path.display()));
    assert!(output.status.success(), "{} failed", path.display());
    String::from_utf8(output.stdout)
        .expect("UTF-8 output")
        .trim()
        .to_owned()
}

fn write_package(root: &Path, name: &str, main: &str, build_script: Option<&str>) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname='{name}'\nversion='0.0.0'\nedition='2024'\n"),
    )
    .expect("manifest");
    fs::write(root.join("src/main.rs"), main).expect("main source");
    if let Some(build_script) = build_script {
        fs::write(root.join("build.rs"), build_script).expect("build script");
    }
}

#[test]
fn env_macro_variable_is_keyed_and_reaches_the_compile() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let main = "fn main() { println!(\"{}\", env!(\"ENVKEY_FIXTURE_VALUE\")); }\n";
    for worktree in ["producer", "changed", "original"] {
        write_package(&roots.path().join(worktree), "envkey-app", main, None);
    }

    // The variable is outside the toolchain families and Cargo did not set
    // it, so the first compile runs without it. rustc's dep-info names it,
    // and the compile runs again with it.
    let producer = cached_build(
        &roots.path().join("producer"),
        cache.path(),
        &[("ENVKEY_FIXTURE_VALUE", "one")],
    );
    assert!(producer.succeeded(), "{}", producer.stderr());
    assert_eq!(
        producer.executions("envkey_app"),
        ["local-environment-withheld", "local-cache-miss"]
    );
    assert_eq!(
        run_binary(&roots.path().join("producer/target/debug/envkey-app")),
        "one"
    );

    // A changed value is a different action. The learned name is passed from
    // the start.
    let changed = cached_build(
        &roots.path().join("changed"),
        cache.path(),
        &[("ENVKEY_FIXTURE_VALUE", "two")],
    );
    assert!(changed.succeeded(), "{}", changed.stderr());
    assert_eq!(changed.executions("envkey_app"), ["local-cache-miss"]);
    assert_eq!(
        run_binary(&roots.path().join("changed/target/debug/envkey-app")),
        "two"
    );

    let original = cached_build(
        &roots.path().join("original"),
        cache.path(),
        &[("ENVKEY_FIXTURE_VALUE", "one")],
    );
    assert!(original.succeeded(), "{}", original.stderr());
    assert_eq!(original.executions("envkey_app"), ["cache-hit"]);
    assert_eq!(
        run_binary(&roots.path().join("original/target/debug/envkey-app")),
        "one"
    );
}

#[test]
fn rerun_if_env_changed_variable_is_keyed() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let build_script =
        "fn main() { println!(\"cargo::rerun-if-env-changed=ENVKEY_BUILD_VALUE\"); }\n";
    for worktree in ["producer", "changed", "original"] {
        write_package(
            &roots.path().join(worktree),
            "envkey-build",
            "fn main() {}\n",
            Some(build_script),
        );
    }

    let producer = cached_build(
        &roots.path().join("producer"),
        cache.path(),
        &[("ENVKEY_BUILD_VALUE", "one")],
    );
    assert!(producer.succeeded(), "{}", producer.stderr());
    assert_eq!(producer.executions("envkey_build"), ["local-cache-miss"]);
    assert!(
        producer.final_action("envkey_build")["keyed_environment"]["ENVKEY_BUILD_VALUE"]
            .is_string()
    );

    let changed = cached_build(
        &roots.path().join("changed"),
        cache.path(),
        &[("ENVKEY_BUILD_VALUE", "two")],
    );
    assert!(changed.succeeded(), "{}", changed.stderr());
    assert_eq!(changed.executions("envkey_build"), ["local-cache-miss"]);

    let original = cached_build(
        &roots.path().join("original"),
        cache.path(),
        &[("ENVKEY_BUILD_VALUE", "one")],
    );
    assert!(original.succeeded(), "{}", original.stderr());
    assert_eq!(original.executions("envkey_build"), ["cache-hit"]);
}

#[cfg(unix)]
#[test]
fn shell_history_and_unrelated_variables_do_not_change_action_keys() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let main = "fn main() { println!(\"{}\", env!(\"CARGO_PKG_NAME\")); }\n";
    write_package(&roots.path().join("first"), "envkey-shell", main, None);
    write_package(
        &roots.path().join("second-worktree"),
        "envkey-shell",
        main,
        None,
    );

    // Each shell gets its own symlink to one tool directory, as fnm does.
    let tools = roots.path().join("tools");
    fs::create_dir(&tools).expect("tool directory");
    let first_link = roots.path().join("shell-1-tools");
    let second_link = roots.path().join("shell-2-tools");
    std::os::unix::fs::symlink(&tools, &first_link).expect("first shell link");
    std::os::unix::fs::symlink(&tools, &second_link).expect("second shell link");
    let path = std::env::var_os("PATH").expect("PATH");
    // Shells drop an OLDPWD that names no directory.
    let first_previous = tools.to_string_lossy().into_owned();
    let second_previous = cache.path().to_string_lossy().into_owned();
    let first_path = prepend_path(&first_link, &path);
    let second_path = prepend_path(&second_link, &path);

    let first = cached_build(
        &roots.path().join("first"),
        cache.path(),
        &[
            ("OLDPWD", first_previous.as_str()),
            ("ENVKEY_UNRELATED", "first-shell"),
            ("PATH", first_path.as_str()),
        ],
    );
    assert!(first.succeeded(), "{}", first.stderr());
    assert_eq!(first.executions("envkey_shell"), ["local-cache-miss"]);

    let second = cached_build(
        &roots.path().join("second-worktree"),
        cache.path(),
        &[
            ("OLDPWD", second_previous.as_str()),
            ("ENVKEY_UNRELATED", "second-shell"),
            ("PATH", second_path.as_str()),
        ],
    );
    assert!(second.succeeded(), "{}", second.stderr());
    assert_eq!(second.executions("envkey_shell"), ["cache-hit"]);

    let first_action = first.final_action("envkey_shell");
    let second_action = second.final_action("envkey_shell");
    assert_eq!(first_action["action_key"], second_action["action_key"]);
    assert!(first_action["keyed_environment"]["OLDPWD"].is_null());
    assert!(
        second_action["stripped_environment"]
            .as_array()
            .expect("stripped environment")
            .iter()
            .any(|name| name == "OLDPWD")
    );
}

#[cfg(unix)]
fn prepend_path(directory: &Path, path: &std::ffi::OsStr) -> String {
    format!("{}:{}", directory.display(), path.to_string_lossy())
}

fn write_environment_macro_workspace(root: &Path) {
    fs::create_dir_all(root.join("macro/src")).expect("macro source");
    fs::create_dir_all(root.join("app/src")).expect("app source");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['macro','app']\nresolver='3'\n",
    )
    .expect("workspace manifest");
    fs::write(
        root.join("macro/Cargo.toml"),
        "[package]\nname='envkey-macro'\nversion='0.0.0'\nedition='2024'\n[lib]\nproc-macro=true\n",
    )
    .expect("macro manifest");
    fs::write(
        root.join("macro/src/lib.rs"),
        r#"extern crate proc_macro;
use proc_macro::TokenStream;
#[proc_macro]
pub fn untracked_value(_: TokenStream) -> TokenStream {
    std::env::var("ENVKEY_PROC_VALUE")
        .expect("ENVKEY_PROC_VALUE must be set")
        .parse()
        .unwrap()
}
"#,
    )
    .expect("macro source");
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname='envkey-macro-app'\nversion='0.0.0'\nedition='2024'\n[dependencies]\nenvkey-macro={path='../macro'}\n",
    )
    .expect("app manifest");
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", envkey_macro::untracked_value!()); }\n",
    )
    .expect("app source");
}

#[test]
fn undeclared_proc_macro_environment_read_is_withheld_and_reported() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let workspace = roots.path().join("workspace");
    write_environment_macro_workspace(&workspace);

    // rustc cannot track a proc macro's std::env read, so the variable never
    // reaches the compile: the build fails rather than caching an artifact
    // keyed without it. The report names what was removed.
    let undeclared = cached_build(
        &workspace,
        cache.path(),
        &[
            ("ENVKEY_PROC_VALUE", "42"),
            ("CARGO_REAPI_REPORT_STRIPPED_ENV", "1"),
        ],
    );
    assert!(!undeclared.succeeded());
    let stderr = undeclared.stderr();
    assert!(
        stderr.contains("envkey_macro_app compiles without:")
            && stderr.contains("ENVKEY_PROC_VALUE"),
        "{stderr}"
    );

    let declared = cached_build(
        &workspace,
        cache.path(),
        &[
            ("ENVKEY_PROC_VALUE", "42"),
            ("CARGO_REAPI_DECLARED_ENV", "ENVKEY_PROC_VALUE"),
        ],
    );
    assert!(declared.succeeded(), "{}", declared.stderr());
    assert_eq!(
        run_binary(&workspace.join("target/debug/envkey-macro-app")),
        "42"
    );
}
