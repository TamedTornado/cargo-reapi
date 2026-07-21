# Real-world benchmarks

Benchmark claims in this repository are evidence, not marketing estimates. The
binding correctness and timing rules live in
[`../acceptance/ACCEPTANCE_CRITERIA.md`](../acceptance/ACCEPTANCE_CRITERIA.md),
and only `cargo reapi prove aggregate` can validate both complete generated
platform receipt sets as one cross-platform result. Raw receipt sets are
disposable build artifacts: they are not committed or required to be retained
after verification and statistics extraction.

The latest current-schema macOS measurements are recorded in
[`results/2026-07-21-macos-apfs.md`](results/2026-07-21-macos-apfs.md). The
preceding-model final Linux qualification is recorded in
[`results/2026-07-20-linux-xfs.md`](results/2026-07-20-linux-xfs.md). Earlier
historical and partial results remain linked from the result files and are not
presented as current multi-platform acceptance.

Reproduction entry points:

The canonical end-to-end instructions, including the required source layout,
macOS authorization, Linux XFS setup, cross-platform aggregation, benchmark
fields, and disposal policy, are in
[`../acceptance/REPRODUCING.md`](../acceptance/REPRODUCING.md). The shorter
commands below are useful component entry points, not a substitute for that
procedure.

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
