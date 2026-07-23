# Launch preflight — 2026-07-23

This report records the repository pass against
`cargo-reapi-launch-preflight.md`. It distinguishes completed hardening from
requirements that the retained evidence cannot satisfy.

## Result

**UNMET: the 74-record production observation cannot be reconciled exactly.**

The retained terminal transcript proves that the execution histogram was read
at 2026-07-22 18:42:18 UTC, when the growing log contained 68 records:

- 31 `cache-hit`
- 10 `coalesced-hit`
- 21 `local-cache-miss`
- 6 `local-ineligible`

A separate line count at 18:42:32 UTC observed 74 records, while the
`local-ineligible` count remained 6. Six eligible records were appended between
those observations, but the execution histogram was not sampled again. The raw
production log was later disposed of, and recovery searches of the original and
migrated Docker volumes, `/srv/bro`, `/home/chandler`, and the operator
transcript did not recover those final execution outcomes.

The public Markdown and machine-readable JSON therefore retain both
observations and mark the 74-record result `UNRECONCILED`. No record was
deleted, the total was not adjusted to fit the histogram, and no category was
invented. This leaves preflight requirement 1 and the checklist's completion
definition unmet.

All other preflight requirements passed.

## Requirement checklist

1. **Record reconciliation — UNMET.** The production Markdown and JSON agree
   on the complete 68-record histogram and the later 74-record line count.
   Both explicitly identify the six intervening eligible records as
   unreconciled because their execution outcomes are no longer retained.
2. **Two-layer reuse explanation — PASS.** The README now explains exact
   whole-gate restoration and action-level reuse during Cargo replanning before
   presenting benchmarks. It states the observable JSONL events for each path
   and discloses that a human-readable end-of-gate action summary is not yet
   implemented.
3. **“Legitimate” producer misses — PASS by claim removal.** No public
   Markdown or benchmark JSON calls the 21 observations “legitimate.” They are
   reported only as producer misses because retained key-difference evidence is
   insufficient to support the stronger adjective.
4. **Failure direction — PASS.** The dogfood defect records show extra misses,
   ineligible actions, or loud build failure for environment leakage, ephemeral
   session values, target-mount disagreement, and `/etc/alternatives`
   indirection. None records a stale artifact or false cache hit. The case study
   now states that every observed integration defect failed in the safe
   direction: availability, never correctness.
5. **Cold-reader link and claim pass — PASS.** File-by-file results and the
   clean-checkout execution are recorded below.
6. **GC telemetry limitation — PASS.** The README plainly discloses
   operator-configured cache thresholds, minimal large-cache GC progress
   telemetry, and the absent human-readable end-of-gate summary.

## Cold-reader traversal

The link checker resolved 33 Markdown links and anchors with zero local
failures. The only external project link is the public Moria repository; it was
also verified with `git ls-remote`. No launch document links to a private Bro
resource or a local filesystem path.

- `README.md` — fixed the two-layer explanation, observable output, current
  capture/cache-mode status, GC telemetry disclosure, and public-name check
  date.
- `docs/case-studies/moria-agent-fleet.md` — fixed the safe failure-direction
  statement and preserved the 68/74 distinction as `UNRECONCILED`.
- `benchmarks/README.md` — links resolve; current and historical evidence
  classes remain distinct.
- `benchmarks/results/2026-07-19-local.md` — links resolve; earlier
  15.431s / 54.808s / 110.513s figures remain explicitly historical and
  unsubstantiated.
- `benchmarks/results/2026-07-20-linux-ext4.md` — links resolve; fallback-copy
  scope is unchanged.
- `benchmarks/results/2026-07-20-linux-xfs.md` — links resolve; historical
  qualification scope is unchanged.
- `benchmarks/results/2026-07-21-linux-xfs-schema-v3.md` — links resolve;
  independent Linux qualification status remains PASS.
