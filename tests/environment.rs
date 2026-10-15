//! A cached compile runs with, and is keyed on, only the environment it can
//! read. These tests drive real Cargo through the cache backend.

use std::fs;
use std::path::{Path, PathBuf};
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

    /// Every recorded execution of `crate_name`'s library compiles, in
    /// order. A binary's compile is a link step, which always runs.
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
            .filter(move |action| action["crate_name"] == crate_name && is_library_compile(action))
    }
}

fn is_library_compile(action: &Value) -> bool {
    action["arguments"].as_array().is_some_and(|arguments| {
        arguments
            .windows(2)
            .any(|pair| pair[0] == "--crate-type" && pair[1] == "lib")
    })
}

/// Runs `cargo build` through the cache backend.
fn cached_build(root: &Path, cache: &Path, environment: &[(&str, &str)]) -> Build {
    cargo_build(root, cache, &["build"], environment)
}

fn cargo_build(
    root: &Path,
    cache: &Path,
    cargo_args: &[&str],
    environment: &[(&str, &str)],
) -> Build {
    let output = cargo_reapi(root, cache, cargo_args, environment)
        .output()
        .expect("run cargo-reapi");
    finished_build(root, output)
}

fn cargo_reapi(
    root: &Path,
    cache: &Path,
    cargo_args: &[&str],
    environment: &[(&str, &str)],
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"));
    command
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_REAPI_DECLARED_ENV")
        .env_remove("CARGO_REAPI_REPORT_STRIPPED_ENV")
        .args(["--backend", "cache", "--action-log"])
        .arg(action_log(root))
        .arg("--cache-dir")
        .arg(cache)
        .arg("--")
        .args(cargo_args);
    for (name, value) in environment {
        command.env(name, value);
    }
    command
}

fn action_log(root: &Path) -> PathBuf {
    root.join("target/cargo-reapi/actions.jsonl")
}

fn finished_build(root: &Path, output: Output) -> Build {
    let actions = fs::read_to_string(action_log(root))
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

/// A package whose library defines `value() -> &'static str` with `body`,
/// and whose binary prints it.
fn write_package(root: &Path, name: &str, body: &str, build_script: Option<&str>) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname='{name}'\nversion='0.0.0'\nedition='2024'\n"),
    )
    .expect("manifest");
    fs::write(
        root.join("src/lib.rs"),
        format!("pub fn value() -> &'static str {{ {body} }}\n"),
    )
    .expect("library source");
    let crate_name = name.replace('-', "_");
    fs::write(
        root.join("src/main.rs"),
        format!("fn main() {{ println!(\"{{}}\", {crate_name}::value()); }}\n"),
    )
    .expect("main source");
    if let Some(build_script) = build_script {
        fs::write(root.join("build.rs"), build_script).expect("build script");
    }
}

