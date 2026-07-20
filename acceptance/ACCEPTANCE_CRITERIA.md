# cargo-reapi acceptance criteria

Status: **macOS/arm64 empirical qualification passed, acceptance-gap closure pending; Linux/x86_64 qualification pending; publication-grade multi-platform aggregate acceptance not achieved.**

This document is the acceptance authority for cargo-reapi. A benchmark, test
binary, Moria run, Bro run, or `PASS` line is only partial evidence. The project
is accepted only when one aggregate proof requires and validates every receipt
defined below.

These criteria must not be weakened to make an implementation pass. A proposed
change to a criterion must be reviewed as a contract change before the affected
experiment runs. Results produced under different criteria remain historical
evidence and cannot be relabelled as current acceptance.

## 1. Evidence integrity

The final aggregate proof must fail closed unless every required receipt is
present. Every receipt must identify the same:

- acceptance-contract digest;
- source revision or source-tree digest;
- cargo-reapi executable digest;
- Cargo and Rust toolchain identity;
- operating system and architecture;
- run identifier and wall-clock interval.

The aggregate verifier must reject missing, stale, mismatched, self-contradictory,
or failed receipts. `contract verify` alone is not acceptance. Unit-test success
alone is not acceptance. Timing alone is not acceptance.

Cargo-reapi's action log is not independent evidence of compiler absence.
Zero physical compiler or linker work must also be established by an observer
outside the cargo-reapi process and outside its report-generation path. The
acceptance observer must audit process execution at the operating-system level
(for example, an appropriate macOS process observer, Linux `strace`/ptrace
observer, or Windows ETW observer). An injected `RUSTC` wrapper is useful
diagnostic evidence but is not sufficient by itself for final acceptance.

## 2. Snapshot-key completeness and timestamp safety

Refreshing restored target timestamps is permitted only after every input that
could affect Cargo freshness or an artifact has been represented in the key or
has been independently revalidated. A refreshed timestamp must never conceal a
stale input.

The keyed or revalidated input set must include at least:

- all workspace source, manifests, lockfiles, configuration, symlinks, and
  relevant file modes;
- Cargo arguments, selected profile, features, target, and relevant complete
  environment;
- `RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS`, including values supplied through
  workspace, ancestor, and `CARGO_HOME` configuration;
- Cargo and Rust toolchain identity, linker identity, SDK identity, native
  libraries, response files, and generated linker inputs;
- generated outputs and build-script declarations;
- every external path dependency and its relevant contents;
- environment values consumed by build scripts and proc macros;
- filesystem inputs read outside the worktree by build scripts or proc macros.

Network-dependent compiler, build-script, or proc-macro actions are not safely
cacheable merely because a URL or environment variable is keyed. Acceptance
requires them to be sandboxed with reproducible declared inputs or rejected from
snapshot reuse. Likewise, undeclared filesystem reads must be observed and keyed
or must make the snapshot ineligible. Cargo-reapi may fail closed; it may not
silently assume these effects do not exist.

## 3. Required adversarial correctness receipts

Each test below has a binary outcome and must use a fresh cache unless the test
explicitly exercises a previously populated cache.

### 3.1 Exact mutation invalidation

1. Build and publish a workspace containing a leaf crate, its dependants, and an
   unrelated crate.
2. Delete the producer worktree and target.
3. Restore into a clean worktree, mutate one line in the leaf, and run the gate.
4. Independently observe compiler processes.
5. Require the observed rebuilt workspace-crate set to equal exactly the leaf
   and its transitive dependants. The unrelated crate must not rebuild.
6. Execute the resulting binary and require behavior reflecting the mutation.

A fast snapshot hit with no required rebuild is a failure, not a performance win.

### 3.2 Poison rejection

1. Restore a clean consumer from a deleted producer.
2. Add a deliberately failing test in a dependency.
3. Require the dependency and affected dependants to rebuild.
4. Require the gate and test process to fail for the deliberate reason.
5. Reject any gate-snapshot-hit claim for the poisoned state.

The cache must be able to say no.

### 3.3 Flags, profiles, and Cargo configuration

Independently change each of the following in only one clean consumer and
require a cache miss plus behavior matching the changed value:

- `RUSTFLAGS` in the environment;
- `CARGO_ENCODED_RUSTFLAGS`;
- `.cargo/config.toml` `build.rustflags`;
- an ancestor Cargo configuration;
- a `CARGO_HOME` Cargo configuration;
- a Cargo profile setting;
- selected features and compilation target.

### 3.4 External and generated inputs

Require correct invalidation and resulting behavior for:

- a path dependency outside the worktree;
- a build-script `rerun-if-changed` file outside the worktree;
- a build-script `rerun-if-env-changed` variable;
- an environment variable consumed during proc-macro expansion;
- an undeclared external filesystem read by `build.rs`;
- an undeclared external filesystem read by a proc macro;
- a deterministic local network dependency whose response changes.

The final three cases must either rebuild correctly because the effects were
observed or fail closed because the action is ineligible. A stale hit fails.

### 3.5 Executable and test-binary integrity

