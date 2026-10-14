//! Helpers shared by the integration tests.

use std::path::Path;
use std::process::Command;

/// Writes `root`'s lockfile when it has none. A cached build never creates
/// one, because preparing the project must not change it, so a fixture needs
/// its lockfile before the first gated invocation.
pub fn ensure_lockfile(root: &Path) {
    if root.join("Cargo.lock").exists() {
        return;
    }
    let status = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(root)
        .status()
        .expect("run cargo generate-lockfile");
    assert!(
        status.success(),
        "generate-lockfile failed in {}",
        root.display()
    );
}
