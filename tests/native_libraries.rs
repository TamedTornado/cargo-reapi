//! Native libraries a compile reads: a link step always runs, a metadata-only
//! compile reads none, an rlib that bundles a static library from outside the
//! workspace and toolchain runs uncached, an rlib that only searches outside
//! is cached, and a static library an rlib bundles from inside the workspace
//! keys the compile.

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
    cached_cargo(root, cache, "build")
}

/// Runs `cargo <command>` through the cache backend with a fresh action log.
fn cached_cargo(root: &Path, cache: &Path, command: &str) -> Build {
    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let _ = fs::remove_file(&action_log);

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(root)
        .env_remove("CARGO_TARGET_DIR")
        .args(["--backend", "cache", "--action-log"])
        .arg(&action_log)
        .arg("--cache-dir")
        .arg(cache)
        .args(["--", command])
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

/// How the `native-library` package reaches `libanswer.a`.
#[derive(Clone, Copy)]
enum Bundling {
    /// `#[link(name = "answer", kind = "static")]` in the library source.
    LinkAttribute,
    /// `cargo::rustc-link-lib=static=answer` from the build script.
    LinkLibArgument,
    /// Nothing: the library searches the directory, names the system `libm`
    /// with a default-kind `-l m`, which only the link step reads, and
    /// returns 7.
    DynamicOnly,
}

/// A library that searches `search` (relative to the package) for native
/// libraries and bundles `libanswer.a` from it as `bundling` says, and a
/// binary that prints what it returns.
fn write_native_library(root: &Path, search: &str, bundling: Bundling) {
    write_manifest(root, "native-library");

    let link_lib = match bundling {
        Bundling::LinkLibArgument => "println!(\"cargo::rustc-link-lib=static=answer\");\n",
        Bundling::DynamicOnly => "println!(\"cargo::rustc-link-lib=m\");\n",
        Bundling::LinkAttribute => "",
    };
    fs::write(
        root.join("build.rs"),
        format!(
            "fn main() {{\n\
             let manifest = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n\
             let search = std::path::Path::new(&manifest).join({search:?});\n\
             println!(\"cargo::rustc-link-search=native={{}}\", search.display());\n\
             println!(\"cargo::rerun-if-changed={{}}\", search.join(\"libanswer.a\").display());\n\
             {link_lib}\
             }}\n"
        ),
    )
    .expect("build script");

    let library = match bundling {
        Bundling::LinkAttribute => {
            "#[link(name = \"answer\", kind = \"static\")]\n\
             unsafe extern \"C\" { fn answer() -> i32; }\n\
             pub fn value() -> i32 { unsafe { answer() } }\n"
        }
        Bundling::LinkLibArgument => {
            "unsafe extern \"C\" { fn answer() -> i32; }\n\
             pub fn value() -> i32 { unsafe { answer() } }\n"
        }
        Bundling::DynamicOnly => "pub fn value() -> i32 { 7 }\n",
    };
    fs::write(root.join("src/lib.rs"), library).expect("library source");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"{}\", native_library::value()); }\n",
    )
    .expect("binary source");
}

/// A library that bundles `libanswer.a` from `search` through `#[link]`.
fn write_bundling_library(root: &Path, search: &str) {
    write_native_library(root, search, Bundling::LinkAttribute);
}

/// The library compile's executions: the one action of `native_library`
/// with `--crate-type lib`.
fn library_executions(build: &Build) -> Vec<String> {
    compile_executions(build, "native_library", "lib")
}