#[test]
fn env_macro_variable_is_keyed_and_reaches_the_compile() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let body = "env!(\"ENVKEY_FIXTURE_VALUE\")";
    for worktree in ["producer", "changed", "original"] {
        write_package(&roots.path().join(worktree), "envkey-app", body, None);
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
            "\"\"",
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
    let body = "env!(\"CARGO_PKG_NAME\")";
    write_package(&roots.path().join("first"), "envkey-shell", body, None);
    write_package(
        &roots.path().join("second-worktree"),
        "envkey-shell",
        body,
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

#[test]
fn job_scheduling_and_wrapper_settings_do_not_change_action_keys() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let body = "\"scheduled\"";
    let first_root = roots.path().join("first");
    let second_root = roots.path().join("second");
    write_package(&first_root, "envkey-jobs", body, None);
    write_package(&second_root, "envkey-jobs", body, None);

    let first = cached_build(
        &first_root,
        cache.path(),
        &[
            ("CARGO_BUILD_JOBS", "1"),
            ("CARGO_REAPI_UNRELATED_SETTING", "first"),
        ],
    );
    assert!(first.succeeded(), "{}", first.stderr());
    assert_eq!(first.executions("envkey_jobs"), ["local-cache-miss"]);

    let second = cached_build(
        &second_root,
        cache.path(),
        &[
            ("CARGO_BUILD_JOBS", "2"),
            ("CARGO_REAPI_UNRELATED_SETTING", "second"),
        ],
    );
    assert!(second.succeeded(), "{}", second.stderr());
    assert_eq!(second.executions("envkey_jobs"), ["cache-hit"]);

    let first_action = first.final_action("envkey_jobs");
    let second_action = second.final_action("envkey_jobs");
    assert_eq!(first_action["action_key"], second_action["action_key"]);
    for name in [
        "CARGO_BUILD_JOBS",
        "CARGO_MAKEFLAGS",
        "CARGO_REAPI_UNRELATED_SETTING",
    ] {
        assert!(
            first_action["keyed_environment"][name].is_null(),
            "{name} is keyed"
        );
    }
}

#[test]
fn a_compile_that_reads_an_unkeyed_scheduling_variable_is_not_stored() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let body = "env!(\"CARGO_BUILD_JOBS\")";
    let first_root = roots.path().join("first");
    let second_root = roots.path().join("second");
    write_package(&first_root, "envkey-reads-jobs", body, None);
    write_package(&second_root, "envkey-reads-jobs", body, None);

    let first = cached_build(&first_root, cache.path(), &[("CARGO_BUILD_JOBS", "1")]);
    assert!(first.succeeded(), "{}", first.stderr());
    assert_eq!(
        first.executions("envkey_reads_jobs"),
        ["local-output-incomplete"]
    );
    assert_eq!(
        run_binary(&first_root.join("target/debug/envkey-reads-jobs")),
        "1"
    );

    // The key leaves the variable out, so only not storing the compile keeps
    // the second build from restoring the first one's value.
    let second = cached_build(&second_root, cache.path(), &[("CARGO_BUILD_JOBS", "2")]);
    assert!(second.succeeded(), "{}", second.stderr());
    assert_eq!(
        run_binary(&second_root.join("target/debug/envkey-reads-jobs")),
        "2"
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
        root.join("app/src/lib.rs"),
        "pub fn value() -> u64 { envkey_macro::untracked_value!() }\n",
    )
    .expect("app library");
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", envkey_macro_app::value()); }\n",
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

#[cfg(unix)]
#[test]
fn a_search_path_reaches_the_compile_exactly_as_it_is_keyed() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let fresh_cache = tempdir().expect("separate cache");
    let body = "env!(\"PATH\")";
    for worktree in ["first", "second-worktree", "fresh"] {
        write_package(&roots.path().join(worktree), "envkey-path", body, None);
    }

    let tools = roots.path().join("tools");
    fs::create_dir(&tools).expect("tool directory");
    let first_link = roots.path().join("shell-1-tools");
    let second_link = roots.path().join("shell-2-tools");
    std::os::unix::fs::symlink(&tools, &first_link).expect("first shell link");
    std::os::unix::fs::symlink(&tools, &second_link).expect("second shell link");
    let path = std::env::var_os("PATH").expect("PATH");
    let first_path = prepend_path(&first_link, &path);
    let second_path = prepend_path(&second_link, &path);

    let first = cached_build(
        &roots.path().join("first"),
        cache.path(),
        &[("PATH", first_path.as_str())],
    );
    assert!(first.succeeded(), "{}", first.stderr());

    // Both lists resolve through the same directories, so the second shell
    // reuses the first one's compile.
    let second = cached_build(
        &roots.path().join("second-worktree"),
        cache.path(),
        &[("PATH", second_path.as_str())],
    );
    assert!(second.succeeded(), "{}", second.stderr());
    assert_eq!(second.executions("envkey_path"), ["cache-hit"]);

    // That reuse is sound only if a compile in the second shell's
    // environment embeds exactly what the reused artifact embeds.
    let fresh = cached_build(
        &roots.path().join("fresh"),
        fresh_cache.path(),
        &[("PATH", second_path.as_str())],
    );
    assert!(fresh.succeeded(), "{}", fresh.stderr());
    assert_eq!(fresh.executions("envkey_path"), ["local-cache-miss"]);
    assert_eq!(
        run_binary(
            &roots
                .path()
                .join("second-worktree/target/debug/envkey-path")
        ),
        run_binary(&roots.path().join("fresh/target/debug/envkey-path"))
    );
}

