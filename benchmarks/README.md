# Real-world benchmarks

Benchmark claims in this repository are evidence, not marketing estimates. The
binding correctness and timing rules live in
[`../acceptance/ACCEPTANCE_CRITERIA.md`](../acceptance/ACCEPTANCE_CRITERIA.md),
and only `cargo reapi prove complete` can turn the full receipt set into an
acceptance result.

The current local measurements and exact workload definitions are recorded in
[`results/2026-07-19-local.md`](results/2026-07-19-local.md). Raw reports are
retained outside the Git repository because Moria target snapshots are large;
the result page names their locations and records which contract digest they
used.

Reproduction entry points:

```sh
# Correctness, invalidation, coalescing, and portable-copy tests
cargo test --all-targets

# Pinned Bevy application and integration-test binary parity
cargo test --test bevy \
  bevy_linked_artifact_restores_after_producer_deletion \
  -- --ignored --nocapture

# Real Moria SSD populations; requires sudo-authorized eslogger
sudo -v
target/release/cargo-reapi-auditor run \
  --report /path/to/report/resource-proof.json -- \
  acceptance/run-moria-local.sh \
  /path/to/moria /path/to/cache /path/to/report ssd
```
