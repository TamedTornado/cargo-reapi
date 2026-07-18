# cargo-reapi

`cargo-reapi` is an experimental Cargo-native path to remote execution. Cargo remains the build planner and source of truth; the tool observes the exact `rustc` commands Cargo schedules through `RUSTC_WRAPPER`, captures their inputs, and will translate those actions to the Remote Execution API (REAPI).

The project exists because Bazel `rules_rust` and Buck2/Reindeer both require a second maintained build graph. That is a poor fit for arbitrary Cargo projects and is particularly costly around workspace feature selection, build scripts, proc macros, and Cargo-provided environment variables.

## Current status

The capture milestone works: Cargo runs normally, every compiler action still executes locally, and a JSON Lines action log records the compiler command, Cargo environment, package inputs, explicit `--extern` artifacts, output directory, and content digests.

The reclient adapter for CAS upload, action execution, and output materialization is the next milestone. Reusing the production `rewrapper`/`reproxy` implementation keeps this project focused on Cargo and Rust action discovery. `--backend reapi` fails closed until that adapter is implemented; it never silently falls back to an unverified remote result.

## Usage

```sh
cargo install --path .
cargo reapi --backend capture -- test
```

The default log is `target/cargo-reapi/actions.jsonl`. To prove there is no semantic change, compare the exit status and artifacts with the same Cargo command without the wrapper.

## Design constraints

- Cargo owns dependency resolution, features, build scripts, proc macros, profiles, and command ordering.
- The wrapper may distribute compiler actions; it must not regenerate Cargo's graph.
- Action keys include all declared inputs, the toolchain identity, compiler arguments, and relevant environment.
- Remote execution must match the host/target platform contract. Native test and proc-macro artifacts cannot cross OS or architecture boundaries.
- Missing inputs or unsupported actions fail closed or execute locally according to explicit policy.
- Quality-gate concurrency is still bounded outside this tool. Remote execution changes where work runs; it does not grant unbounded scheduling.

See [docs/architecture.md](docs/architecture.md) for the implementation boundary and milestones.