/// A workspace whose application prints what a proc macro read, through
/// `std::env`, from the variables `names`. rustc cannot track those reads.
fn write_reading_macro_workspace(root: &Path, names: &[&str], app_prelude: &str) {
    fs::create_dir_all(root.join("macro/src")).expect("macro source");
    fs::create_dir_all(root.join("app/src")).expect("app source");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['macro','app']\nresolver='3'\n",
    )
    .expect("workspace manifest");
    fs::write(
        root.join("macro/Cargo.toml"),
        "[package]\nname='envkey-reader'\nversion='0.0.0'\nedition='2024'\n[lib]\nproc-macro=true\n",
    )
    .expect("macro manifest");
    let names = names
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        root.join("macro/src/lib.rs"),
        format!(
            r#"extern crate proc_macro;
use proc_macro::TokenStream;
#[proc_macro]
pub fn read_values(_: TokenStream) -> TokenStream {{
    let values = [{names}]
        .iter()
        .map(|name| std::env::var(name).unwrap_or_else(|_| "MISSING".to_owned()))
        .collect::<Vec<_>>()
        .join("/");
    format!("{{values:?}}").parse().unwrap()
}}
"#
        ),
    )
    .expect("macro source");
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname='envkey-reader-app'\nversion='0.0.0'\nedition='2024'\n\
         [features]\ntracked=[]\n[dependencies]\nenvkey-reader={path='../macro'}\n",
    )
    .expect("app manifest");
    fs::write(
        root.join("app/src/lib.rs"),
        format!("{app_prelude}\npub const VALUE: &str = envkey_reader::read_values!();\n"),
    )
    .expect("app library");
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", envkey_reader_app::VALUE); }\n",
    )
    .expect("app source");
}

#[test]
fn a_learned_variable_retires_entries_cached_without_it() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let workspace = roots.path().join("workspace");
    let later = roots.path().join("later-worktree");
    for root in [&workspace, &later] {
        write_reading_macro_workspace(
            root,
            &["ENVKEY_LEARNED_VALUE"],
            "#[cfg(feature = \"tracked\")]\nconst _TRACKED: &str = env!(\"ENVKEY_LEARNED_VALUE\");\n",
        );
    }
    let binary = workspace.join("target/debug/envkey-reader-app");
    let environment = [("ENVKEY_LEARNED_VALUE", "configured")];
    let default_build = ["build", "-p", "envkey-reader-app"];

    // Nothing marks the proc macro's read, so the variable is withheld.
    let untracked = cargo_build(&workspace, cache.path(), &default_build, &environment);
    assert!(untracked.succeeded(), "{}", untracked.stderr());
    assert_eq!(run_binary(&binary), "MISSING");

    // The feature's env! reveals the variable, and later compiles of the
    // crate receive it.
    let tracked = cargo_build(
        &workspace,
        cache.path(),
        &["build", "-p", "envkey-reader-app", "--features", "tracked"],
        &environment,
    );
    assert!(tracked.succeeded(), "{}", tracked.stderr());
    assert_eq!(run_binary(&binary), "configured");

    // The first build cached a compile without the variable. Later compiles
    // of the crate pass it, so a later worktree never restores that entry.
    let repeated = cargo_build(&later, cache.path(), &default_build, &environment);
    assert!(repeated.succeeded(), "{}", repeated.stderr());
    assert_eq!(
        repeated.executions("envkey_reader_app"),
        ["local-cache-miss"]
    );
    assert_eq!(
        run_binary(&later.join("target/debug/envkey-reader-app")),
        "configured"
    );
}

#[test]
fn cargo_env_configuration_reaches_the_compile_even_when_the_caller_sets_it() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let workspace = roots.path().join("workspace");
    write_reading_macro_workspace(&workspace, &["ENVKEY_FORCED", "ENVKEY_DEFAULTED"], "");
    fs::create_dir_all(workspace.join(".cargo")).expect("Cargo configuration directory");
    fs::write(
        workspace.join(".cargo/config.toml"),
        "[env]\nENVKEY_FORCED = { value = \"configured\", force = true }\nENVKEY_DEFAULTED = \"configured\"\n",
    )
    .expect("Cargo configuration");

    // Plain Cargo gives the proc macro the forced value, which equals the
    // caller's, and the caller's value of the unforced variable.
    let build = cached_build(
        &workspace,
        cache.path(),
        &[
            ("ENVKEY_FORCED", "configured"),
            ("ENVKEY_DEFAULTED", "caller"),
        ],
    );
    assert!(build.succeeded(), "{}", build.stderr());
    assert_eq!(
        run_binary(&workspace.join("target/debug/envkey-reader-app")),
        "configured/caller"
    );
}

