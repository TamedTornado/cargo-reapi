#![cfg(target_os = "linux")]

use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn stall_auditor_terminates_the_process_group_before_natural_completion() {
    let root = tempfile::tempdir().expect("temporary evidence root");
    let ledger = root.path().join("ledger");
    let report = root.path().join("stall-report.json");
    std::fs::create_dir_all(&ledger).expect("ledger root");

    let started = Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_cargo-reapi-auditor"))
        .args([
            "run",
            "--report",
            report.to_str().expect("report path"),
            "--ledger-root",
            ledger.to_str().expect("ledger path"),
            "--stall-seconds",
            "1",
            "--",
            "/bin/sleep",
            "10",
        ])
        .status()
        .expect("run stall auditor");
    let elapsed = started.elapsed();

    assert!(
        !status.success(),
        "a deliberate stall must fail the monitored command"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "stalled process survived too long: {elapsed:?}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report).expect("stall report"))
            .expect("valid report");
    assert_eq!(report["infrastructure_stall"], true);
    assert_ne!(report["exit_code"], 0);
    assert_eq!(report["passed"], false);
}
