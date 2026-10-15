# Changelog

All notable changes to `cargo-reapi` are documented in this file.

## [Unreleased]

cargo-reapi now caches only what it can prove safe; everything else runs as
plain Cargo. Several faults below could serve a stale gate hit or overwrite
source.

- A decline is never an error. When an invocation cannot be proven safe to
  cache, or preparing it fails for any reason, cargo-reapi runs the exact Cargo
  command uncached, restores and records nothing, and prints one line,
  `cargo-reapi: not caching (<reason>); running Cargo uncached`. A resolved
  project keeps the strict sandbox for the uncached run. Refusals used to be
  errors, and applied to `--backend local` and `capture` as well, so a valid
  alias failed there; the checks now apply only where caching would happen.
- Only `build`, `check`, `clippy`, `test`, `run`, `bench` and `doc` are cached.
  Other commands, `--help` and `--version` run as plain Cargo before any
  metadata run; they used to resolve and fetch first, could create the
  lockfile, and failed outside a project.
- Preparation never changes the project: `cargo metadata` runs with
  `--locked`. A missing or outdated lockfile, or a metadata failure such as an
  offline library build whose dev-dependency sources are not cached, declines
  instead of writing the lockfile or failing. An alternate lockfile
  (`CARGO_RESOLVER_LOCKFILE_PATH`, `resolver.lockfile-path`) and
  `--ignore-rust-version` decline; the gate did not track the former, and
  preparation silently dropped the latter, choosing dependencies Cargo would
  not have.
- The target directory must be disjoint from every project directory in both
  directions: the working directory, selected workspace, every package root,
  source directory and declared input. Only Cargo's own default,
  `<workspace root>/target`, is exempt. `build.target-dir = "data"` over an
  `include_str!` data directory used to pass and let a hit restore over edits.
- Hashing has no exclusions by name, except `.git`. A source directory named
  `target` (`include_str!("target/message.txt")`) was skipped by the gate key
  and by rustc input discovery, so an edit returned the old binary. Only the
  canonical target directory is left out. The strict sandbox now denies reads
  of every `.git` (directory or file) and the gitdir it names, in a main clone
  as in a linked worktree, and `.git` is not hashed: no partial hash of a
  repository is sound (reflogs and abbreviated object names can change a build
  script's answer), so a build script gets no Git state under caching.
- A gate writes into a target only when it is absent or clean (nothing but
  cargo-reapi's directory and Cargo's `.rustc_info.json` and `CACHEDIR.TAG`),
  and a producer publishes only from one. A restore had overwritten files
  already there: an `include_str!` input inside Cargo's own default target, and
  other builds' artifacts that a snapshot of the whole target carried. Any
  other target state skips the gate and runs Cargo with the per-action cache.
- A marker alone never proves a hit. It records the size, times and inode of
  every file the gate left, and a hit needs exactly that set of files to be
  there with those identities; a plain `cargo build --features extra` into the
  same target used to leave its executable in place of the cached one.
- Snapshots record a content digest of every file when published and verify
  them before restoring; a mismatch evicts the snapshot.
- Every cached compile is a hit only while every file in rustc's own dep-info
  still matches its recorded digest. A file included from outside the package
  directory (`include!("../../shared/body.rs")`) did not key the compile, so
  editing it returned the old successful result.
- Options after the subcommand that cargo-reapi does not know decline,
  including the older `--lockfile-path`, as do `[unstable]` configuration and
  `CARGO_UNSTABLE_*`; configured compilers and wrappers (`build.rustc`,
  `build.rustdoc`, `build.rustc-wrapper`, `build.rustc-workspace-wrapper` and
  their environment forms) and `build.build-dir`/`CARGO_BUILD_BUILD_DIR`
  decline instead of being overridden or left out of snapshots. A custom
  `RUSTDOC` is hashed by content.
- `run` caches only its build, as the `build` it contains; the program then
  runs normally with its own stdin and stdout every time. A whole-gate hit
  used to skip the program.
- The `cargo-reapi` directory in the target must be real, with no symlink
  inside; a link there sent the driver's marker and log writes into source.
- A strict sandbox that cannot start is an error that says Cargo was not run,
  not a "running Cargo uncached" decline that then failed.
- An action entry written by another cargo-reapi, such as a schema-1 entry
  after upgrading, is a cache miss; reading its missing `dep_inputs` field
  aborted the build. Action keys are namespaced by the cache layout's version.
- `run` gates the `build --bin <name>` for the binary `run` would build
  (`--bin`, `--example`, `-p`, `default-run`, the only binary), not every
  binary of the package, and declines when that cannot be told; a gate
  preparation failure never runs the program inside the sandbox as well as
  outside it. `--open` declines.
- Every driver write in the target after a build (the marker, the hit record)
  opens beneath a handle with `O_NOFOLLOW`, so a link a build script planted
  cannot redirect it into source. Snapshot verification covers every entry a
  restore copies. Fingerprint invalidation removes only Cargo's own
  `.fingerprint` directories. cargo-reapi's own directory in the target counts
  as content for cleanliness, and a marker hit needs it to hold only what
  cargo-reapi writes.
- Dep-info inputs are resolved as the kernel does (a link then `..`) and
  parsed with rustc's escaping, spaces only; a changed file behind either was
  not noticed.
- A Git dependency's checkout repository state is part of the gate key, since
  Cargo needs its `.git` readable. The audit observer's variables no longer
  reach the compiler.
- Directories are keyed by name as well as files, empty ones included, in tree
  hashing and in declared and observed directory inputs: a build script that
  lists a directory saw a new empty subdirectory only after a rebuild nothing
  forced.
- A file under `cargo-reapi/` that rustc's dependency files list as read (a
  generator's file in the build's temporary directory) is checked by content on
  every marker hit; a snapshot with such files is never restored elsewhere.
