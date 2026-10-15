//! `cargo install` replaces cargo-reapi's executable by renaming a new file
//! over it, and agents reinstall while other builds are running. A build
//! that started before the replacement must still finish.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
use tempfile::tempdir;

/// How long the build script holds the build open. The executable is
/// replaced during that time, before any program is linked.
const BUILD_SCRIPT_HOLD_SECONDS: u64 = 8;

/// How long the test waits for Cargo to reach the build script.
const BUILD_SCRIPT_START_LIMIT: Duration = Duration::from_secs(120);

/// A package whose build script runs first and holds the build open, and
/// whose binary is linked after it.
fn write_package(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='relinked'\nversion='0.0.0'\nedition='2024'\n",
    )
    .expect("manifest");
    fs::write(
        root.join("build.rs"),
        format!(
            "fn main() {{\n    \
             std::thread::sleep(std::time::Duration::from_secs({BUILD_SCRIPT_HOLD_SECONDS}));\n\
             }}\n"
        ),
    )
    .expect("build script");
    fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"linked\"); }\n",
    )
    .expect("main source");
}

/// Installs a copy of cargo-reapi as `cargo install` would.
fn install(executable: &Path) {
    let staged = executable.with_extension("staged");
    fs::copy(env!("CARGO_BIN_EXE_cargo-reapi"), &staged).expect("stage executable");
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).expect("executable mode");
    fs::rename(&staged, executable).expect("replace executable");
}

/// Waits until Cargo has created the build script's output directory,
/// which it does just before running the script.
fn wait_for_build_script(target: &Path) {
    let started = Instant::now();
    loop {
        let running = fs::read_dir(target.join("debug/build"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| entry.path().join("out").is_dir());
        if running {
            return;
        }
        assert!(
            started.elapsed() < BUILD_SCRIPT_START_LIMIT,
            "the build script never started"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// A fixture with cargo-reapi installed in its own `bin` directory.
struct Fixture {
    root: tempfile::TempDir,
    executable: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir().expect("fixture");
        let executable = root.path().join("bin/cargo-reapi");
        let workspace = root.path().join("workspace");
        fs::create_dir_all(executable.parent().unwrap()).expect("bin directory");
        install(&executable);
        write_package(&workspace);
        Self {
            root,
            executable,
            workspace,
        }
    }

    fn cache(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    fn target(&self) -> PathBuf {
        self.workspace.join("target")
    }

    /// Starts an ordinary gated build, in the strict sandbox.
    fn start_build(&self) -> Child {
        Command::new(&self.executable)
            .current_dir(&self.workspace)
            .env_remove("CARGO_TARGET_DIR")
            .args(["--backend", "cache", "--cache-dir"])
            .arg(self.cache())
            .args(["--", "build"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start cargo-reapi")
    }

    fn assert_built(&self, build: Child) {
        let output = build.wait_with_output().expect("wait for cargo-reapi");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let program = Command::new(self.target().join("debug/relinked"))
            .output()
            .expect("run the linked program");
        assert_eq!(String::from_utf8_lossy(&program.stdout).trim(), "linked");
    }
}

/// Waits until `process` has opened `file`.
fn wait_for_open_file(process: &Child, file: &Path) {
    let descriptors = PathBuf::from(format!("/proc/{}/fd", process.id()));
    let started = Instant::now();
    loop {
        let open = fs::read_dir(&descriptors)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| fs::read_link(entry.path()).is_ok_and(|target| target == file));
        if open {
            return;
        }
        assert!(
            started.elapsed() < BUILD_SCRIPT_START_LIMIT,
            "cargo-reapi never opened {}",
            file.display()
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_build_survives_reinstalling_cargo_reapi_while_it_runs() {
    let fixture = Fixture::new();

    let build = fixture.start_build();
    wait_for_build_script(&fixture.target());
    install(&fixture.executable);

    fixture.assert_built(build);
}

/// A gate waits for cache admission, or for another producer, before Cargo
/// starts. A reinstall during that wait must not leave Cargo running the
/// new version as the compiler wrapper of the old driver.
#[cfg(target_os = "linux")]
#[test]
fn a_build_waiting_to_start_survives_reinstalling_cargo_reapi() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.cache()).expect("cache directory");
    let admission_path = fixture.cache().join(".maintenance-admission.lock");
    let admission = fs::File::create(&admission_path).expect("admission lock");
    FileExt::lock_exclusive(&admission).expect("hold cache admission");

    let build = fixture.start_build();
    wait_for_open_file(&build, &admission_path);
    install(&fixture.executable);
    FileExt::unlock(&admission).expect("release cache admission");

    fixture.assert_built(build);
}
