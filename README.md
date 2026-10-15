# cargo-reapi

`cargo-reapi` is a shared compile cache for ordinary Cargo projects, in the
model of `sccache`. It was built for massively parallel Rust agent pipelines,
where many clean worktrees of one project would otherwise repeat the same
compiles.

`cargo-reapi` is an independent project and is not affiliated with or endorsed
by the Rust Project or Rust Foundation.

Cargo remains the build planner. `cargo-reapi` runs Cargo with itself as the
compiler wrapper (`RUSTC_WRAPPER`), so Cargo runs every time, decides what to
build and runs build scripts as usual. Each compile Cargo asks the wrapper for
is either restored from a shared local cache, after its inputs are verified,
or run and published. Identical compiles in different worktrees share one
entry; identical compiles running at the same moment run once.

## What is cached, and what runs fresh

**Cached: library compiles.** A compile that produces an `rlib`, or only its
metadata, is looked up in the cache and restored on a hit.

**Always run: link steps.** Binaries, tests, examples, benches, proc macros,
and `cdylib`, `dylib` and `staticlib` crates are linked every time. A linker
reads libraries, start files and itself from places the key does not cover,
so a cached link could be stale. The action log records these as
`local-ineligible`, with the reason.

**Run uncached: rlibs that bundle a static library from outside the
project.** A metadata-only compile (as in `cargo check` and `cargo clippy`)
reads no native library, so its `-L` and `-l` arguments do not matter. An
rlib compile reads only the static libraries it bundles: `-l static=` (without
`-bundle`) and `#[link(kind = "static")]` in its source. Other `-l` kinds are
read only by a later link step. An rlib compile runs uncached when a bundled
`-l static=` library is not found in a `-L` directory inside the workspace,
the target directory, the package or the sysroot's own library directory for
the target (`<sysroot>/lib/rustlib/<target>/lib`). The rest of the
compiler's installation prefix, such as `/usr` for a distribution's
`/usr/bin/rustc`, is an ordinary outside directory. A thin archive in an
inside directory names member files elsewhere, so it also makes the compile
run uncached. A `-L` directory outside those, such as the system library directory a pkg-config
`-sys` crate passes to all its dependents, is allowed, but the result is
stored only if the rlib rustc wrote holds nothing besides `lib.rmeta` and the
crate's own object files; otherwise it ran uncached. This check is needed
because a `#[link(kind = "static")]` bundles an archive that never appears as
`-l`. The key covers the arguments and the sources, so every hit bundles the
same nothing.

**Run uncached: outputs and options the cache does not reproduce.** A
compile is cached only when it emits dep-info, metadata and link outputs and
uses codegen options from a fixed list whose effect the key covers. Another
`--emit` kind, `-C save-temps`, `-C split-debuginfo=unpacked` (anything but
`off` or `packed`), the profile-guided options, `-C target-cpu=native` (the
CPU it resolves to is not keyed) and any codegen option not on the list run
uncached. So do arguments read from a response file (`@file`), which are not
analysed.

**Plain Cargo: a wrapper you already chose.** When `RUSTC_WRAPPER`,
`RUSTC_WORKSPACE_WRAPPER`, their `CARGO_BUILD_*` forms, or
`build.rustc-wrapper` or `build.rustc-workspace-wrapper` in a Cargo
configuration file or a `--config` argument already names a compiler wrapper,
cargo-reapi would replace it, so it runs your Cargo command unchanged and
prints `cargo-reapi: not caching (<reason>); running Cargo uncached`.

## What a cached compile is keyed on

A compile's key covers:

- the compiler binary's digest and version, the host platform, and the
  compiler's arguments with the worktree's paths normalized, so the same
  project in two worktrees shares entries;
- its keyed environment (below);
- every file in the package directory and in `OUT_DIR`, by content, except
  the target directory, any directory Cargo marked as its output with
  `CACHEDIR.TAG` (such as a `build.build-dir` inside the package) and any
  entry named `.git`; a directory named `target` inside the sources is
  ordinary source;
- for an rlib compile, every file directly in each `-L` native search
  directory inside the workspace, target or package, so a static library an
  rlib bundles from another package's output, or from a directory of the
  workspace, is keyed by content;