- Git dependency repositories hash `objects/info` and pack index, `.keep` and
  `.rev` files by content, and a checkout with alternates declines.
- `run` resolves its package as Cargo does (`-p`, the workspace's
  `default-members` at its root, else the member) and declines glob, unknown,
  repeated and non-executable `--bin`/`--example` selections before building.
- The audit variables are removed from every compiler that does not carry the
  observer shim's marker; setting `CARGO_REAPI_REAL_RUSTC` no longer opens
  them to an ordinary compiler.
- The sandbox makes the cache's `gate-snapshots` and `access/gates` read-only
  to builds, and the action log is only appended to as a single-link regular
  file; a build could plant a link there that the driver wrote through.
- Test hooks are compiled only with the `test-hooks` feature, documented as
  for the crate's own tests: the coalescing signal and
  `CARGO_REAPI_ACTION_CACHE_TEST_MODE`. The integration tests that need them
  require it; run the suite with `cargo test --features test-hooks`.
- Selected packages are hashed by their real directories: a package selected
  through a symlink counted as hashed by the working directory's scan, so an
  edit got a stale hit. The root of a selected virtual workspace is hashed too
  and is readable in the sandbox.
- User-defined aliases, `--config` beyond `profile.*`, `term.*` and
  `build.jobs`, `-Z`, `-C`, unknown global options and short-option clusters
  holding a value option decline instead of being emulated.
- `-F=` is an explicitly empty feature list; a lockfile or network flag given
  twice reaches `cargo metadata` once; features without a package selection
  reach `cargo metadata` as given, and with `-p`, `--workspace` or `--exclude`
  offline they decline; a target directory whose parent does not exist yet
  works; an empty `CARGO_HOME` falls back to `$HOME/.cargo`.
- Tests: the collection test detects a lock cycle in the kernel's lock table
  instead of a 60 second deadline, and every regression drives real Cargo.

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

[Unreleased]: https://github.com/TamedTornado/cargo-reapi/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/TamedTornado/cargo-reapi/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/TamedTornado/cargo-reapi/releases/tag/v0.1.0
