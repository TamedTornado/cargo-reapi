# Moria agent-fleet dogfood

This case study records how `cargo-reapi` behaves inside our private agentic
coding harness while five agents work on
[Moria](https://github.com/TamedTornado/moria), a Rust/Bevy voxel-world
substrate. The orchestration system is private; the cache implementation,
acceptance contract, qualification runners, and Moria source are public.

The results are promising production dogfood evidence, not a claim that the
private harness itself is independently reproducible. The underlying cache
mechanics are covered by the public
[macOS APFS](../../benchmarks/results/2026-07-21-macos-apfs.md) and
[Linux XFS](../../benchmarks/results/2026-07-21-linux-xfs-schema-v3.md)
qualification runs.

## The problem

Five independent agents mean five independent Git worktrees. Each logical
quality gate still asks Cargo to plan a complete project:

```text
cargo fmt
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

For a Bevy project, allowing every worktree to compile and link the same graph
independently duplicates compiler work, multiplies peak memory, and leaves
large mutable target trees in every worktree.

Setting Cargo's job count to one reduced the damage but did not solve the
problem: five agents could still start five independent single-threaded build
graphs. The required boundary was one host-wide resource and cache authority
across every worktree.

## Controlled baseline

Before using the cache in the live harness, the public qualification suite
seeded one cold Moria producer and launched five clean consumers simultaneously
on Linux/XFS:

| Measurement | Result |
| --- | ---: |
| Cold complete gate | 3,125.608s |
| Cold peak process-tree RSS | 6.37 GB |
| Cold swap growth | 144 MB |
| Five simultaneous warm gates | 24.695s |
| Warm peak process-tree RSS | 879 MB |
| Warm swap growth | 0 bytes |

Every consumer began with an empty target directory, ran the complete gate, and
recorded three whole-gate snapshot hits. External OS observation found zero
compiler or linker executions during the warm population. The detailed
statistics and pinned revisions are in the
[production benchmark record](../../benchmarks/results/2026-07-22-bro-moria-production.md).

The broader current-schema qualification also exercises adversarial
invalidation, poison propagation, configuration and environment changes,
relocated Bevy binaries, concurrent miss coalescing, undeclared reads, network
denial, and recursive evidence verification. A fast warm clock alone is not
treated as proof.

## Two reuse layers

`cargo-reapi` has two nested reuse layers:

1. An exact whole-gate snapshot can restore the complete Cargo target state
   before Cargo runs. This is the fast path exercised by the one/five/ten clean
   Moria populations above.
2. If the whole-gate key does not match, Cargo plans the gate normally and the
   compiler wrapper applies the action cache to each `rustc` or linker action.
   Unchanged actions can still be restored or coalesced while changed actions
   execute and publish new outputs.

The second layer is what makes partial matches useful. It is not inferred only
from production telemetry: the public
[exact-mutation acceptance](../../acceptance/COVERAGE.md) changes a leaf crate
and requires OS-observed execution of exactly the leaf and its transitive
dependants, while an unrelated crate must remain uncompiled. Wrapper attribution
must match the OS-derived set.

## What happened under live load

### A real Bevy miss executed once

During a cold production population, five simultaneous Cargo processes reached
`bevy_pbr`. All five worktrees computed action key:

```text
5b1f68bad75f30f384bc1595e445b41296dafd73eee16e9b0d887bfd7a217fb6
```

The retained action records showed one `local-cache-miss`, four
`coalesced-hit` results, and five successful callers. Cargo still walked five
logical graphs, but the shared physical action ran once.

### Agent builds initially missed the shared cache

The first live agent runs exposed two integration failures:

1. The private harness passed orchestration variables such as storage and disk
   admission settings into Cargo. `cargo-reapi` correctly keyed variables
   visible to build scripts and proc macros, so changing host configuration
   invalidated otherwise identical Rust actions.
2. Agent containers mounted their Cargo target at `/tmp/bro-cargo-target` but
   left `CARGO_REAPI_TARGET_ROOT` and the action log pointed at the hidden host
   path. Those actions were correctly classified as ineligible rather than
   cached unsafely.

The repair did not teach the cache to ignore arbitrary environment. The
harness now removes its reserved orchestration namespaces before invoking
Cargo, so they cannot be read by project code, and rewrites all target-bearing
paths together:

```text
CARGO_TARGET_DIR=/tmp/bro-cargo-target
CARGO_REAPI_TARGET_ROOT=/tmp/bro-cargo-target
CARGO_REAPI_ACTION_LOG=/tmp/bro-cargo-target/cargo-reapi/actions.jsonl
```

`cargo-reapi` additionally removes three proven runtime-plumbing values from
compiler children and keys: the per-session thread ID, container hostname, and
shell nesting level. Arbitrary project environment and `PATH` remain keyed.

The defect records confirm that all four integration failures went in the safe
direction: environment and ephemeral-session differences caused extra misses,
the target-root disagreement made actions ineligible, and the
`/etc/alternatives` gap failed the build loudly. None produced a stale artifact
or false cache hit; availability failed, never correctness, as intended by the
fail-closed design.

After the repair, an operator sampled the still-growing action log from a fresh
Moria agent test. At 18:42:18 UTC, its complete execution histogram contained
68 compiler-wrapper records:

| Result | Records |
| --- | ---: |
| Cache hits | 31 |
| Coalesced hits | 10 |
| Producer misses | 21 |
| Non-cacheable compiler capability probes | 6 |

The internally complete snapshot therefore contained 62 cacheable actions:
41 (66.1%) reused existing or concurrently produced outputs and 21 executed as
producers. This is the production partial-match result: a whole-gate miss did
not become a full rebuild.

At 18:42:32 UTC, while the same build was still running, a separate line count
observed 74 records. The six records appended between those observations were
eligible, because the second observation still found only the original six
ineligible probes, but their execution outcomes were not re-histogrammed.
**The 74-record observation is UNRECONCILED.** It is retained here, but the
earlier 68-record histogram must not be presented as a partition of it. The raw
operational log was subsequently disposed under the project's evidence-
retention policy, and a final recovery search of the original Docker volume,
the migrated action-log volumes, and the operator transcript did not recover
those six outcomes. They are not guessed.

Every record with declared outputs was cache eligible. None of the removed
runtime-plumbing fields appeared in the keyed environment.

### Linux native-tool discovery found a real sandbox gap

A Bevy gate reached `basis-universal-sys` and failed because its build script
could not resolve `c++`. The executable existed, but Debian resolved it through
`/etc/alternatives`, which the strict snapshot sandbox had hidden.

The repair admits that read-only path and adds an integration fixture whose
real `build.rs` invokes `c++`, archives an object, links it, and executes the
result. This is why Moria remains part of the test strategy: a synthetic
Rust-only fixture would not have exercised the native dependency graph that
real Bevy projects carry.

### Cold source acquisition exposed a qualification blind spot

The first implementation issue in a later Moria wave required
`futures-channel 0.3.33`, which was absent from the shared Cargo home.
cargo-reapi's strict-snapshot preparation unconditionally invoked `cargo
metadata --offline`, so the quality gate failed before any compiler action.
The production harness initially treated that as project feedback, and an
agent produced a lockfile-only repair that downgraded dependencies to versions
already present on the host. The run was stopped and that change was closed
without merge.

The earlier qualification did not catch this because its runner explicitly
prefetched every fixture. The regression now creates a locked Git dependency
and two empty Cargo homes. An explicitly offline invocation must remain
offline and fail without acquiring the dependency; a normal first invocation
must acquire the locked source before entering the network-denied transition
sandbox. The test failed against the old binary and passed with the repair,
followed by the complete formatting, Clippy, and test suite.

The live Moria retry then passed with the original lockfile. External
inspection found both the `futures-channel-0.3.33.crate` archive and extracted
source in the shared Cargo home, and the replacement PR contained only the
original implementation commit. This result distinguishes source acquisition
from compiler/build-script execution: Cargo may provision the resolved source
graph before strict execution, while the transition itself remains
network-denied.

### Integration-test companion binaries exposed incomplete environment relocation

A later Moria quality gate restored the `moria_qualify` binary action from the
shared cache, then every integration test that used
`env!("CARGO_BIN_EXE_moria-qualify")` failed to launch it. The restored test
binary contained the deleted producer worktree's absolute companion path.
This was a cargo-reapi defect, not project feedback: the action key normalized
Cargo's environment across worktrees, but compiler execution relocated only a
fixed inventory of path variables and omitted Cargo's dynamically named
`CARGO_BIN_EXE_<name>` values.

The repair removes that name inventory. Every compiler environment value is now
scanned for declared package, workspace, target, and toolchain roots before
rustc embeds it. This also covers customer-defined config-relative `[env]`
values and arbitrary paths emitted through `cargo::rustc-env`.
The cross-worktree regression uses different-length producer and consumer
paths, deletes the producer, requires an action-cache hit, launches the restored
companion binary, and proves both custom path sources resolve inside the
consumer. The complete audited surface and remaining remote-execution boundary
are recorded in the
[compiler environment relocation audit](../compiler-environment-relocation.md).

### Nested target leakage needed a product diagnostic

The Linux verification initially inherited the outer harness's
`CARGO_TARGET_DIR` into nested fixture builds. That made otherwise independent
workspaces share the target containing the running cargo-reapi test binary.
The resulting read-only action-log errors and `Text file busy` failures did not
identify the configuration mistake.

cargo-reapi now rejects the unsafe topology before starting Cargo when its own
resolved executable is inside the active target root. The error identifies
inherited `CARGO_TARGET_DIR` as the likely cause and tells the operator to
remove it from the child or use a separate external target. This is deliberately
not a ban on shared external targets: a separate regression proves an installed
cargo-reapi can still drive an explicitly configured external
`CARGO_TARGET_DIR`. Nested test launchers also clear the parent target unless
the test is specifically exercising that contract.

After removing the leaked parent variable, the production Linux integration
binary completed 32 tests with zero failures and three intentional ignores in
222.23 seconds. The failed contaminated invocation is not counted as product
evidence.

The follow-up verification of committed repair `2125e67` rebuilt the test
binaries from a clean source archive inside Bro's production build worker.
Its Linux results were:

- 69 cargo-reapi core tests passed;
- 3 receipt-auditor and 4 exec-auditor tests passed;
- 34 integration tests passed, 3 dedicated acceptance tests were intentionally
  ignored, and no test failed, in 224.27 seconds;
- the resource-harness and stall-auditor tests passed; and
- the four Bevy tests remained intentionally delegated to their phased
  acceptance runner.

The same deployed binary then reran Moria issue 404's real quality gate. Six
cacheable Moria actions restored from the shared action cache: `moria` twice,
`public_boundary`, `moria_qualify` twice, and `qualifier_scaffold`. There were
zero cacheable misses. The remaining rustc records were successful
non-cacheable control probes. The gate completed successfully before the run
was stopped again, so no downstream product work is included in this
infrastructure result.

### Five cold gates showed that a full queue is not a stall

On 2026-10-04 Bro split one large Moria issue into five parallel issues. Each
agent changed different source, so five cold, divergent gates compiled at once
against one host-wide physical-action ledger of 20 CPU tokens. cargo-reapi
`4a893c1` classified any action that waited 300 seconds for a lease as an
infrastructure stall, even while the other gates kept completing actions, so
two of the five gates failed on a busy but healthy host.

The acceptance criteria define a stall as 300 seconds with no compiler or
linker progress. `275284e` makes releasing a lease rewrite a progress marker in
the ledger and restarts a waiter's stall clock whenever it changes. After the
fix, the same five-gate load filled the 20-token ledger with no infrastructure
stall, and 71% of cacheable actions reused outputs. The
[field observation](../../benchmarks/results/2026-10-04-bro-moria-five-cold-gates.md)
records the samples and the action histogram.

## Resource behavior

At one measured five-session production point:

- all five logical agent slots were occupied;
- the single shared build worker serving five gates used approximately
  3.1 GB RAM and 2.06 CPU cores;
- individual agent containers used approximately 47–399 MB RAM;
- host load was 11.79 / 8.88 / 7.04 on 20 logical CPUs;
- approximately 54 GB of host memory remained available.

After the server received a dedicated 1.9 TiB reflink-enabled XFS volume and
additional RAM, another sample separated logical and physical work more
clearly:

- five agents and five build jobs were active, with thirteen more build jobs
  queued;
- the build worker used approximately 3.9 CPU cores;
- external RSS across its 48 processes was approximately 1.49 GiB;
- approximately 109 GiB of host memory remained available;
- swap use was below 1 MiB.

These are point-in-time operational measurements, not universal capacity
claims. They demonstrate that agent admission, logical quality-gate admission,
and physical compiler admission can be controlled independently.

## What the dogfood run proved—and did not prove

It provides production evidence that:

- independent worktrees can share real Rust/Bevy compiler and linker outputs;
- identical simultaneous misses can become one producer and multiple waiters;
- five complete warm quality gates can overlap without compiler/linker work;
- strict cache eligibility exposes integration mistakes instead of silently
  serving unsafe hits;
- the host-wide physical-action ledger bounds heavy work without serializing
  logical gates.

It also exposed operational work outside the cache kernel:

- mutable target trees and container storage still need explicit reclamation;
- cache garbage collection needs phase/progress telemetry at large scale;
- build admission and storage-recovery watermarks must agree;
- container and orchestration environment must be separated from project build
  inputs at the process boundary;
- filesystem page cache can make container memory accounting misleading.

The current public implementation is a qualified local shared cache. It
contains a REAPI transport adapter, but validation against a live production
remote-execution service remains open. Windows and arbitrary native build
systems are also outside the qualified boundary.

## Why this matters beyond Moria

The workload pattern is no longer unusual: coding agents, CI fan-out, large
change stacks, and release branches all create multiple clean consumers of the
same Rust graph. Teams experiencing long Bevy links, duplicated monorepo
compilation, memory exhaustion under parallel CI, or many-agent worktree
contention have the same underlying problem.

The engagement shape is measurable:

1. capture the current build graph without replacing Cargo;
2. classify duplicated work and hidden environmental inputs;
3. establish adversarial correctness and binary-integrity baselines;
4. coalesce identical misses and restore verified outputs;
5. size physical-action admission from real host memory and CPU;
6. dogfood under production load and retain honest pass statistics.

The operational result is not merely a cache installation. It is a measured
way to make parallel Rust delivery faster without weakening Cargo's correctness
boundary.