- every `--extern` artifact by content.

An entry is a hit only while every file rustc's own dep-info listed when the
entry was made still resolves to the same file with the same content. Paths
are resolved as the kernel does (a link followed by `..` included), and
rustc's escaping (spaces only) is read exactly. So a file that `include!`,
`include_str!`, `include_bytes!` or `#[path]` reads from outside the package
invalidates the entry when it changes. A dep-info that cannot be read, or that
lists a file outside the package, workspace, target and toolchain, is not
published.

A compile's files are hashed around it, not at the instant rustc reads them,
so an edit while it runs could pair its outputs with content it never read.
The entry is therefore not published (`local-input-changed`) when a package
or `OUT_DIR` file the key covers has different content after the compile
than before it, or when any keyed or dep-info file was modified at or after
the moment the compile started.

An entry written by another version of cargo-reapi is a miss, never an error.
Restored artifacts have the worktree's paths written back into them, so a
binary linked from restored libraries names its own worktree.

## Assumptions

The cache gives the artifacts plain Cargo would, given two assumptions:

- **Build scripts and proc macros are not written to defeat the cache.** A
  build script that rewrites another package's sources while the build runs,
  or a proc macro that deliberately reads inputs hidden from rustc, is out of
  scope.
- **Proc macros declare the files they read.** rustc's dep-info lists the
  files a compile reads through `include!`, `include_str!` and
  `include_bytes!`, but not files a proc macro opens itself. A macro that reads
  a file should also expand to an `include_bytes!` of it, or otherwise make
  rustc track it; an edit to an untracked file alone is not noticed.

Build scripts always run under Cargo, so whatever they read is keyed through
what they produce: `OUT_DIR` contents, compiler arguments and `rustc-env`
values.

## Usage

```sh
cargo install cargo-reapi --locked
cargo reapi --backend cache --cache-dir /shared/cargo-reapi-cache -- check
cargo reapi --backend cache --cache-dir /shared/cargo-reapi-cache -- test
cargo reapi doctor --cache-dir /shared/cargo-reapi-cache --json
cargo reapi cache stats --cache-dir /shared/cargo-reapi-cache --json
cargo reapi cache gc --cache-dir /shared/cargo-reapi-cache \
  --max-bytes 96636764160 --min-free-bytes 32212254720 \
  --target-free-bytes 48318382080 --json
```

Everything after `--` is passed to Cargo unchanged. Long-lived runtimes may
set `CARGO_REAPI_BACKEND`, `CARGO_REAPI_CACHE_DIR` and the other options in
the environment once, then run `cargo-reapi check`, `cargo-reapi clippy ...`
or `cargo-reapi test`; explicit flags still take precedence. Cache mode
requires an explicit cache directory, so separate worktrees share only the
store the operator chose.

Backends:

- `cache` restores and publishes compiles as described above.
- `capture` runs every compile and records, in the action log, the key a
  cached compile would have.
- `local` runs every compile.
- `reapi` sends eligible compiles to a Remote Execution API service through
  reclient (below).

The action log is JSON Lines, by default `cargo-reapi/actions.jsonl` in the
target directory; a relative `--action-log` is relative to the directory
cargo-reapi starts in. Each compile's `execution` is one of `cache-hit`,
`coalesced-hit` (it waited for an identical compile and restored its
result), `local-cache-miss` (run and published), `local-ineligible` (run
uncached; `cache_eligibility.reasons` says why), `local-environment-withheld`
(see below), `local-input-changed` (run, but an input changed while it ran,
so nothing was stored), `local-output-incomplete` or `local-failed`. Cargo's control
probes, such as `rustc -vV`, are logged too.

To check that cargo-reapi changes nothing, compare the exit status and
artifacts with the same Cargo command run without it.

### Cache maintenance

`cache gc` takes an exclusive maintenance lease, waits for active restores and
producers, evicts least-recently-used compile entries, and removes blobs that
no surviving entry references. A dry run reports the same selection without
changing anything. It also removes, whole, the directories earlier versions
kept (`gate-snapshots`, `access/gates`, `rustc-query-cache-v1`, `discoveries`,
`link-captures`, `file-digests`), and reports how many as
`removed_obsolete_directories`. Only `actions/<key>.json` files are entries;
any other file there is a temporary manifest a publisher was interrupted
writing, removed under the lease and counted as `removed_temporary_files`.

