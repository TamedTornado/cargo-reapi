# Real-world benchmarks

Benchmark claims in this repository are evidence, not marketing estimates. The
binding correctness and timing rules live in
[`../acceptance/ACCEPTANCE_CRITERIA.md`](../acceptance/ACCEPTANCE_CRITERIA.md),
and only `cargo reapi prove aggregate` can turn both complete platform receipt
sets into publication-grade acceptance.

The latest current-schema macOS measurements are recorded in
[`results/2026-07-21-macos-apfs.md`](results/2026-07-21-macos-apfs.md). The
preceding-model final Linux qualification is recorded in
[`results/2026-07-20-linux-xfs.md`](results/2026-07-20-linux-xfs.md). Earlier
historical and partial results remain linked from the result files and are not
presented as current multi-platform acceptance.

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