#[test]
fn concurrently_learned_variables_all_survive() {
    const BUILDS: usize = 24;
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let names = (0..BUILDS)
        .map(|index| format!("ENVKEY_LEARNED_{index}"))
        .collect::<Vec<_>>();
    let environment = names
        .iter()
        .map(|name| (name.as_str(), "value"))
        .collect::<Vec<_>>();

    // One crate whose variants each read a different variable, so every
    // build learns a different name for the same crate.
    let body = |name: &str| format!("env!(\"{name}\")");
    for (index, name) in names.iter().enumerate() {
        for kind in ["learn", "check"] {
            write_package(
                &roots.path().join(format!("{kind}-{index}")),
                "envkey-learned",
                &body(name),
                None,
            );
        }
    }

    std::thread::scope(|scope| {
        let builds = (0..BUILDS)
            .map(|index| {
                let root = roots.path().join(format!("learn-{index}"));
                let mut command = cargo_reapi(&root, cache.path(), &["build"], &environment);
                scope.spawn(move || command.output().expect("run cargo-reapi"))
            })
            .collect::<Vec<_>>();
        for build in builds {
            let output = build.join().expect("build thread");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    });

    // Every learned name must still be passed from the start, so no later
    // compile has to rediscover it.
    for (index, name) in names.iter().enumerate() {
        let check = cached_build(
            &roots.path().join(format!("check-{index}")),
            cache.path(),
            &environment,
        );
        assert!(check.succeeded(), "{}", check.stderr());
        let executions = check.executions("envkey_learned");
        assert!(
            !executions.contains(&"local-environment-withheld".to_owned()),
            "{name} was not remembered: {executions:?}"
        );
    }
}

#[test]
fn a_variable_that_can_never_be_passed_ends_learning() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let root = roots.path().join("private");
    // cargo-reapi sets its backend for its own wrapper, never for compiles,
    // so this crate cannot compile through it, as under plain Cargo.
    write_package(
        &root,
        "envkey-private",
        "env!(\"CARGO_REAPI_BACKEND\")",
        None,
    );

    let mut child = cargo_reapi(&root, cache.path(), &["build"], &[])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start cargo-reapi");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll cargo-reapi") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().expect("stop cargo-reapi");
            child.wait().expect("reap cargo-reapi");
            panic!("learning retried the compile without end");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    assert!(!status.success());
    let build = finished_build(
        &root,
        Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    );
    assert_eq!(build.executions("envkey_private"), ["local-failed"]);
}

#[test]
fn a_sibling_of_the_package_named_in_a_variable_keeps_its_path() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    // `foo-assets` begins with the package root `foo`, but is not beneath it.
    let build_script = "fn main() {\n\
         let manifest = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n\
         println!(\"cargo::rustc-env=ASSET_DIR={manifest}-assets/data\");\n\
         }\n";
    for name in ["foo", "bar"] {
        write_package(
            &roots.path().join(name),
            "envkey-sibling",
            "env!(\"ASSET_DIR\")",
            Some(build_script),
        );
    }

    for name in ["foo", "bar"] {
        let root = roots.path().join(name);
        let built = cached_build(&root, cache.path(), &[]);
        assert!(built.succeeded(), "{}", built.stderr());

        let expected = format!(
            "{}-assets/data",
            root.canonicalize().expect("package root").display()
        );
        assert_eq!(
            run_binary(&root.join("target/debug/envkey-sibling")),
            expected
        );
    }
}

#[cfg(unix)]
#[test]
fn variables_that_are_not_utf8_neither_stop_the_wrapper_nor_share_a_key() {
    use std::os::unix::ffi::OsStrExt;

    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let body = "env!(\"CARGO_PKG_NAME\")";
    for name in ["first", "second"] {
        write_package(&roots.path().join(name), "envkey-bytes", body, None);
    }
    // Each value is one byte that is not UTF-8; read lossily, both would be
    // the same replacement character.
    let build = |name: &str, keyed: &[u8]| {
        let root = roots.path().join(name);
        let mut command = cargo_reapi(&root, cache.path(), &["build"], &[]);
        command
            .env(
                "ENVKEY_UNRELATED_BYTES",
                std::ffi::OsStr::from_bytes(b"\xff"),
            )
            .env("RUST_ENVKEY_BYTES", std::ffi::OsStr::from_bytes(keyed));
        finished_build(&root, command.output().expect("run cargo-reapi"))
    };

    let first = build("first", b"\xfe");
    assert!(first.succeeded(), "{}", first.stderr());
    assert_eq!(first.executions("envkey_bytes"), ["local-cache-miss"]);

    let second = build("second", b"\xfd");
    assert!(second.succeeded(), "{}", second.stderr());
    assert_eq!(second.executions("envkey_bytes"), ["local-cache-miss"]);
}
