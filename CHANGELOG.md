# Changelog

All notable changes to `cargo-reapi` are documented in this file.

## [Unreleased]

- A new dependency of a project selected with `--manifest-path` is fetched
  before the strict sandbox starts. Sources were acquired for the workspace in
  the working directory only, so building or fetching a separate workspace,
  such as a vendored tool excluded from the main one, failed offline with "no
  matching package named …". The pre-sandbox `cargo metadata` now takes the
  invocation's `--manifest-path`, `--locked`, `--frozen`, `--offline`,
  `--config` and `-Z` options, so it also never writes a lockfile that a
  `--locked` build would have refused. The gate key's path dependencies come
  from the same project.
- Without `CARGO_TARGET_DIR`, the target directory is the one Cargo reports
  for the selected project, including `build.target-dir`. A `--manifest-path`
  workspace was built into the working directory's `target`, where the
  sandbox did not let Cargo write, or a gate restored it there.
- The default action log lives in that target directory and every compile
  finds it. It was a relative path, which compiles run from a package outside
  the workspace resolved beside that package, failing in the sandbox.

## [0.2.7] - 2026-10-14

Three faults below could serve a wrong cache hit. Earlier entries cannot be
reused: every action key and the gate snapshot schema changed.

- A dependent restored in another worktree names that worktree's copy of each
  workspace dependency. Its artifacts can embed a dependency's package path,
  for example a constant built from `env!("CARGO_MANIFEST_DIR")`, but
  publication relocated only the compile's own package, so the restored
  program named the first worktree's dependency. Every slot for a directory
  beneath the workspace is now relocated. A package outside the workspace,
  such as a registry crate, keeps its absolute path in its artifacts and its
  key.
- A compile receives search lists such as `PATH` as the canonical directories
  it is keyed on. It received the caller's spelling, so two shells with
  different symlinks to one directory shared a key, and a program embedding
  `env!("PATH")` restored the other shell's value.
- Whole-gate snapshots are keyed on the environment names compiles have
  learned, and a gate that learns a name while it runs is not published. A
  snapshot built while a proc macro's read was withheld was restored after
  the crate learned the variable, although a fresh build would pass it.
- Every name Cargo's `[env]` configuration defines reaches the compile. A
  forced entry whose value equaled the caller's, or an unforced entry the
  caller had already set, looked unchanged to the wrapper and was stripped,
  so a proc macro reading it saw it unset.
- Concurrently learned names are all kept. Each is now its own file under
  `learned-environment/` in the cache; compiles that learned different names
  for one crate at once overwrote each other's list.
- A compile that reads one of cargo-reapi's own settings, such as
  `CARGO_REAPI_BACKEND`, sees it unset and fails as it would under plain
  Cargo. It was retried without end, because the wrapper never passes those
  settings.

## [0.2.6] - 2026-10-14

- A compile output has one logical digest, whichever compile reads it and
  whenever. Cargo starts a pipelined dependent as soon as a dependency's
  compiler writes its metadata, before cargo-reapi publishes it. The
  dependent normalized that early `.rmeta` with its own path mappings, which
  lack the dependency's package root, while publication used the
  dependency's mappings. The same metadata was keyed under two digests,
  depending on timing, and a worktree whose dependency was restored missed
  every dependent compiled early elsewhere, along with everything downstream:
  82 of 681 compiles in a fresh Moria worktree. A compile now records the
  mappings its outputs are published with before its compiler starts, and
  every reader normalizes an output with them. These records replace the
  logical-digest receipts and are replaced whole, so a reader never sees a
  partly written one.

## [0.2.5] - 2026-10-14

- Cached compiles run with, and are keyed on, only the environment they can
  read. Every variable outside a short deny list used to enter each action
  key, including `OLDPWD`, `PWD`, agent session IDs and per-shell `PATH`
  entries, so worktrees built from different shells shared almost nothing: a
  fresh Moria worktree from a second shell had 666 misses and no hits. A
  compile now receives only the toolchain variables rustc, Cargo, Clippy and
  native linkers read; what Cargo defined for it (`[env]`, `rustc-env`,
  crate variables); its build script's `rerun-if-env-changed` names; the
  variables rustc recorded as `env-dep` dependencies; and the names in the new
  `CARGO_REAPI_DECLARED_ENV`. Everything else is removed before rustc
  starts, which fails closed: an unkeyed variable cannot change an artifact.
  A proc macro that reads an undeclared variable through `std::env` now sees
  it as unset. Declare it in `CARGO_REAPI_DECLARED_ENV` (for example sqlx's
  `DATABASE_URL`). `CARGO_REAPI_REPORT_STRIPPED_ENV=1` lists the variables
  removed from each compile, and the action log records them as
  `stripped_environment`.
- Search lists such as `PATH` are keyed by the canonical directories they
  resolve through, so per-shell symlinks such as fnm's multishell directory
  no longer change keys.
- The strict sandbox removes srt's randomly numbered
  `CLAUDE_CODE_HOST_HTTP_PROXY_PORT` and `CLAUDE_CODE_HOST_SOCKS_PROXY_PORT`
  before Cargo starts, so build scripts no longer see them either.

## [0.2.4] - 2026-10-13

- A library with several crate types caches every file it links. Only the
  first file `rustc --print file-names` reported was declared as an output,
  so a `crate-type = ["cdylib", "rlib"]` library cached its `.so` alone; a
  cache hit restored no `.rlib` and every crate depending on it failed with
  "reading input …rlib: No such file". The fault predates 0.2.3 and showed
  once such a crate entered a shared cache. Declared outputs are part of the
  action key, so the incomplete entries cached by earlier versions are never
  served again.

## [0.2.3] - 2026-10-13

- The hermetic sandbox no longer lists a readable path that lies beneath
  another readable path. Each package root under the already readable Cargo
  registry became its own read-only bind in the single `sh -c` command srt
  hands bubblewrap; once a workspace reached about 700 packages that
  argument passed Linux's 128 KiB `MAX_ARG_STRLEN` and every build failed
  with `spawn E2BIG`. Reading a directory already permits reading beneath
  it, so the nested entries granted nothing.

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