- `benchmarks/results/2026-07-21-macos-apfs.md` — corrected a stale statement
  that Linux qualification was pending. It now distinguishes the macOS PASS,
  the later independent Linux PASS, and the UNMET combined aggregate caused by
  disposal of the macOS raw evidence tree.
- `benchmarks/results/2026-07-22-bro-moria-production.md` — added the complete
  68-record histogram and the later unreconciled 74-record observation.
- `benchmarks/results/2026-07-22-bro-moria-production.json` — added the same
  machine-readable status and counts. Arithmetic checks confirm histogram
  `31 + 10 + 21 + 6 = 68`, cacheable `68 - 6 = 62`, reused `31 + 10 = 41`,
  and later unresolved delta `74 - 68 = 6`.
- `ACCEPTANCE_CRITERIA.md` — status agrees with the README: macOS and Linux
  independently passed; publication-grade combined aggregate acceptance is not
  achieved.
- `COVERAGE.md` — platform and aggregate status agrees with the README and
  qualification reports.
- `REPRODUCING.md` — executed from clean sibling checkouts as described below,
  rather than inspected only.

The historical 15.431s / 54.808s / 110.513s figures occur only in explicitly
labeled historical contexts:
`docs/moria-acceptance-2026-07-19.md`,
`benchmarks/results/2026-07-19-local.md`, and
`acceptance/legacy-evidence-classification.json`.

## Clean-checkout reproduction

The macOS procedure in `REPRODUCING.md` was executed from disposable sibling
checkouts on an APFS SSD:

- cargo-reapi: `c004fc52ab349b1c12c2622ce6b4c257580b9c9a`
- Bro: `fdeeff3622b16037d2a571bc17ea09b2f3be7f77`
- Moria: `42450dacf1a41a7f9bec44dfa3a2f96eb6d2e06e`
- Rust/Cargo: 1.97.1, `aarch64-apple-darwin`
- Node: 22.23.1
- pnpm: 11.11.0

The Bro dependency installation and locked release build completed from those
clean checkouts. The qualification command was:

```sh
./acceptance/run-platform-qualification.sh \
  /path/to/clean/moria \
  /path/to/clean/bro \
  /path/to/disposable/cache \
  /path/to/disposable/evidence \
  ssd
```

Batch `cargo-reapi-macos-arm64-20260723T074242Z` completed in 6,811,846 ms with
status `PASS`, 11 recursively hashed receipts, and zero aggregate violations.
The runner recorded its intrinsic digest, the criteria identity, source
revision, and start timestamp before each experiment. The generated raw
evidence and cache are disposable; the scripts and this invocation are the
reproduction contract.

Selected measurements:

| Receipt | Result | Measurement |
| --- | --- | --- |
| Moria, 1 consumer | PASS | 13.512s; zero physical actions; zero OS-observed compiler/linker events |
| Moria, 5 consumers | PASS | 39.066s; zero physical actions; zero OS-observed compiler/linker events |
| Moria, 10 consumers | PASS | 73.592s; zero physical actions; zero OS-observed compiler/linker events |
| Bro, 5 consumers | PASS | 38.019s; zero physical actions; zero OS-observed compiler/linker events |
| Bevy binary integrity | PASS | 30.312s warm; relocated behavior matched fresh control |
| Resource ledger | PASS | 3,936,468,992-byte peak aggregate RSS; zero swap growth; deliberate 300s stall terminated |

The remaining PASS receipts cover the environment, APFS clone selection,
isolated portable-copy fallback, adversarial suite, and exact coalescing. The
adversarial receipt includes OS-derived exact mutation attribution, wrapper
cross-check equality, poison rejection, flag and Cargo-configuration
invalidation, external/generated input handling, and undeclared filesystem and
network denial.

## Additional launch checks

- `cargo search cargo-reapi --limit 10` returned no package.
- GitHub's exact-name repository search returned only this repository.
- The public-name check remains advisory rather than a crates.io reservation
  and must be repeated immediately before publication.