/// The executions of `crate_name`'s compiles with `--crate-type crate_type`.
fn compile_executions(build: &Build, crate_name: &str, crate_type: &str) -> Vec<String> {
    let crate_type = format!("\"--crate-type\",\"{crate_type}\"");
    build
        .actions
        .iter()
        .filter(|action| {
            action["crate_name"] == crate_name
                && action["arguments"].to_string().contains(&crate_type)
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

/// A workspace of `native-library`, which names `libanswer.a` in
/// `../../outside` with `-l static=answer`, and `native-user`, a library that
/// calls it and so inherits its `-L` directory from Cargo. A check reads
/// neither.
fn write_propagating_workspace(root: &Path) {
    fs::create_dir_all(root).expect("workspace directory");
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = ['native', 'user']\nresolver = '3'\n",
    )
    .expect("workspace manifest");
    write_native_library(
        &root.join("native"),
        "../../outside",
        Bundling::LinkLibArgument,
    );

    let user = root.join("user");
    write_manifest(&user, "native-user");
    let mut manifest = fs::read_to_string(user.join("Cargo.toml")).expect("user manifest");
    manifest.push_str("[dependencies]\nnative-library = { path = '../native' }\n");
    fs::write(user.join("Cargo.toml"), manifest).expect("user manifest");
    fs::write(
        user.join("src/lib.rs"),
        "pub fn twice() -> i32 { native_library::value() * 2 }\n",
    )
    .expect("user source");
}

#[test]
fn a_check_with_a_propagated_outside_search_directory_is_a_hit_the_second_time() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    write_answer_archive(&roots.path().join("outside"), 42);
    let first = roots.path().join("first");
    let second = roots.path().join("second");
    write_propagating_workspace(&first);
    write_propagating_workspace(&second);

    let checked = cached_cargo(&first, cache.path(), "check");
    checked.assert_succeeded();
    assert_eq!(
        compile_executions(&checked, "native_user", "lib"),
        ["local-cache-miss"]
    );
    let user_arguments = checked
        .actions
        .iter()
        .find(|action| action["crate_name"] == "native_user")
        .expect("native_user compile")["arguments"]
        .to_string();
    assert!(
        user_arguments.contains("native="),
        "the dependent compile inherits the -L directory: {user_arguments}"
    );

    let rechecked = cached_cargo(&second, cache.path(), "check");
    rechecked.assert_succeeded();
    assert_eq!(library_executions(&rechecked), ["cache-hit"]);
    assert_eq!(
        compile_executions(&rechecked, "native_user", "lib"),
        ["cache-hit"]
    );
}

#[test]
fn a_library_searching_an_outside_directory_without_bundling_is_cached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    write_answer_archive(&roots.path().join("outside"), 42);
    let first = roots.path().join("first");
    let second = roots.path().join("second");
    write_native_library(&first, "../outside", Bundling::DynamicOnly);
    write_native_library(&second, "../outside", Bundling::DynamicOnly);

    let built = cached_build(&first, cache.path());
    built.assert_succeeded();
    assert_eq!(library_executions(&built), ["local-cache-miss"]);

    let restored = cached_build(&second, cache.path());
    restored.assert_succeeded();
    assert_eq!(library_executions(&restored), ["cache-hit"]);
    assert_eq!(
        run_program(&second.join("target/debug/native-library")),
        "7"
    );
}

#[test]
fn a_static_library_named_by_link_lib_inside_the_workspace_keys_the_compile() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    let workspace = roots.path().join("workspace");
    let copy = roots.path().join("copy");
    for root in [&workspace, &copy] {
        write_answer_archive(&root.join("native"), 42);
        write_native_library(
            &root.join("package"),
            "../native",
            Bundling::LinkLibArgument,
        );
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = ['package']\nresolver = '3'\n",
        )
        .expect("workspace manifest");
    }

    let built = cached_build(&workspace, cache.path());
    built.assert_succeeded();
    assert_eq!(library_executions(&built), ["local-cache-miss"]);

    let restored = cached_build(&copy, cache.path());
    restored.assert_succeeded();
    assert_eq!(library_executions(&restored), ["cache-hit"]);

    // Replacing the archive the `-l` argument names changes the key.
    write_answer_archive(&copy.join("native"), 43);
    let rebuilt = cached_build(&copy, cache.path());

    rebuilt.assert_succeeded();
    assert_eq!(library_executions(&rebuilt), ["local-cache-miss"]);
    assert_eq!(run_program(&copy.join("target/debug/native-library")), "43");
}

#[test]
fn a_library_naming_an_outside_static_archive_by_link_lib_runs_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    write_answer_archive(&roots.path().join("outside"), 42);
    let root = roots.path().join("package");
    write_native_library(&root, "../outside", Bundling::LinkLibArgument);

    let built = cached_build(&root, cache.path());

    built.assert_succeeded();
    assert_eq!(library_executions(&built), ["local-ineligible"]);
    assert_eq!(run_program(&root.join("target/debug/native-library")), "42");
}

