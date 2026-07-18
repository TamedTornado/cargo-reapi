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

1. **Complete:** Capture and replay audit: record real Cargo actions and prove local wrapper transparency.
2. **Complete:** Deterministic action model: normalize paths, predict outputs, identify toolchains, and reject incomplete inputs. Real-Cargo tests prove identical worktrees share an action key and links fail closed.
3. **In progress:** The local CAS substrate is complete: path-normalized action keys, content-addressed blobs, single-flight locking, atomic publication, verified restore, and dep-info relocation work across independent Cargo worktrees. The remaining work is the reclient transport: stage explicit action roots and execute eligible actions through `rewrapper`/`reproxy`.
4. REAPI execution: run eligible `rustc` actions through reclient on platform-matched workers and materialize outputs.
5. Build-script sandboxing: trace and declare filesystem/environment effects before allowing remote execution.
6. Bro integration: per-project policy, telemetry, bounded admission, fallback behavior, and five-worktree acceptance.

## Schedule guardrails

Backend proofs and milestones have clocks as well as correctness gates. A proof that spends its time reconstructing Cargo ecosystem behavior has already produced the maintainability answer; it does not earn an open-ended extension.

- Bazel `rules_rust`: at most one agent-day or four elapsed hours for a representative Moria target, whichever comes first.
- Buck2/Reindeer: at most half an agent-day or two elapsed hours after dependency import, whichever comes first.
- Existing Cargo-native wrappers: at most one agent-day to demonstrate native macOS, platform-matched artifacts and the full Cargo gate.
- `cargo-reapi` milestones: each milestone is split or stopped after five agent-days without a reviewable, tested artifact.

The operator is pre-authorized to skip to the Cargo-authoritative implementation when a proof stalls on second-graph maintenance, build-script fixups, native platform gaps, or other ecosystem friction rather than measured execution performance. The completed Bazel and Buck2 evaluations exercised that authority.

## Linker correctness

Linked outputs are the critical compatibility surface and are not treated as an incidental `rustc` output. Link actions must key every object, native library, response file, linker/toolchain binary, relevant environment value, generated build-script output, and platform SDK input. Compatibility fixtures must cover embedded absolute paths, debug information, build-script-generated linker arguments, proc macros, native dependencies, and stale-output rejection before remote linker results can be accepted.

Performance reports expose two measurements:

1. the authoritative full warm Cargo gate, including final links and runnable test artifacts;
2. a diagnostic warm measurement excluding final links, used to separate compiler-action reuse from linker work.

The diagnostic measurement cannot satisfy the production acceptance gate. It keeps a linker limitation visible and measurable instead of making all backend progress appear to be zero.

## Acceptance

The production gate is the same Cargo command set used without the wrapper: format, check, clippy, and test. A backend is acceptable only when exit status and produced artifacts match, stale results cannot be accepted, peak coordinator memory stays within its configured cap, and five independent worktrees can progress under bounded admission. A 60-second identical warm-gate target and a 15-minute five-worktree warm target remain performance goals; failure to cache final links must be reported explicitly and cannot be hidden by the compiler-only diagnostic.
