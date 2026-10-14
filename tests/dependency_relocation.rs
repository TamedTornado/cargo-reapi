//! A dependency's paths embedded in a dependent's artifacts must name the
//! consumer's copy of that dependency after a cross-worktree cache hit.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::tempdir;

fn write_workspace(root: &Path) {
    fs::create_dir_all(root.join("dependency/src")).expect("dependency source");
    fs::create_dir_all(root.join("app/src")).expect("app source");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['dependency','app']\nresolver='3'\n",
    )
    .expect("workspace manifest");
    fs::write(
        root.join("dependency/Cargo.toml"),
        "[package]\nname='relocated-dependency'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("dependency manifest");
    fs::write(
        root.join("dependency/src/lib.rs"),
        "pub const ROOT: &str = env!(\"CARGO_MANIFEST_DIR\");\n",
    )
    .expect("dependency source");
    fs::write(
        root.join("app/Cargo.toml"),
        "[package]\nname='relocated-app'\nversion='0.0.0'\nedition='2024'\n\
         [dependencies]\nrelocated-dependency={path='../dependency'}\n",
    )
    .expect("app manifest");
    fs::write(
        root.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", relocated_dependency::ROOT); }\n",
    )
    .expect("app source");
}

fn cached_build(root: &Path, cache: &Path) -> Vec<serde_json::Value> {
    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_REAPI_ACTION_CACHE_TEST_MODE", "1")
        .args(["--backend", "cache", "--action-log"])
        .arg(&action_log)
        .arg("--cache-dir")
        .arg(cache)
        .args(["--", "build"])
        .output()
        .expect("run cargo-reapi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(&action_log)
        .expect("action log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect()
}

fn printed_root(worktree: &Path) -> PathBuf {
    let output = Command::new(worktree.join("target/debug/relocated-app"))
        .output()
        .expect("run app");
    assert!(output.status.success());
    let printed = String::from_utf8(output.stdout).expect("UTF-8 output");
    fs::canonicalize(printed.trim()).expect("printed path exists")
}

#[test]
fn restored_dependent_names_the_consumers_dependency() {
    let roots = tempdir().expect("worktree roots");
    let cache = tempdir().expect("shared cache");
    let first = roots.path().join("first");
    let second = roots.path().join("second-worktree");
    write_workspace(&first);
    write_workspace(&second);

    cached_build(&first, cache.path());
    let actions = cached_build(&second, cache.path());
    let app = actions
        .iter()
        .find(|action| action["crate_name"] == "relocated_app")
        .expect("app action");
    assert_eq!(app["execution"], "cache-hit");

    // The first worktree is gone, as an abandoned worktree is.
    fs::remove_dir_all(&first).expect("remove first worktree");
    assert_eq!(
        printed_root(&second),
        fs::canonicalize(second.join("dependency")).unwrap()
    );
}
