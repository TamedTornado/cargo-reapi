# Sharing Rust build work across Cargo worktrees with cargo-reapi

Large Rust builds are expensive once. They become much more expensive when CI
jobs, stacked branches, or coding agents compile the same dependency graph in
several clean worktrees at the same time.

We built [`cargo-reapi`](https://github.com/TamedTornado/cargo-reapi) to reuse
that work without replacing Cargo's build graph. Cargo still resolves
dependencies, selects features, runs build scripts and proc macros, and decides
which compiler commands should execute. `cargo-reapi` observes those commands,
captures their complete inputs, and restores verified outputs from a shared
cache when it can prove that doing so is safe.

The project grew out of a concrete workload: five independent coding agents
working on [Moria](https://github.com/TamedTornado/moria), a Rust and Bevy
voxel-world substrate. Each agent has its own Git worktree and runs ordinary
quality gates:

```text
cargo fmt
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

Five worktrees should not need five physical compilations of Bevy. Making that
statement true, however, required more than putting `target/` on shared
storage.

## Why not share Cargo's target directory?

Cargo's target directory is mutable state. Sharing it directly between
independent Cargo processes creates lock contention and makes cleanup,
isolation, and failure recovery difficult. Giving each worktree its own target
directory avoids those problems, but duplicates artifacts and compiler work.

Build systems such as Bazel and Buck2 can model Rust builds for remote
execution, but they introduce another build graph that must remain consistent
with Cargo. That is a difficult boundary for arbitrary Cargo projects,
especially around workspace feature selection, Cargo-provided environment
variables, build scripts, proc macros, and native dependencies.

We wanted Cargo to remain the planner. `cargo-reapi` therefore uses
`RUSTC_WRAPPER` to observe the exact compiler commands Cargo schedules. It does
not independently reconstruct the crate graph.

This preserves a useful compatibility property: if an action cannot be
represented completely, it runs locally or fails explicitly according to the
selected mode. It is never made remotely eligible through a guess.

## Two layers of reuse

There are two opportunities to avoid repeated work.

The first is an exact whole-gate snapshot. If the workspace state, Cargo
configuration, toolchain, platform, arguments, relevant environment, declared
external inputs, and requested gate all match, `cargo-reapi` restores the
complete target state and skips Cargo.

The second operates inside a Cargo run. When the whole gate differs, Cargo
plans normally and invokes the wrapper. Each compiler or linker action gets a
cross-worktree-stable key derived from:

- normalized arguments and working directory;
- source, dependency, and explicitly declared input content;
- compiler and toolchain identity;
- relevant environment;
- platform contract; and
- declared outputs.

An unchanged action can then restore its outputs into the current worktree.
When several processes miss on the same key concurrently, a cross-process lock
elects one producer while the other callers wait and consume the published
result.

That second path matters in real development. Editing one leaf crate should
not require either a perfect whole-gate hit or a complete rebuild. Our
adversarial acceptance test mutates a leaf and independently observes that
exactly the leaf and its transitive dependants execute; unrelated work is
restored.

## Relocation is part of correctness

Rust compiler artifacts are not automatically independent of the worktree that
created them. Arguments, dep-info files, diagnostics, and linked outputs can
carry paths. A cache key that merely hashes source files is not enough, and a
cache hit is not correct merely because `rustc` did not run.

`cargo-reapi` normalizes worktree paths for keying and uses a fixed-width
relocation scheme when materializing outputs into another worktree. It hashes
published blobs and verifies them again before restoration. On macOS it also
re-signs relocated binaries.

The acceptance suite tests a relocated Bevy application and test binary
against a fresh control build. It also poisons cached data deliberately to
ensure corrupt entries are rejected instead of served.

Native linking widens the input problem further. Response files, linker
binaries, native libraries, generated linker arguments, and platform SDKs can
all affect the result. The local cache discovers and keys those inputs for its
qualified cases. Remote link actions remain ineligible until the complete
native input set can be represented for a worker. Metadata-only compiler
actions have a smaller, currently supported remote-eligibility boundary.

## Environment variables are inputs, too

One of the most useful dogfood failures came from the orchestration layer.
Host-level scheduling and storage variables leaked into Cargo's environment.
Build scripts and proc macros could observe them, so `cargo-reapi` correctly
treated worktrees with different values as different actions.

The tempting repair would have been to ignore more environment variables.
That would improve the hit rate while creating a route to stale outputs.
Instead, the harness now removes its private orchestration namespaces before
invoking Cargo. Project-visible environment remains part of the action key.

Another failure came from inconsistent container paths: the Cargo target
directory was mounted at one path, while the declared target root and action
log still referred to the host path. Those actions became ineligible rather
than entering the cache under an ambiguous identity.

A third failure appeared when a Bevy dependency's build script invoked `c++`.
On Debian, that executable resolved through `/etc/alternatives`, which the
strict snapshot sandbox had hidden. The build failed loudly. The fix was to
model the read-only path and add an integration fixture whose real `build.rs`
compiles C++, archives an object, links it, and executes the result.

All three bugs reduced availability or reuse. None produced a false hit. That
is the failure direction we want.

## What the measurements look like

On a Linux/XFS qualification host, one cold Moria gate took 3,125.608 seconds
and reached 6.37 GB peak process-tree RSS with 144 MB of swap growth. After
seeding the cache, five simultaneous consumers with empty target directories
completed their full gates in 24.695 seconds. External OS observation recorded
zero compiler or linker executions during the warm population, peak RSS was
879 MB, and swap did not grow.

During a separate cold production population, five Cargo processes reached the
same `bevy_pbr` action. They computed the same key, and the retained records
showed one physical producer, four coalesced waiters, and five successful
callers.

We do not treat a fast wall clock as proof by itself. The qualification suite
combines action logs with OS-level compiler and linker observation, exact
mutation attribution, cache-poison tests, relocated-binary comparison,
concurrent failure propagation, resource accounting, sandbox checks, and
recursive evidence hashing.

Current-schema platform batches independently passed on macOS/arm64 with APFS
and Linux/x86_64 with XFS. They are independent platform qualifications, not a
combined cross-platform aggregate: the raw macOS evidence tree had already
been intentionally discarded before the later aggregate verification.

There is also a small but important gap in one production observation. A
complete sample classified 68 wrapper records; a later line count saw 74 after
the log had grown, but the six appended outcomes were not reclassified before
the raw operational log was disposed. The project reports those six records as
unreconciled rather than guessing their outcomes. The public acceptance
results do not depend on that production sample.

## Remote execution is a stricter milestone

The name reflects the intended protocol boundary: `cargo-reapi` includes an
adapter for the standard Remote Execution API through reclient's `rewrapper`.
Eligible actions are staged into explicit input roots with declared outputs
and a platform template binding the operating system, architecture, and
toolchain digest.

The adapter is tested against a behaviorally faithful fake `rewrapper`, but it
has not yet been validated against a live production REAPI service. Today, the
qualified public result is the shared local cache. Calling that distinction out
matters because content addressing and a transport adapter are not substitutes
for proving toolchain and platform compatibility on real workers.

## What is ready, and what is not

`cargo-reapi` 0.1 is an early public implementation for Linux and macOS. Its
qualified path is aimed at ordinary Cargo projects whose filesystem and
environmental effects fit the declared-input model. Windows, arbitrary native
build systems and targets, untested filesystems and architectures, and live
production REAPI validation remain outside the proven boundary.

Build scripts and proc macros are especially important here. Strict
whole-gate snapshots run in a sandbox that denies network access and denies
undeclared reads and writes. A project that needs an additional deterministic
input can declare it. A project with effects we cannot yet represent does not
silently receive a snapshot hit.

Operational work remains as well. Cache size and free-space thresholds are
operator configured, large-cache garbage collection needs better phase and
progress telemetry, and the command-line interface does not yet print a
friendly end-of-gate reuse summary. The JSON Lines action log is currently the
authoritative view of whole-gate hits, per-action hits, coalesced work, and
physical misses.

The larger lesson from building this is that Rust build reuse is not mainly a
question of storing `.rlib` files. It is a question of preserving Cargo's
planning semantics while making toolchain identity, native inputs, environment,
paths, outputs, and platform assumptions explicit enough to compare work from
independent processes.

For coding-agent fleets that can mean avoiding five copies of the same build.
The same mechanism also applies to CI fan-out, stacked branches, and developers
working across several clean worktrees. In every case, the useful invariant is
the same: reuse only what can be represented completely, and let Cargo remain
the source of truth.

---

Disclosure: This article was prepared with AI assistance.
