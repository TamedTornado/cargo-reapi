# cargo-reapi

`cargo-reapi` is an experimental Cargo-native path to remote execution. Cargo remains the build planner and source of truth; the tool observes the exact `rustc` commands Cargo schedules through `RUSTC_WRAPPER`, captures their inputs, and will translate those actions to the Remote Execution API (REAPI).

The project exists because Bazel `rules_rust` and Buck2/Reindeer both require a second maintained build graph. That is a poor fit for arbitrary Cargo projects and is particularly costly around workspace feature selection, build scripts, proc macros, and Cargo-provided environment variables.

## Current status

The capture and deterministic-action milestones work: Cargo runs normally, every compiler action still executes locally, and a JSON Lines action log records the compiler command, Cargo environment, package inputs, explicit `--extern` artifacts, output directory, and content digests. Actions also carry a cross-worktree-stable key derived from normalized paths, input content, compiler identity, platform, arguments, environment, and outputs.

Remote eligibility is fail-closed and auditable. Metadata-only compiler actions with fully mapped inputs and outputs can be marked eligible. Link actions are explicitly ineligible until native libraries, linker binaries, response files, generated linker arguments, and platform SDK inputs are completely represented. Identical real Cargo fixtures in different worktrees must produce the same action key in the integration suite.

The local shared-cache backend now implements the complete action lifecycle: content-addressed output blobs, per-action cross-process locks, atomic publication, digest verification, dep-info path rewriting, and output materialization into independent worktrees. Concurrent identical actions execute once; waiters restore the published result. Corrupt blobs are rejected and rebuilt locally. Failed actions and remotely ineligible link actions are never published.

The reclient adapter for remote CAS upload and action execution is the next milestone. Reusing the production `rewrapper`/`reproxy` implementation keeps this project focused on Cargo and Rust action discovery. `--backend reapi` fails closed until that adapter is implemented; it never silently falls back to an unverified remote result.

The public name is currently collision-free: a crates.io exact-name search returned no `cargo-reapi` package, and the only GitHub repository returned for the name was this project (checked 2026-07-18). That is not a crates.io reservation; publication must repeat the check.

## Usage

```sh
cargo install --path .
cargo reapi --backend capture -- test
cargo reapi --backend cache --cache-dir /shared/cargo-reapi-cache -- check
```

The default log is `target/cargo-reapi/actions.jsonl`. Cache mode deliberately requires an explicit cache directory so separate worktrees share only the operator-selected store. To prove there is no semantic change, compare the exit status and artifacts with the same Cargo command without the wrapper.

## Design constraints

- Cargo owns dependency resolution, features, build scripts, proc macros, profiles, and command ordering.
- The wrapper may distribute compiler actions; it must not regenerate Cargo's graph.
- Action keys include all declared inputs, the toolchain identity, compiler arguments, and relevant environment.
- Remote execution must match the host/target platform contract. Native test and proc-macro artifacts cannot cross OS or architecture boundaries.
- Missing inputs or unsupported actions fail closed or execute locally according to explicit policy.
- Quality-gate concurrency is still bounded outside this tool. Remote execution changes where work runs; it does not grant unbounded scheduling.

See [docs/architecture.md](docs/architecture.md) for the implementation boundary and milestones.

## Project policy

This is infrastructure built first for the Moria/Bro workload and shared in public as-is. Issues with reproducible action captures are welcome, but publication does not promise compatibility with every Cargo project, hosted workers, or support response times. Correctness and fail-closed behavior take priority over backend coverage.
