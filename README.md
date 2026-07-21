# cargo-reapi

The binding project acceptance requirements are recorded in
[`acceptance/ACCEPTANCE_CRITERIA.md`](acceptance/ACCEPTANCE_CRITERIA.md). The
current-model macOS/arm64 and Linux/x86_64 qualification is not yet established
by a passing multi-platform aggregate. Historical macOS empirical qualification
and the preceding-model Linux local qualification passed; a matching
multi-platform aggregate has not yet been regenerated under the current model.
Raw proof trees are disposable generated artifacts, not repository content. The
committed acceptance machinery and the
[end-to-end reproduction procedure](acceptance/REPRODUCING.md) are the durable
proof surface.

`cargo-reapi` is an experimental Cargo-native path to remote execution. Cargo remains the build planner and source of truth; the tool observes the exact `rustc` commands Cargo schedules through `RUSTC_WRAPPER`, captures their inputs, and will translate those actions to the Remote Execution API (REAPI).

The project exists because Bazel `rules_rust` and Buck2/Reindeer both require a second maintained build graph. That is a poor fit for arbitrary Cargo projects and is particularly costly around workspace feature selection, build scripts, proc macros, and Cargo-provided environment variables.

## Real-world benchmarks

The [benchmark index](benchmarks/README.md) contains the pinned Bevy linked-
binary proof, real Moria one/five/ten rotational qualification, Bro's five-job
qualification, reproduction commands, explicit SSD status, and the latest
[macOS APFS current-schema qualification](benchmarks/results/2026-07-21-macos-apfs.md)
and [final preceding-model Linux XFS statistics](benchmarks/results/2026-07-20-linux-xfs.md).
Rotational results are not presented as SSD acceptance, the partial Linux batch
is not presented as full qualification, and the README does not treat a warm
clock as a substitute for adversarial correctness or OS-level process evidence.
The complete macOS, Linux/XFS, aggregation, benchmark-recording, and evidence-
disposal procedure is documented in
[`acceptance/REPRODUCING.md`](acceptance/REPRODUCING.md).

## Current status

The capture and deterministic-action milestones work: Cargo runs normally, every compiler action still executes locally, and a JSON Lines action log records the compiler command, Cargo environment, package inputs, explicit `--extern` artifacts, output directory, and content digests. Actions also carry a cross-worktree-stable key derived from normalized paths, input content, compiler identity, platform, arguments, environment, and outputs.

Remote eligibility is fail-closed and auditable. Metadata-only compiler actions with fully mapped inputs and outputs can be marked eligible. Link actions are explicitly ineligible until native libraries, linker binaries, response files, generated linker arguments, and platform SDK inputs are completely represented. Identical real Cargo fixtures in different worktrees must produce the same action key in the integration suite.

The local shared-cache backend implements content-addressed output blobs, per-action cross-process locks, atomic publication, digest verification, fixed-width cross-worktree relocation, macOS re-signing, and output materialization into independent worktrees. Native link discovery keys linker binaries, response files, native libraries, and platform SDK inputs. Concurrent identical actions execute once; distinct cold actions lease CPU and memory from one shared physical-action ledger. Cache hits never acquire a heavy-action lease and logical Cargo gates are never admission-capped.

The reclient transport adapter stages eligible actions into explicit input roots, invokes the production `rewrapper` client with declared inputs and outputs, and materializes successful outputs back into Cargo's target directory. Its platform template must bind `{os}`, `{arch}`, and `{toolchain_sha256}` so an action cannot silently execute against a mismatched worker toolchain. Real remote execution still requires an operator-provided reclient installation, a running `reproxy`, and a platform-matched REAPI service. The repository test suite exercises the complete adapter against a behaviorally faithful fake `rewrapper`; validation against a live service is the next infrastructure milestone.

The first bounded [five-worktree Moria experiment](docs/moria-acceptance-2026-07-18.md) is retained as failed evidence: it used serialized two-process waves and executed cacheable work. The later [self-reported Moria experiment](docs/moria-acceptance-2026-07-19.md) met its timing thresholds, but predates external compiler observation and is therefore also historical, unaudited evidence rather than an acceptance result. Fixed timing references and anti-escape clauses are embedded from `acceptance/contract.toml`; receipts always report whether each host met or exceeded its selected reference, while correctness still requires a complete externally observed `cargo reapi prove` report with zero warm compiler/linker work.

The current [real-world benchmark record](benchmarks/results/2026-07-19-local.md#moria-ssd-acceptance-receipt) contains the first externally observed Moria SSD pass: 9.441s for one worktree, 14.918s for five simultaneous worktrees, and 26.639s for ten simultaneous worktrees, with zero OS-observed compiler/linker executions in every warm population. The 2026-07-20 aggregate run also passed Bevy behavioral parity, adversarial invalidation, coalescing, resource, portability, and Bro five-job receipts. Peak aggregate RSS was 5.20 GB with no swap growth; three distinct heavy actions made simultaneous progress; and a live 300-second no-progress run was rejected as infrastructure rather than agent feedback.

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
