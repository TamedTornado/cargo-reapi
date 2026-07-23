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

After the repair, a retained action log from a fresh Moria agent test contained
74 compiler-wrapper records:

| Result | Records |
| --- | ---: |
| Cache hits | 31 |
| Coalesced hits | 10 |
| Producer misses | 21 |
| Non-cacheable compiler capability probes | 6 |

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
