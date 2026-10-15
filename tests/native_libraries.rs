//! Native libraries a compile reads: a link step always runs, a compile that
//! reaches outside the workspace and toolchain runs uncached, and a static
//! library an rlib bundles from inside the workspace keys the compile.

#![cfg(unix)]

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
    fn assert_succeeded(&self) {
        assert!(
            self.output.status.success(),
            "{}",
            String::from_utf8_lossy(&self.output.stderr)
        );
    }

    /// The executions logged for `crate_name`'s compiles.
    fn executions(&self, crate_name: &str) -> Vec<String> {
        self.actions
            .iter()
            .filter(|action| action["crate_name"] == crate_name)
            .map(|action| action["execution"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

/// Runs `cargo build` through the cache backend with a fresh action log.
fn cached_build(root: &Path, cache: &Path) -> Build {
    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let _ = fs::remove_file(&action_log);

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .args(["--backend", "cache", "--action-log"])
        .arg(&action_log)
        .arg("--cache-dir")
        .arg(cache)
        .args(["--", "build"])
        .output()
        .expect("run cargo-reapi");

    let actions = fs::read_to_string(&action_log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect();
    Build { output, actions }
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

/// Compiles a C function `answer` returning `value` into `libanswer.a` in
/// `directory`, replacing any archive already there.
fn write_answer_archive(directory: &Path, value: u32) -> PathBuf {
    fs::create_dir_all(directory).expect("archive directory");
    let source = directory.join("answer.c");

    let archive = directory.join("libanswer.a");
    fs::write(&source, format!("int answer(void) {{ return {value}; }}\n")).expect("C source");

    // Relative names keep the archive identical wherever it is built.
    let compiled = Command::new("cc")
        .current_dir(directory)
        .args(["-c", "answer.c", "-o", "answer.o"])
        .status()
        .expect("run cc");
    assert!(compiled.success(), "cc failed");
    let _ = fs::remove_file(&archive);
    let archived = Command::new("ar")
        .current_dir(directory)
        .args(["rcs", "libanswer.a", "answer.o"])
        .status()
        .expect("run ar");
    assert!(archived.success(), "ar failed");

    archive
}

fn write_manifest(root: &Path, name: &str) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        format!("[package]\nname='{name}'\nversion='0.0.0'\nedition='2024'\n"),
    )
    .expect("manifest");
}

/// A binary that links `archive` through a linker argument, so no `-L`
/// directory names it: only the link step reads it.
fn write_linking_binary(root: &Path, archive: &Path) {
    write_manifest(root, "native-binary");
    fs::write(
        root.join("build.rs"),
        format!(
            "fn main() {{ println!(\"cargo::rustc-link-arg={}\"); }}\n",
            archive.display()
        ),
    )
    .expect("build script");
    fs::write(
        root.join("src/main.rs"),
        "unsafe extern \"C\" { fn answer() -> i32; }\n\
         fn main() { println!(\"{}\", unsafe { answer() }); }\n",
    )
    .expect("binary source");
}

#[test]
fn an_outside_static_library_replaced_makes_the_link_step_run_fresh() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    let archive = write_answer_archive(&roots.path().join("outside"), 42);
    let first = roots.path().join("first");
    let second = roots.path().join("second");
    write_linking_binary(&first, &archive);
    write_linking_binary(&second, &archive);

    let built = cached_build(&first, cache.path());
    built.assert_succeeded();
    assert_eq!(run_program(&first.join("target/debug/native-binary")), "42");

    // Same sources and arguments, but the library the linker reads changed.
    write_answer_archive(&roots.path().join("outside"), 43);
    let rebuilt = cached_build(&second, cache.path());

    rebuilt.assert_succeeded();
    assert_eq!(rebuilt.executions("native_binary"), ["local-ineligible"]);
    assert_eq!(
        run_program(&second.join("target/debug/native-binary")),
        "43"
    );
}

/// A library that bundles `libanswer.a` from `search` (relative to the
/// package) and a binary that prints what it returns.
fn write_bundling_library(root: &Path, search: &str) {
    write_manifest(root, "native-library");
    fs::write(
        root.join("build.rs"),
        format!(
            "fn main() {{\n\
             let manifest = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n\
             let search = std::path::Path::new(&manifest).join({search:?});\n\
             println!(\"cargo::rustc-link-search=native={{}}\", search.display());\n\
             println!(\"cargo::rerun-if-changed={{}}\", search.join(\"libanswer.a\").display());\n\
             }}\n"
        ),
    )
    .expect("build script");
    fs::write(
        root.join("src/lib.rs"),
        "#[link(name = \"answer\", kind = \"static\")]\n\
         unsafe extern \"C\" { fn answer() -> i32; }\n\
         pub fn value() -> i32 { unsafe { answer() } }\n",
    )
    .expect("library source");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", native_library::value()); }\n",
    )
    .expect("binary source");
}

/// The library compile's executions: the one action of `native_library`
/// with `--crate-type lib`.
fn library_executions(build: &Build) -> Vec<String> {
    build
        .actions
        .iter()
        .filter(|action| {
            action["crate_name"] == "native_library"
                && action["arguments"]
                    .to_string()
                    .contains("\"--crate-type\",\"lib\"")
        })
        .map(|action| action["execution"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_library_bundling_an_outside_static_archive_runs_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    write_answer_archive(&roots.path().join("outside"), 42);
    let first = roots.path().join("first");
    let second = roots.path().join("second");
    write_bundling_library(&first, "../outside");
    write_bundling_library(&second, "../outside");

    let built = cached_build(&first, cache.path());
    built.assert_succeeded();
    assert_eq!(library_executions(&built), ["local-ineligible"]);
    assert_eq!(
        run_program(&first.join("target/debug/native-library")),
        "42"
    );

    write_answer_archive(&roots.path().join("outside"), 43);
    let rebuilt = cached_build(&second, cache.path());

    rebuilt.assert_succeeded();
    assert_eq!(library_executions(&rebuilt), ["local-ineligible"]);
    assert_eq!(
        run_program(&second.join("target/debug/native-library")),
        "43"
    );
}

#[test]
fn a_static_archive_bundled_from_inside_the_workspace_keys_the_compile() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    let workspace = roots.path().join("workspace");
    let copy = roots.path().join("copy");
    // The archive lies inside the workspace but outside the package and its
    // build script's output directory.
    for root in [&workspace, &copy] {
        write_answer_archive(&root.join("native"), 42);
        write_bundling_library(&root.join("package"), "../native");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = ['package']\nresolver = '3'\n",
        )
        .expect("workspace manifest");
    }

    let built = cached_build(&workspace, cache.path());
    built.assert_succeeded();
    assert_eq!(library_executions(&built), ["local-cache-miss"]);

    // An identical workspace restores the compile: an inside archive keeps
    // it cacheable.
    let restored = cached_build(&copy, cache.path());
    restored.assert_succeeded();
    assert_eq!(library_executions(&restored), ["cache-hit"]);
    assert_eq!(run_program(&copy.join("target/debug/native-library")), "42");

    // Replacing the archive alone changes the compile's key.
    write_answer_archive(&workspace.join("native"), 43);
    let rebuilt = cached_build(&workspace, cache.path());

    rebuilt.assert_succeeded();
    assert_eq!(library_executions(&rebuilt), ["local-cache-miss"]);
    assert_eq!(
        run_program(&workspace.join("target/debug/native-library")),
        "43"
    );
}
