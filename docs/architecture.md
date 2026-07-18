# Architecture

## Why the integration point is `RUSTC_WRAPPER`

Cargo already computes the authoritative unit graph, resolves features, runs build scripts, builds proc macros for the host, and schedules compiler commands. Replacing that graph recreates Cargo semantics and makes correctness depend on synchronized Bazel or Buck metadata. A compiler wrapper sees the exact actions after Cargo has made those decisions.

## Action lifecycle

1. The `cargo reapi` driver starts Cargo with `cargo-reapi` as `RUSTC_WRAPPER`.
2. Cargo invokes the wrapper as `cargo-reapi <rustc> <rustc arguments...>`.
3. The wrapper classifies the action and discovers package sources, response files, explicit `--extern` artifacts, generated inputs, environment, toolchain identity, and expected outputs.
4. Capture mode executes `rustc` locally and writes an auditable action record.
5. REAPI mode gives a complete, path-normalized action to reclient's `rewrapper`; reclient constructs the Merkle input root, uploads missing blobs to CAS, queries the action cache, executes on a matching worker when necessary, and materializes verified outputs back into Cargo's target directory.
6. Cargo continues its normal schedule and executes build scripts, proc macros, tests, and binaries exactly as requested.

## Correctness boundary

Rust compiler actions are suitable for remote execution only when every file read by the action is present in the input root and the worker exposes an identical toolchain/platform contract. Package source discovery must include implicit Rust modules and macro inputs. Generated files under `OUT_DIR`, native libraries, linker inputs, response files, and proc-macro dependencies require explicit handling.

Build scripts are initially executed by Cargo on the coordinator. Their compilation can be remote, but their execution is not moved until filesystem and environment tracing can prove a complete sandbox. This preserves Cargo behavior while still offloading the dominant Rust compilation work.

## Milestones

1. Capture and replay audit: record real Cargo actions and prove local wrapper transparency.
2. Deterministic action model: normalize paths, predict outputs, identify toolchains, and reject incomplete inputs.
3. Reclient adapter: stage path-normalized action roots, generate explicit input/output lists, and use local execution with remote caching.
4. REAPI execution: run eligible `rustc` actions through reclient on platform-matched workers and materialize outputs.
5. Build-script sandboxing: trace and declare filesystem/environment effects before allowing remote execution.
6. Bro integration: per-project policy, telemetry, bounded admission, fallback behavior, and five-worktree acceptance.

## Acceptance

The production gate is the same Cargo command set used without the wrapper: format, check, clippy, and test. A backend is acceptable only when exit status and produced artifacts match, stale results cannot be accepted, peak coordinator memory stays within its configured cap, and five independent worktrees can progress under bounded admission.
