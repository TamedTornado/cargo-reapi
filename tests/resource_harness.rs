#[test]
fn cold_resource_receipt_runs_two_distinct_moria_gates_concurrently() {
    let cold_work = include_str!("../acceptance/run-resource-cold-work.sh");
    let receipt = include_str!("../acceptance/run-resources.sh");
    let platform = include_str!("../acceptance/run-platform-qualification.sh");

    assert!(cold_work.contains("run_check_lane lane-a \"$lane_a\" 1"));
    assert!(cold_work.contains("run_check_lane lane-b \"$lane_b\" 2"));
    assert!(cold_work.contains("export CARGO_PROFILE_DEV_DEBUG=\"$debug\""));
    assert!(cold_work.contains(
        "mkdir -p \"$report_root/lane-a-rustc-trace\" \"$report_root/lane-b-rustc-trace\""
    ));
    assert!(cold_work.contains("pid_a=$!"));
    assert!(cold_work.contains("pid_b=$!"));
    assert!(cold_work.contains("wait \"$pid_a\""));
    assert!(cold_work.contains("wait \"$pid_b\""));
    assert!(cold_work.contains("target_empty_at_start == true"));
    assert!(cold_work.contains("local-cache-miss"));
    assert!(cold_work.contains("check --all-targets"));
    assert!(cold_work.contains("test --no-run"));
    assert!(cold_work.contains("bevy-fixture"));

    assert!(receipt.contains("\"$resource_auditor\" run"));
    assert!(receipt.contains("peak_simultaneous_progress_processes >= 2"));
    assert!(receipt.contains("cold_work_runner:$report_root/resource-cold-work-source.sh"));
    assert!(platform.contains("$cache_dir/resource-cold"));
}