/// A compiler installed in `<prefix>/bin`, as a distribution installs
/// `/usr/bin/rustc`: a script that runs the real rustc.
fn write_prefixed_compiler(prefix: &Path) -> PathBuf {
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("ask rustc for its sysroot");
    let real = PathBuf::from(
        String::from_utf8(sysroot.stdout)
            .expect("UTF-8 sysroot")
            .trim(),
    )
    .join("bin/rustc");

    let compiler = prefix.join("bin/rustc");
    fs::create_dir_all(compiler.parent().expect("bin directory")).expect("bin directory");
    fs::write(
        &compiler,
        format!("#!/bin/sh\nexec '{}' \"$@\"\n", real.display()),
    )
    .expect("compiler script");
    let mut permissions = fs::metadata(&compiler).expect("script").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    fs::set_permissions(&compiler, permissions).expect("make the script executable");
    compiler
}

#[test]
fn a_library_bundling_an_archive_from_the_compilers_installation_prefix_runs_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    let prefix = roots.path().join("usr");
    let compiler = write_prefixed_compiler(&prefix);
    // A system library directory beside the compiler, like
    // /usr/lib/x86_64-linux-gnu beside /usr/bin/rustc.
    let system_libraries = prefix.join("lib/system");
    write_answer_archive(&system_libraries, 42);
    let root = roots.path().join("package");
    write_native_library(
        &root,
        system_libraries.to_str().expect("UTF-8 path"),
        Bundling::LinkLibArgument,
    );

    let action_log = root.join("target/cargo-reapi/actions.jsonl");
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-reapi"))
        .current_dir(&root)
        .env_remove("CARGO_TARGET_DIR")
        .env("RUSTC", &compiler)
        .args(["--backend", "cache", "--action-log"])
        .arg(&action_log)
        .arg("--cache-dir")
        .arg(cache.path())
        .args(["--", "build"])
        .output()
        .expect("run cargo-reapi");
    let actions = fs::read_to_string(&action_log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSON action"))
        .collect();
    let built = Build { output, actions };

    built.assert_succeeded();
    assert_eq!(library_executions(&built), ["local-ineligible"]);
    assert_eq!(run_program(&root.join("target/debug/native-library")), "42");
}

#[test]
fn a_thin_archive_inside_the_workspace_runs_uncached() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    let workspace = roots.path().join("workspace");
    write_bundling_library(&workspace.join("package"), "../native");
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = ['package']\nresolver = '3'\n",
    )
    .expect("workspace manifest");

    // The thin archive inside the workspace names an object outside it.
    let objects = roots.path().join("objects");
    write_answer_archive(&objects, 42);
    let native = workspace.join("native");
    fs::create_dir_all(&native).expect("native directory");
    let archived = Command::new("ar")
        .current_dir(&native)
        .arg("rcsT")
        .arg("libanswer.a")
        .arg(objects.join("answer.o"))
        .status()
        .expect("run ar");
    assert!(archived.success(), "ar failed");

    let built = cached_build(&workspace, cache.path());

    built.assert_succeeded();
    assert_eq!(library_executions(&built), ["local-ineligible"]);
    assert_eq!(
        run_program(&workspace.join("target/debug/native-library")),
        "42"
    );
}

#[test]
fn a_search_directory_reached_through_a_link_and_its_parent_keys_the_compile() {
    let roots = tempdir().expect("roots");
    let cache = tempdir().expect("shared cache");
    let workspace = roots.path().join("workspace");
    // `native/current/../lib` is `native/v1/lib` to the kernel, since
    // `current` links to `v1/build`; collapsed as text it would be
    // `native/lib`, which does not exist.
    write_bundling_library(&workspace.join("package"), "../native/current/../lib");
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = ['package']\nresolver = '3'\n",
    )
    .expect("workspace manifest");
    let native = workspace.join("native");
    fs::create_dir_all(native.join("v1/build")).expect("build directory");
    std::os::unix::fs::symlink("v1/build", native.join("current")).expect("current link");
    write_answer_archive(&native.join("v1/lib"), 42);

    let built = cached_build(&workspace, cache.path());
    built.assert_succeeded();
    assert_eq!(library_executions(&built), ["local-cache-miss"]);

    // Replacing the archive the compile bundled changes its key.
    write_answer_archive(&native.join("v1/lib"), 43);
    let rebuilt = cached_build(&workspace, cache.path());

    rebuilt.assert_succeeded();
    assert_eq!(library_executions(&rebuilt), ["local-cache-miss"]);
    assert_eq!(
        run_program(&workspace.join("target/debug/native-library")),
        "43"
    );
}