`--max-bytes` bounds the cache's physical footprint, reported as
`total_bytes`. On Linux it comes from each file's extent map, so blocks shared
by reflinked copies count once, and evicting an entry is credited only with
the blocks no surviving entry still references. Files without an exact extent
map, and every file on other platforms, count their allocated size, which can
over-count clones but never under-counts. `apparent_bytes` reports the sum of
file lengths for comparison.

A waiting collector closes cache admission before draining existing readers.
Active readers continue; new readers wait until collection finishes. The
admission fence is an OS file lock, so process exit releases it. Upgrade every
reader and collector sharing a cache together.

### Resource ledger

Compiles that miss the cache lease CPU and memory from one host-wide ledger,
so many concurrent Cargo runs do not oversubscribe the machine; cache hits
never take a lease. `CARGO_REAPI_RESOURCE_CPU_CAPACITY` and
`CARGO_REAPI_RESOURCE_MEMORY_GIB_CAPACITY` set its capacity; without them it
uses the host's logical CPUs and physical memory, and values above those are
rejected. `CARGO_REAPI_RESOURCE_LEDGER` selects the ledger directory (by
default `resource-ledger-v1` in the cache directory); use the same path and
capacities for every worker on one host, even with separate caches. A compile
that waits 300 seconds for a lease while no other lease is released fails as
an infrastructure stall.

### Compile environment

In cache mode, each compile runs with only the environment variables its key
covers. Every other variable is removed before rustc starts, so an unkeyed
variable cannot change an artifact, and unrelated shell state (`OLDPWD`,
`PWD`, terminal and agent session variables) does not split the cache between
shells. A compile receives:

- the variables rustc, Cargo, Clippy, the dynamic loader, the C toolchain and
  native linkers read: every `CARGO*`, `__CARGO*`, `RUST*`, `CLIPPY_*`, `LD_*`,
  `DYLD_*`, `GCC_*`, `COLLECT_*`, `CCC_*`, `CLANG_*`, `LC_*` and `MOLD_*`
  variable, `*_DEPLOYMENT_TARGET`, and `PATH`, `HOME`, `TMPDIR`, `TMP`, `TEMP`,
  `LANG`, `LANGUAGE`, `LIBRARY_PATH`, `COMPILER_PATH`, `SOURCE_DATE_EPOCH`, `CC`,
  `CXX`, `AR`, `LD`, `CFLAGS`, `CXXFLAGS`, `CPPFLAGS`, `LDFLAGS`, `SDKROOT`,
  `DEVELOPER_DIR`, `ZERO_AR_DATE`, `MAKEFLAGS`, `MFLAGS`, `OUT_DIR`, `TARGET`,
  `HOST`, `PROFILE`, `DEBUG`, `OPT_LEVEL` and `NUM_JOBS`;
- everything Cargo defined for the compile beyond the caller's environment,
  such as build-script `rustc-env` values;
- every name Cargo's `[env]` configuration defines, read from the
  `.cargo/config.toml` (or `.cargo/config`) files Cargo loads and any
  `--config` arguments, even when the value equals the caller's. A
  configuration file with an `include` makes compiles run with the whole
  environment instead;
- the build script's `cargo::rerun-if-env-changed` variables;
- variables rustc recorded as `# env-dep:` dependencies (`env!`, `option_env!`
  and tracked proc-macro reads). The first compile that tracks a set variable
  it was not given is repeated with it, and the cache remembers the name for
  that crate, so later compiles of the crate are keyed on it and an entry made
  before is never restored. If such a compile had already succeeded, it fails
  once with an explanation instead, because Cargo may already have used its
  artifacts; the next build passes the variable. A compile that reads one of
  cargo-reapi's own settings sees it unset, as under plain Cargo;
- every name in `CARGO_REAPI_DECLARED_ENV`, a comma-separated list.

cargo-reapi's own settings (action log, cache directory, backend, worktree
roots and resource capacity) are read by the wrapper only; they never reach a
compile and are not keyed.

