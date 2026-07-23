# cargo-reapi

`cargo-reapi` lets ordinary Cargo projects reuse compiled artifacts across
independent worktrees and processes without replacing Cargo's build graph. It
was built for massively parallel Rust agent pipelines, where many clean
worktrees otherwise repeat the same expensive checks, tests, compiler actions,
and links.

We are currently dogfooding it in our private agentic coding harness against
[Moria](https://github.com/TamedTornado/moria), a real Rust/Bevy voxel-world
substrate. Five simultaneous clean Moria consumers completed their full warm
quality gates in 24.695 seconds with zero OS-observed compiler/linker work. In
the live harness, five worktrees reaching the same cold `bevy_pbr` action
produced one physical compiler action and four coalesced waiters. The
[agent-fleet case study](docs/case-studies/moria-agent-fleet.md) includes the
measurements, integration failures, repairs, and limits.

Cargo remains the build planner and source of truth. `cargo-reapi` observes the
exact commands Cargo schedules through `RUSTC_WRAPPER`, captures and verifies
their inputs and outputs, coalesces identical concurrent work, and restores
artifacts from a shared local cache. It also includes a reclient adapter for
eligible Remote Execution API (REAPI) actions; validation against a live
production REAPI service remains a separate milestone.

`cargo-reapi` uses two nested reuse paths. An exact whole-gate hit restores the
complete Cargo target state and skips Cargo entirely. When the gate differs,
Cargo remains the planner and runs inside the strict sandbox, while individual
compiler and linker actions reuse verified cached outputs or coalesce identical
concurrent misses. Only invalidated actions execute physically, and a successful
result becomes a new whole-gate snapshot. The adversarial mutation qualification
proves this by rebuilding exactly the changed leaf and its dependants while
restoring unrelated work. The
[Moria agent-fleet case study](docs/case-studies/moria-agent-fleet.md#two-reuse-layers)
documents both paths and a live partial-match sample.

On an exact hit, the user sees no Cargo compilation stream and the JSONL action
log records `gate-snapshot-hit` or `coalesced-gate-hit`. On a replan, Cargo emits
its normal output and the action log records each `cache-hit`, `coalesced-hit`,
or physical miss. The CLI does not yet print a human-readable end-of-gate
“N actions reused” summary; that presentation improvement is listed under
[known limitations and roadmap](#known-limitations-and-roadmap).

The current-schema macOS/arm64 APFS and Linux/x86_64 XFS platform batches each
passed all 11 required qualification receipts. Independent recursive
verification rehashed 152 macOS artifacts and 192 Linux artifacts and reported
zero platform violations. A combined cross-platform aggregate is not claimed:
the macOS raw evidence tree had already been intentionally discarded before the
Linux verification run, so the enclosing two-platform command correctly
reported macOS `UNMET`.

The binding project acceptance requirements are recorded in
[`acceptance/ACCEPTANCE_CRITERIA.md`](acceptance/ACCEPTANCE_CRITERIA.md). Raw
proof trees are disposable generated artifacts, not repository content. The
committed acceptance machinery and the
[end-to-end reproduction procedure](acceptance/REPRODUCING.md) are the durable,
reproducible validation surface. Only concise benchmark statistics and pass
matrices are committed—never raw OS events, receipt trees, caches, restored
binaries, or aggregate evidence directories.

The project exists because Bazel `rules_rust` and Buck2/Reindeer both require a
second maintained build graph. That is a poor fit for arbitrary Cargo projects
and is particularly costly around workspace feature selection, build scripts,
proc macros, and Cargo-provided environment variables.

## Real-world benchmarks

The [benchmark index](benchmarks/README.md) contains the pinned Bevy linked-
binary proof, real Moria one/five/ten rotational qualification, Bro's five-job
qualification, reproduction commands, explicit SSD status, and the latest
[macOS APFS current-schema qualification](benchmarks/results/2026-07-21-macos-apfs.md)
and [Linux XFS current-schema qualification](benchmarks/results/2026-07-21-linux-xfs-schema-v3.md).
The production Bro/Moria dogfood result is recorded in
[the 2026-07-22 Linux/XFS run](benchmarks/results/2026-07-22-bro-moria-production.md).
Bro is our private agentic coding harness, so its production record is a
documented field result rather than a public reproduction of the orchestration
layer. The public Moria runner and acceptance suite reproduce and independently
observe the underlying cache, invalidation, coalescing, sandbox, and
linked-binary behavior.
Rotational results are not presented as SSD acceptance, platform qualification
is not presented as a combined cross-platform aggregate, and the README does
not treat a warm clock as a substitute for adversarial correctness or OS-level
process evidence.
The complete macOS, Linux/XFS, aggregation, benchmark-recording, and evidence-
disposal procedure is documented in
[`acceptance/REPRODUCING.md`](acceptance/REPRODUCING.md).

### Qualification coverage at a glance

The suite is exhaustive against the committed acceptance threat model, not
against every possible Rust toolchain or project. The complete
[requirement-to-runner-to-evidence map](acceptance/COVERAGE.md) identifies the
test, receipt, and independent evidence behind every row.

| Area | What must be demonstrated | macOS APFS record | Linux XFS record |
| --- | --- | --- | --- |
| Invalidation | Exact dependent rebuild set; poison, flags/configuration, external inputs, and undeclared effects cannot produce stale hits | schema-v3 pass | schema-v3 pass |
| Linked artifacts | Relocated Bevy application and test binary match a fresh control | schema-v3 pass | schema-v3 pass |
| Coalescing | One producer/one waiter, correct waiter behavior, and failure propagation | schema-v3 pass | schema-v3 pass |
| Warm populations | Complete Moria gates in 1/5/10 simultaneous clean consumers with zero physical and OS-observed compiler/linker work | schema-v3 pass | schema-v3 pass |
| Bro integration | Five simultaneous public-boundary Moria jobs with complete gates and zero warm compilation | schema-v3 pass | schema-v3 pass |
| Resources | Distinct cold work overlaps within RSS/swap bounds; a 300-second stall is infrastructure | schema-v3 pass | schema-v3 pass |
| Portability | APFS clone or Linux reflink selection is proved; portable fallback remains isolated | schema-v3 pass | schema-v3 pass |
| Evidence integrity | Runner identity, criteria, raw OS events, derived audits, and all recursive evidence hashes verify fail-closed | 152 artifacts rehashed; zero violations | 192 artifacts rehashed; zero violations |

Current deliberate limits are Windows, arbitrary Rust/native build systems and
targets, untested filesystems and architectures, and validation against a live
production REAPI service. Unsupported effects fail closed in strict mode. The
[coverage document](acceptance/COVERAGE.md#deliberate-limits) gives the exact
boundary so “exhaustive” is not used as an unbounded compatibility claim.

## Known limitations and roadmap

Cache growth currently requires operator-configured size/free-space thresholds,
and large-cache GC progress reporting is minimal: it does not yet expose
lock-wait, scan, eviction, and blob-sweep phase telemetry. The CLI also lacks a
human-readable end-of-gate reuse summary; the JSONL action log is currently the
authoritative distinction between whole-gate hits, per-action reuse, coalesced
waiters, and physical misses.

## Current status

In capture mode, Cargo runs normally, every compiler action executes locally,
and a JSON Lines action log records the compiler command, Cargo environment,
package inputs, explicit `--extern` artifacts, output directory, and content
digests. Cache mode additionally restores verified actions and whole gates as
described above. Actions carry a cross-worktree-stable key derived from
normalized paths, input content, compiler identity, platform, arguments,
environment, and outputs.

Remote REAPI eligibility is fail-closed and auditable. Metadata-only compiler
actions with fully mapped inputs and outputs can be marked eligible. Link
actions remain remotely ineligible until native libraries, linker binaries,
response files, generated linker arguments, and platform SDK inputs are
completely represented. The local shared cache does support verified linked
outputs. Identical real Cargo fixtures in different worktrees must produce the
same action key in the integration suite.

The local shared-cache backend implements content-addressed output blobs, per-action cross-process locks, atomic publication, digest verification, fixed-width cross-worktree relocation, macOS re-signing, and output materialization into independent worktrees. Native link discovery keys linker binaries, response files, native libraries, and platform SDK inputs. Concurrent identical actions execute once; distinct cold actions lease CPU and memory from one shared physical-action ledger. Cache hits never acquire a heavy-action lease and logical Cargo gates are never admission-capped.

The reclient transport adapter stages eligible actions into explicit input roots, invokes the production `rewrapper` client with declared inputs and outputs, and materializes successful outputs back into Cargo's target directory. Its platform template must bind `{os}`, `{arch}`, and `{toolchain_sha256}` so an action cannot silently execute against a mismatched worker toolchain. Real remote execution still requires an operator-provided reclient installation, a running `reproxy`, and a platform-matched REAPI service. The repository test suite exercises the complete adapter against a behaviorally faithful fake `rewrapper`; validation against a live service is the next infrastructure milestone.

The first bounded [five-worktree Moria experiment](docs/moria-acceptance-2026-07-18.md) is retained as failed evidence: it used serialized two-process waves and executed cacheable work. The later [self-reported Moria experiment](docs/moria-acceptance-2026-07-19.md) met its timing thresholds, but predates external compiler observation and is therefore also historical, unaudited evidence rather than an acceptance result. Fixed timing references and anti-escape clauses are embedded from `acceptance/contract.toml`; receipts always report whether each host met or exceeded its selected reference, while correctness still requires a complete externally observed `cargo reapi prove` report with zero warm compiler/linker work.

The current platform records report complete clean-consumer Moria gates in
8.302s / 14.264s / 25.016s on [macOS APFS](benchmarks/results/2026-07-21-macos-apfs.md)
and 6.455s / 10.818s / 18.852s on
[Linux XFS](benchmarks/results/2026-07-21-linux-xfs-schema-v3.md) for one, five,
and ten simultaneous worktrees respectively. Every warm population recorded
zero physical actions and zero OS-observed compiler/linker executions. Both
schema-v3 platform batches also passed Bevy behavioral parity, adversarial
invalidation, coalescing, resource, portability, and Bro five-job receipts.
Peak aggregate build-process RSS was 3.50 GB on macOS and 9.32 GB on Linux with
no swap growth; deliberate no-progress runs were terminated and classified as
infrastructure rather than agent feedback.

The public name is currently collision-free: a crates.io exact-name search returned no `cargo-reapi` package, and the only GitHub repository returned for the name was this project (checked 2026-07-18). That is not a crates.io reservation; publication must repeat the check.

## Usage

Whole-gate snapshots are strict by default and currently support macOS and
Linux. They require the exact audited sandbox provider release; cargo-reapi
rejects missing, older, newer, or modified installations:

```sh
npm install --global @anthropic-ai/sandbox-runtime@0.0.66
```

On macOS this uses Seatbelt through `/usr/bin/sandbox-exec`. On Linux the same
provider uses bubblewrap and requires `bwrap`, `socat`, and `rg` on `PATH`.
Sandbox-runtime is still beta infrastructure, so cargo-reapi hashes its full
recursive Node dependency closure and the platform helper executables into
every snapshot key. `CARGO_REAPI_SRT` may point to an exact pinned installation
for development and CI; it is not a version bypass.

```sh
cargo install --path .
cargo reapi --backend capture -- test
cargo reapi --backend cache --cache-dir /shared/cargo-reapi-cache -- check
cargo reapi doctor --cache-dir /shared/cargo-reapi-cache --json
cargo reapi cache stats --cache-dir /shared/cargo-reapi-cache --json
cargo reapi cache gc --cache-dir /shared/cargo-reapi-cache \
  --max-bytes 96636764160 --min-free-bytes 32212254720 \
  --target-free-bytes 48318382080 --json
cargo reapi contract verify --path acceptance/contract.toml
cargo reapi prove action-log \
  --action-log /path/to/actions.jsonl \
  --rustc-trace /path/to/external-rustc-trace \
  --worktree /path/to/audited-worktree \
  --report /path/to/proof.json
cargo reapi --backend reapi \
  --rewrapper /opt/reclient/rewrapper \
  --rewrapper-cfg /etc/cargo-reapi/rewrapper.cfg \
  --reclient-staging-dir /shared/cargo-reapi-stage \
  --reclient-platform 'OSFamily={os},Arch={arch},toolchain_sha256={toolchain_sha256}' \
  -- check
```

Long-lived runtimes may configure the backend and cache once, then use the ordinary
driver form `cargo-reapi check`, `cargo-reapi clippy ...`, or `cargo-reapi test`.
`CARGO_REAPI_BACKEND`, `CARGO_REAPI_CACHE_DIR`, and the other documented driver
options are environment-backed defaults; explicit CLI flags still take precedence.
`CARGO_REAPI_RESOURCE_CPU_CAPACITY` and
`CARGO_REAPI_RESOURCE_MEMORY_GIB_CAPACITY` define one host-wide physical-action
ledger; they do not cap logical Cargo gates. `doctor` proves the pinned sandbox,
copy-on-write selection, configured resource capacity, and cache readability
before a worker is admitted. `cache gc` takes an exclusive maintenance lease,
waits for all active restores and producers, evicts least-recently-used action
and whole-gate entries, and then removes unreferenced blobs. A dry run reports
the same selection without mutation.

The complete local proof runner has no concurrency or threshold flags. It cold-seeds one
producer, retires that producer path, then runs the full canonical gate in one, five, and
ten clean worktrees. The five and ten populations are launched simultaneously and their
timestamps and action logs are revalidated by the embedded contract:

```sh
sudo -v
target/release/cargo-reapi-auditor run \
  --report /path/to/proof-report/resource-proof.json -- \
  acceptance/run-moria-local.sh \
  /path/to/Moria \
  /shared/cargo-reapi-cache \
  /path/to/proof-report \
  ssd
```

The final local runner requires macOS Full Disk Access for `/usr/bin/eslogger`
and a current operator-authorized `sudo` session. Its tool-selected OS event
stream is independent of cargo-reapi's wrapper and action log. Rotational mode
uses the fixed compatibility clocks but cannot substitute for the SSD result.

The default log is `target/cargo-reapi/actions.jsonl`. Cache mode deliberately requires an explicit cache directory so separate worktrees share only the operator-selected store. REAPI mode expects `reproxy` to be started and stopped through reclient's `bootstrap` lifecycle outside each individual Cargo action. To prove there is no semantic change, compare the exit status and artifacts with the same Cargo command without the wrapper.

Strict snapshot execution denies network access, denies reads outside the
workspace/package/toolchain/configuration/declared-input set, and denies writes
outside target, cache, action log, Cargo locks, and provider-private temporary
state. Build scripts and proc macros that need another deterministic input must
receive it through `--declared-input`; undeclared filesystem or network effects
fail the gate and publish no snapshot. `--snapshot-policy off` is an explicit
uncached compatibility mode, not a fallback: strict mode never silently
degrades when the provider or an OS primitive is unavailable.

For Linux container consumers, build the repository's image rather than bind-mounting a host binary across an OS boundary:

```sh
docker build -t cargo-reapi:local .
```

The image is also suitable as an init artifact source: its executable is at `/usr/local/bin/cargo-reapi`. Bro's optional local overlay uses that boundary, so cargo-reapi remains independently built and is not copied into Bro's source tree.

## Design constraints

- Cargo owns dependency resolution, features, build scripts, proc macros, profiles, and command ordering.
- The wrapper may distribute compiler actions; it must not regenerate Cargo's graph.
- Action keys include all declared inputs, the toolchain identity, compiler arguments, and relevant environment.
- Remote execution must match the host/target platform contract. Native test and proc-macro artifacts cannot cross OS or architecture boundaries.
- Missing inputs or unsupported actions fail closed or execute locally according to explicit policy.
- Logical quality gates remain concurrent. The shared ledger bounds only physical compiler/linker misses; identical work coalesces and cache hits proceed without heavy-action admission.

See [docs/architecture.md](docs/architecture.md) for the implementation boundary and milestones.

## Project policy

This is infrastructure built first for the Moria/Bro workload and shared in public as-is. Issues with reproducible action captures are welcome, but publication does not promise compatibility with every Cargo project, hosted workers, or support response times. Correctness and fail-closed behavior take priority over backend coverage.
