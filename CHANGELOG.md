# Changelog

All notable changes to `cargo-reapi` are documented in this file.

## [Unreleased]

## [0.2.2] - 2026-10-06

Upgrade collectors and anything that reads their reports together: report
`schema_version` is now 2 and `total_bytes` changed meaning.

- `cache gc` and `cache stats` measure the cache's physical footprint. They
  summed file lengths, so every reflinked gate snapshot counted in full
  although it occupies almost no new disk; collections evicted hot action
  entries and blobs to free space that was never used, and the next gates
  recompiled them. On Linux the footprint comes from each file's extent map
  and counts every shared block once. Elsewhere, and for files without an
  exact extent map, a file's allocated size counts as unshared, which can
  over-count clones but never under-counts.
- Evicting an action is credited with the blobs no surviving action
  references; it was credited only with its manifest, so reaching the budget
  evicted far more actions than necessary. Evicting an entry whose blocks are
  still shared frees nothing and is credited with nothing.
- Free-space recovery measures available space after each removal instead of
  projecting it, since blocks still shared outside the cache stay allocated.
- Reports add `apparent_bytes`, the sum of file lengths.

## [0.2.1] - 2026-10-06

- `cache gc` removes gate snapshot staging directories abandoned by producers
  that died before publishing, and reports them as
  `removed_abandoned_staging_entries`. Such staging was never a reuse
  candidate but counted toward the cache size; once it exceeded `--max-bytes`
  on its own, every collection evicted all reusable action, blob and gate
  entries without satisfying the budget. A live producer's staging is never
  removed: it holds a shared maintenance lease until it publishes, and the
  collector removes staging only under the exclusive lease.

## [0.2.0] - 2026-10-04

Upgrade every reader and collector that shares a cache together: the cache
admission protocol changed, and older binaries do not participate in it.

- Classify a physical-action or snapshot-signing wait as an infrastructure
  stall only after 300 seconds in which no lease in the shared ledger was
  released. Queueing behind concurrent gates that are making progress no
  longer fails them.
- `CARGO_REAPI_RESOURCE_LEDGER` selects one shared physical-action ledger for
  every worker on a host, including workers with separate project caches. The
  driver, snapshot restoration and strict sandbox all honor it.
- Without explicit capacities, the ledger uses the host's detected logical CPUs
  and physical memory instead of the acceptance benchmark's reference machine.
  The driver passes its capacities into the strict sandbox; capacities above
  the detected host are rejected.
- A gate miss discards Cargo's timestamp fingerprints before replanning, so
  preserved or older source timestamps cannot hide changed content.
- A waiting cache collector closes admission before draining existing readers,
  so collection completes under continuous compilation instead of starving.
  Snapshot waiters release their maintenance lease before waiting for a
  producer, preserving lock ordering.
- Shared-cache temporary paths and clone probes are reserved atomically rather
  than by process ID, which is not unique across containers. Cache statistics
  tolerate files disappearing during concurrent publication and cleanup.
- The strict sandbox's control sockets live beneath the caller's `TMPDIR`, with
  an early error when that path is too long for a Unix socket.

## [0.1.1] - 2026-07-30

- Provision locked Cargo sources before entering strict offline snapshots, while
  keeping build scripts, proc macros, and compiler actions network-denied.
- Relocate all known-root paths in the compiler environment, including dynamic
  `CARGO_BIN_EXE_*`, config-relative, and build-script-emitted values.
- Fail fast with an actionable diagnostic when an inherited
  `CARGO_TARGET_DIR` would make cargo-reapi manage the target containing its
  own running executable, while preserving intentional external targets.

## [0.1.0] - 2026-07-23

Initial public release.

- Keeps Cargo authoritative by observing the compiler and linker actions Cargo
  schedules through `RUSTC_WRAPPER`.
- Adds verified cross-worktree action caching and exact whole-gate snapshots,
  including concurrent miss coalescing and content-addressed output storage.
- Keys declared inputs, toolchain identity, platform, arguments, working
  directory, relevant environment, native link inputs, and outputs.
- Restores artifacts into independent worktrees with fixed-width path
  relocation, digest verification, and macOS executable re-signing.
- Adds host-wide CPU and memory admission for physical work, cache inspection
  and garbage collection, environment diagnostics, and proof tooling.
- Adds a reclient transport adapter for eligible Remote Execution API actions.
  Validation against a live production REAPI service remains future work.
- Qualifies the local shared-cache path independently on macOS/arm64 APFS and
  Linux/x86_64 XFS with real Cargo, Bevy, and Moria workloads.

Known limitations and the precise qualified boundary are documented in the
[README](README.md#known-limitations-and-roadmap).

[Unreleased]: https://github.com/TamedTornado/cargo-reapi/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/TamedTornado/cargo-reapi/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/TamedTornado/cargo-reapi/releases/tag/v0.1.0
