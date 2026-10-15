# Changelog

All notable changes to `cargo-reapi` are documented in this file.

## [Unreleased]

## [0.3.0] - 2026-10-15

cargo-reapi is now a per-compile cache, in the model of `sccache`. Cargo runs
every time and plans every build; cargo-reapi caches the individual rustc
compiles it is asked to run.

### What is cached, and what runs fresh

- A library compile (an `rlib`, or only its metadata) is cached. Its key
  covers the compiler's identity, its arguments, its keyed environment, every
  file in the package directory and in `OUT_DIR`, and, for an rlib, every
  file directly in each `-L` native search directory inside the workspace,
  target or package, so a static library an rlib bundles is keyed by content. An entry is a hit
  only while every file in rustc's own dep-info still has the content it had,
  so `include!` and `include_str!` inputs outside the package invalidate it.
- A link step runs every time: binaries, tests, examples, benches, proc
  macros, `cdylib`, `dylib` and `staticlib` crates. The linker reads libraries,
  start files and itself from places the key does not cover.
- A metadata-only compile ignores `-L` and `-l`: it reads no native library,
  so `cargo check` and `cargo clippy` cache crates that link system libraries.
- An rlib compile ignores `-l` kinds it does not bundle (`dylib`, the default
  kind, `static:-bundle`). A bundled `-l static=` library not found in a `-L`
  directory inside the workspace, target, package or the sysroot's own
  library directory (`<sysroot>/lib/rustlib/<target>/lib`; the rest of an
  installation prefix such as `/usr` is not exempt) makes it run uncached, as
  does a thin archive in an inside directory. A `-L` directory outside those (such as the system
  directory pkg-config `-sys` crates pass to every dependent) is allowed, but
  the result is stored only if the rlib holds nothing but `lib.rmeta` and the
  crate's own objects, since `#[link(kind = "static")]` bundles without any
  `-l`. The action log records an uncached compile as `local-ineligible`.
- `CARGO_MAKEFLAGS`, `CARGO_BUILD_JOBS` and `CARGO_REAPI_*` are passed to the
  compile but not keyed, so a different job count no longer splits the cache.
  A compile that reads one through `env!` is not stored.
- Only dep-info, metadata and link outputs and a fixed list of codegen
  options are cached. Other `--emit` kinds, unpacked split debuginfo,
  `-C save-temps`, profile-guided options, `-C target-cpu=native`, unlisted
  codegen options, response-file arguments and unstable `-Z` options run
  uncached.
- Both `--extern NAME=PATH` and `--extern=NAME=PATH` artifacts are keyed; an
  extern artifact that cannot be read makes the compile run uncached.
- A compile whose workspace, target, package or toolchain path contains a
  character rustc escapes in dep-info (space, backslash, newline, carriage
  return) runs uncached.
- `-L` native search directories are resolved as rustc opens them, without
  collapsing `..` before following links.
- The action-key namespace and manifest version are raised, so entries
  stored before these rules are misses.
- A compile whose keyed package files changed while it ran, or whose keyed or
  dep-info files were modified at or after it started, is not stored
  (`local-input-changed`).
- Worktree roots are relocated and keyed only as whole path components
  followed by `/` or the end of the value, so a sibling such as
  `<package>-assets` or `<package> assets` keeps its path.
- Target discovery honours `--config` arguments, and the package walk skips
  directories Cargo marks with `CACHEDIR.TAG`, such as a `build.build-dir`
  inside the package.
- An environment variable that is not UTF-8 no longer stops the wrapper, and
  keyed values that are not UTF-8 are keyed byte for byte.
- `cache gc` reads only committed `actions/<key>.json` manifests and removes
  temporary manifests interrupted publishers left behind.
