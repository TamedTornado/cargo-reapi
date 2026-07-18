# cargo-reapi

`cargo-reapi` is an experimental Cargo-native path to remote execution. Cargo remains the build planner and source of truth; the tool observes the exact `rustc` commands Cargo schedules through `RUSTC_WRAPPER`, captures their inputs, and will translate those actions to the Remote Execution API (REAPI).

The project exists because Bazel `rules_rust` and Buck2/Reindeer both require a second maintained build graph. That is a poor fit for arbitrary Cargo projects and is particularly costly around workspace feature selection, build scripts, proc macros, and Cargo-provided environment variables.

## Current status

The capture and deterministic-action milestones work: Cargo runs normally, every compiler action still executes locally, and a JSON Lines action log records the compiler command, Cargo environment, package inputs, explicit `--extern` artifacts, output directory, and content digests. Actions also carry a cross-worktree-stable key derived from normalized paths, input content, compiler identity, platform, arguments, environment, and outputs.

Remote eligibility is fail-closed and auditable. Metadata-only compiler actions with fully mapped inputs and outputs can be marked eligible. Link actions are explicitly ineligible until native libraries, linker binaries, response files, generated linker arguments, and platform SDK inputs are completely represented. Identical real Cargo fixtures in different worktrees must produce the same action key in the integration suite.

The local shared-cache backend now implements the complete action lifecycle: content-addressed output blobs, per-action cross-process locks, atomic publication, digest verification, dep-info path rewriting, and output materialization into independent worktrees. Concurrent identical actions execute once; waiters restore the published result. Corrupt blobs are rejected and rebuilt locally. Failed actions and remotely ineligible link actions are never published.

The reclient transport adapter stages eligible actions into explicit input roots, invokes the production `rewrapper` client with declared inputs and outputs, and materializes successful outputs back into Cargo's target directory. Its platform template must bind `{os}`, `{arch}`, and `{toolchain_sha256}` so an action cannot silently execute against a mismatched worker toolchain. Real remote execution still requires an operator-provided reclient installation, a running `reproxy`, and a platform-matched REAPI service. The repository test suite exercises the complete adapter against a behaviorally faithful fake `rewrapper`; validation against a live service is the next infrastructure milestone.

The first bounded [five-worktree Moria acceptance](docs/moria-acceptance-2026-07-18.md) passed in 7m07s, with 63 shared-cache hits per fresh worktree. It also exposed the remaining hard boundary: nondeterministic local proc-macro links invalidate 22 downstream actions, so the 60-second fresh-worktree warm target is not yet met.

The public name is currently collision-free: a crates.io exact-name search returned no `cargo-reapi` package, and the only GitHub repository returned for the name was this project (checked 2026-07-18). That is not a crates.io reservation; publication must repeat the check.

## Usage

```sh
cargo install --path .
cargo reapi --backend capture -- test
cargo reapi --backend cache --cache-dir /shared/cargo-reapi-cache -- check
cargo reapi --backend reapi \
  --rewrapper /opt/reclient/rewrapper \
  --rewrapper-cfg /etc/cargo-reapi/rewrapper.cfg \
  --reclient-staging-dir /shared/cargo-reapi-stage \
  --reclient-platform 'OSFamily={os},Arch={arch},toolchain_sha256={toolchain_sha256}' \
  -- check
```

The default log is `target/cargo-reapi/actions.jsonl`. Cache mode deliberately requires an explicit cache directory so separate worktrees share only the operator-selected store. REAPI mode expects `reproxy` to be started and stopped through reclient's `bootstrap` lifecycle outside each individual Cargo action. To prove there is no semantic change, compare the exit status and artifacts with the same Cargo command without the wrapper.

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
- Quality-gate concurrency is still bounded outside this tool. Remote execution changes where work runs; it does not grant unbounded scheduling.

See [docs/architecture.md](docs/architecture.md) for the implementation boundary and milestones.

## Project policy

This is infrastructure built first for the Moria/Bro workload and shared in public as-is. Issues with reproducible action captures are welcome, but publication does not promise compatibility with every Cargo project, hosted workers, or support response times. Correctness and fail-closed behavior take priority over backend coverage.