`CARGO_MAKEFLAGS` (Cargo's jobserver, which rustc joins), `CARGO_BUILD_JOBS`
and any other `CARGO_REAPI_*` variable reach the compile but are not keyed:
they schedule the build and cannot change what rustc writes. A compile whose
source reads one of them through `env!` or `option_env!` runs but is not
stored.

Search lists (`PATH`, `LD_LIBRARY_PATH`, `LIBRARY_PATH`, `COMPILER_PATH` and
the `DYLD_*` library paths) reach the compile as the canonical directories
they resolve through, and are keyed by that value, so a per-shell symlink to
the same directory does not change the key.

A proc macro that reads a variable through `std::env` is invisible to rustc's
dependency tracking, so it sees that variable as unset unless the variable is
in one of the groups above. Such a macro usually fails with a message naming
the variable: sqlx's `DATABASE_URL` is a typical case. Declare it:

```sh
CARGO_REAPI_DECLARED_ENV=DATABASE_URL,SQLX_OFFLINE cargo reapi --backend cache -- build
```

Set `CARGO_REAPI_REPORT_STRIPPED_ENV=1` to have every cached compile list the
variables removed from it on stderr. The action log records the same list as
`stripped_environment`, beside the `keyed_environment` digests.

If cargo-reapi cannot determine a compile's environment (the driver did not
start Cargo, the compile emits no dep-info, or the build script's output is
unreadable), that compile runs with the whole environment and keys all of it.
Link steps and other uncached compiles run with the whole environment. Capture
mode runs compiles with the caller's whole environment and records the key a
cached compile would start with.

### Writes where a build can write

Build scripts and proc macros can write in the target and wherever else the
user can, so any path there may become a link while a build runs. Every write
cargo-reapi makes to such storage (the action log, path-mapping records,
restored outputs, locks) follows one policy: no link is followed; a file is
changed in place only by a locked append to a regular file with a single
link; anything else is replaced by a new file renamed over its name; lock
files are opened without following links and only locked.

### Remote execution

The reclient adapter stages eligible compiles into explicit input roots,
invokes the production `rewrapper` client with declared inputs and outputs,
and materializes successful outputs into Cargo's target directory:

```sh
cargo reapi --backend reapi \
  --rewrapper /opt/reclient/rewrapper \
  --rewrapper-cfg /etc/cargo-reapi/rewrapper.cfg \
  --reclient-staging-dir /shared/cargo-reapi-stage \
  --reclient-platform 'OSFamily={os},Arch={arch},toolchain_sha256={toolchain_sha256}' \
  -- check
```

Eligibility fails closed: link steps never go remote, and the platform
template must bind `{os}`, `{arch}` and `{toolchain_sha256}` so a compile
cannot run against a mismatched worker toolchain. It requires an
operator-provided reclient installation, a running `reproxy` and a
platform-matched REAPI service. The test suite exercises the adapter against a
behaviorally faithful fake `rewrapper`; it has not been validated against a
live service.

### Containers

For Linux container consumers, build the repository's image rather than
bind-mounting a host binary across an OS boundary:

```sh
docker build -t cargo-reapi:local .
```

Its executable is at `/usr/local/bin/cargo-reapi`.

## History

Versions up to 0.2.10 also restored a whole quality gate's target state in one
step and ran Cargo in a sandbox to make that sound. That design, its
acceptance harness and the measurements in [`benchmarks/`](benchmarks/README.md)
and [`docs/`](docs/architecture.md) are historical: they describe the earlier
design, not this one.

## Design constraints

- Cargo owns dependency resolution, features, build scripts, proc macros,
  profiles and command ordering.
- The wrapper may distribute compiles; it must not regenerate Cargo's graph.
- Keys include every input a compile reads, the toolchain identity, the
  compiler arguments and the relevant environment. What cannot be keyed runs
  uncached.
- Remote execution must match the host and target platform contract.

## Project policy

This is infrastructure built first for the Moria/Bro workload and shared in
public as-is. Issues with reproducible action captures are welcome, but
publication does not promise compatibility with every Cargo project, hosted
workers, or support response times. Correctness takes priority over coverage.