- A build whose configuration already names a compiler wrapper
  (`RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, their `CARGO_BUILD_*` forms,
  `build.rustc-wrapper` or `build.rustc-workspace-wrapper` in a configuration
  file or a `--config` argument) runs as plain Cargo, with one line on stderr.
  A `--config` wrapper used to be replaced silently.

### Assumptions

Build scripts and proc macros are trusted not to be written to defeat the
cache, and proc macros declare the files they read: rustc's dep-info lists
the files a compile read through `include!`, `include_str!` and
`include_bytes!`, not files a proc macro opens itself. Variables a proc
macro reads through `std::env` reach it only when they are keyed (see the
README).

### Removed

- The whole-gate snapshot, its restore, marker and key, and
  `gate-inputs.jsonl`; the strict `srt` sandbox, `--snapshot-policy`,
  `--declared-input` and `CARGO_REAPI_SRT`; the `cargo metadata --locked`
  preflight and the selection, `run` and target-disjointness rules.
- Link caching and native link-input discovery, the linker capture
  (`CARGO_REAPI_LINKER_CAPTURE`, `CARGO_REAPI_REAL_LINKER`,
  `CARGO_REAPI_DEFAULT_LINKER`) and Mach-O re-signing of restored outputs.
- The rustc query shim and its cache; `CARGO_REAPI_ACTION_CACHE_TEST_MODE`,
  `CARGO_REAPI_GATE_INPUTS` and `CARGO_REAPI_CLONE_TRACE`; the doctor's
  sandbox and clone fields.
- The acceptance harness: `acceptance/`, the `contract` and `prove`
  commands, the `cargo-reapi-auditor` and `cargo-reapi-exec-auditor`
  binaries and the rustc observer (`CARGO_REAPI_REAL_RUSTC`,
  `CARGO_REAPI_RUSTC_TRACE`). Its warm-run criterion, zero physical compiler
  actions, no longer holds now that link steps always run.

Passing a removed option is a clap error, and the removed variables are no
longer read. `CARGO_REAPI_TARGET_ROOT` is set by the driver; a value the
caller sets is overwritten, as before. `cache gc` removes the directories
earlier versions kept (`gate-snapshots`, `access/gates`,
`rustc-query-cache-v1`, `discoveries`, `link-captures`, `file-digests`) and
reports them as `removed_obsolete_directories`; its report keeps
`schema_version` 2.

### Kept

An action entry written by another cargo-reapi version is a miss, never an
error. Every write cargo-reapi makes where a build can write follows one
policy (no link followed, in-place change only as a locked append to a
single-link regular file, otherwise replace by rename). Concurrent identical
compiles run once; misses lease CPU and memory from the shared ledger.

The unreleased whole-gate work that followed 0.2.10 (declines, target rules,
snapshot verification, sandbox hardening and their regressions) is superseded
by this narrowing and was never released.

## [0.2.10] - 2026-10-14

Three faults below could serve a stale gate hit or lose source.

- A gated build refuses a target directory that is or contains a source:
  the working directory, the selected workspace, a package or a declared
  input. With `build.target-dir = "."`, which 0.2.8 began honoring, the
  source tree was the snapshotted target and was excluded from the gate key,
  so a gate hit restored the original source over an edit.
- Every selected package is hashed into the gate key. One beneath a directory
  named `target`, such as `--manifest-path target/tool/Cargo.toml`, was
  skipped by the workspace scan and, lying beneath the working directory, by
  the path-dependency scan too, so an edit to it got a stale gate hit.
- Aliases are expanded as Cargo expands them before the invocation's
  selection is read. `[alias] ci = "build --locked"` hid `--locked` from the
  metadata run, which wrote a lockfile that plain `cargo ci` refuses to
  create. A command whose alias cannot be read is refused.
- Feature selections (`--features`, `-F`, `--all-features`) resolve every
  feature in the metadata run, so an optional path dependency they activate
  is acquired and readable in the sandbox. It was missing, and the build
  failed to read its manifest.
- A selected workspace beside the working directory
  (`--manifest-path ../other/Cargo.toml`) stays readable. The sandbox dropped
  it because it contains its target and rebuilt the readable inputs from the
  working directory alone, so its manifest did not exist inside.
- `-m`, Cargo's short form of `--manifest-path`, is read separated, attached
  and in clusters (`-m path`, `-mpath`, `-qm path`), as is `-F`, and
  `--target-dir` chooses the target directory. Cargo's `-C` is refused.

## [0.2.9] - 2026-10-14

- Reinstalling cargo-reapi no longer breaks builds already running. `cargo
  install` renames a new file over the old one; a strict-sandbox build keeps
  running the file it bound when it started, whose `/proc/self/exe` then reads
  `… (deleted)`. Every compile that linked afterwards passed rustc that
  unrunnable path as its linker wrapper and failed with "linker
  `…/cargo-reapi (deleted)` not found". Compiles now give rustc
  `/proc/<pid>/exe`, which runs the very executable that is waiting for the
  link. The query shim Cargo runs for metadata is named the same way, and the
  gate key hashes the running executable's bytes rather than whatever is
  installed by then.
- A driver whose executable is reinstalled while it waits to start Cargo, for
  cache admission or another gate's producer, hands the command to the newly
  installed version. Cargo would otherwise run the new version as the
  compiler wrapper of the old driver, or, in the strict sandbox, fail to run
  any compiler wrapper at all.

## [0.2.8] - 2026-10-14

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

[Unreleased]: https://github.com/TamedTornado/cargo-reapi/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/TamedTornado/cargo-reapi/compare/v0.2.10...v0.3.0
[0.1.1]: https://github.com/TamedTornado/cargo-reapi/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/TamedTornado/cargo-reapi/releases/tag/v0.1.0