Use a pinned Bevy fixture because Bevy, proc macros, native dependencies, and
large linked artifacts are part of the target problem rather than optional
compatibility.

1. Produce linked application and test binaries in a fresh producer.
2. Delete the producer worktree and target.
3. Restore them into a differently located consumer.
4. Independently build an equivalent fresh control at the consumer location.
5. Run restored and fresh application and test binaries.
6. Compare exit status, stdout, stderr, test enumeration, and observable
   behavior, normalizing only fields intentionally specific to the worktree.
7. Require embedded paths to resolve to the consumer, not the producer.
8. Require valid platform signatures after relocation on macOS.
9. Require the external process observer to report no compiler or linker in the
   restored consumer.

“The binary executed” is not sufficient.

### 3.6 Coalescing correctness

1. Launch two identical cold misses simultaneously from independent worktrees.
2. Require exactly one physical producer and exactly one waiter.
3. Require the waiter to report a coalesced restore, not an independent hit or
   second producer.
4. Require an OS-level process observer to see compiler/linker work for only the
   producer.
5. Run the waiter's tests and binaries and compare their behavior with the
   producer or a fresh control.
6. Repeat with a deliberately failing producer and require all waiters to fail;
   no partial artifact may be published.

## 4. Resource-ledger and stall receipts

Synthetic token tests do not prove resource feasibility. A real monitored build
receipt must establish all of the following on the acceptance host:

- one cross-process ledger controls physical compiler, linker, and signing work;
- logical Cargo gates are not admission-capped or serialized;
- identical actions coalesce while distinct physical keys overlap;
- at least two distinct physical actions overlap during the cold-work test;
- peak aggregate build-process RSS is at most 15 GiB;
- swap growth during the measured build interval is at most 512 MiB;
- CPU, RSS, swap, process ancestry, lease ownership, and action identity are
  measured externally rather than inferred from configured token weights;
- a 300-second interval with no compiler/linker progress is classified as an
  infrastructure stall, terminates the affected work safely, and is not
  reported to an agent as a code/test failure.

Configured estimates such as “ordinary compile = 2 GiB” or “link = 7 GiB” may
guide scheduling but cannot serve as the measurement receipt.

## 5. Durable Moria population receipts

The canonical Moria gate is, without substitutions:

1. `cargo fmt --all -- --check`
2. `cargo check --all-targets`
3. `cargo clippy --all-targets -- -D warnings`
4. `cargo test`

For every population:

- the repository is clean;
- a complete producer gate exits successfully;
- the producer worktree and target are deleted before any consumer starts;
- every consumer begins with an empty target;
- every consumer runs the entire canonical gate and all Moria tests pass;
- all members start before any member completes;
- no whole-gate admission limit, serialized wave, hidden two-job cap, or
  compiler-only substitution is allowed;
- cargo-reapi reports zero cacheable physical actions in warm consumers;
- the OS-level observer independently reports zero compiler and linker actions.

The original SSD deadlines are immutable:

- one clean consumer: at most 60 seconds;
- five simultaneous clean consumers: at most 120 seconds;
- ten simultaneous clean consumers: at most 120 seconds.

Rotational-storage results may use the separately fixed 300/900/1800-second
qualification clocks, but they are storage-compatibility evidence, not a
substitute for the original SSD acceptance result.

## 6. Bro integration receipt

Bro must launch at least five Moria jobs simultaneously through cargo-reapi's
standalone public command interface. Bro must not own cargo-reapi source or
deployment, and it must not impose a hidden cargo-reapi population cap.

The integration receipt must require:

- a completed exact-environment producer;
- producer deletion before consumers start;
- at least five consumers starting before any completes;
- the complete canonical Moria gate in every consumer;
- passing Moria tests in every consumer;
- zero cargo-reapi-classified physical warm actions;
- zero OS-observed compiler/linker processes in consumers;
- the applicable fixed storage deadline.

GitHub Actions, pull requests, or deployment are not part of this local
development acceptance path. CI may provide additional validation but cannot
replace these receipts.

## 7. Portability and storage behavior

Correctness must not depend on an undocumented macOS-only copy mechanism.

- macOS/APFS should use copy-on-write cloning when source and destination permit;
- Linux should use reflinks when supported and fall back to an isolated portable
  copy when not supported;
- every fallback must preserve required files, permissions, links, and consumer
  isolation;
- storage-specific timing profiles must be selected explicitly and recorded;
- failure to obtain a clone must never silently become a false cache hit.

## 8. Publication gate

Before any public repository or package publication:

- check crates.io and public source hosts for `cargo-reapi` name collisions;
- publish the real benchmark receipts and their reproduction commands;
- state the supported hermeticity boundary and fail-closed cases prominently;
- do not claim acceptance until the aggregate verifier passes this document.

## 9. Qualification history

On 2026-07-20, the local macOS/arm64 SSD experiments produced passing timing,
behavior, and resource results. Subsequent hostile review found provenance and
evidence-binding gaps in the aggregate harness, so those experiments remain
historical evidence while the gaps are closed.

No Linux-host qualification or public release was established by that run. The
earlier rotational proof remains storage-compatibility evidence only, and the
interrupted partial SSD proof remains invalid.
