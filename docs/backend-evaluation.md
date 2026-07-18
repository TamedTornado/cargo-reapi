# Backend evaluation

The initial production case is Moria, a native Apple Silicon Rust/Bevy workspace. Cargo's format, check, clippy, and test commands are the authoritative quality gate.

## Bazel `rules_rust`

A disposable Moria worktree was imported with `crate_universe` and built with Bazel 8.4.2 and `rules_rust` 0.71.3. Bazel analyzed 19,150 targets and ran 1,216 actions. After manually declaring `Cargo.toml` as compiler data, `moria-world` built and then rebuilt from cache in 2.59 seconds.

The proof failed the adoption gate:

- first-party crates still require maintained BUILD targets and Cargo inputs;
- importing the workspace unified features across demo and benchmark members, so a small `moria-world` library build compiled the full Bevy rendering/PBR graph;
- the successful build therefore did not preserve the same selected Cargo unit graph as the quality gate.

## Buck2 and Reindeer

The official Buck2 binary and Reindeer were run against the same Moria revision. Reindeer generated an 10,993-line BUCK file containing 403 Rust rules. It did not generate Moria's first-party targets from the virtual workspace, and it reported 54 crates whose build scripts require manual `fixups.toml` decisions. Those include WGPU, graphics/platform crates, compression libraries, and common procedural-macro dependencies.

Buck2 could parse the generated third-party targets, but a correct Moria build would require a second checked-in first-party graph plus manual declarations for build-script behavior. Cargo would no longer execute at build time.

## `cargo-green`

`cargo-green` is the closest existing Cargo-authoritative design: it uses `RUSTC_WRAPPER` and BuildKit for cached or remote compiler execution. Its documented minimum platform is Ubuntu, its remote mode compiles tests remotely but runs them locally, and remote test execution remains a TODO. That cannot produce or execute Moria's native macOS/Apple Silicon test and acceptance artifacts with the current worker model.

Its wrapper architecture is useful prior art, but it is not a deployable backend for this case.

## Fuchsia `rustc_remote_wrapper`

Fuchsia ships a production Rust wrapper around Google's reclient. It validates the wrapper approach and contains mature input/output discovery. It is coupled to Fuchsia's GN-generated source/dependency manifests and toolchain layout. Its own eligibility logic forces local execution when the macOS SDK lies outside the execution root and when a `.dylib` proc macro is present—the common native-Mac shape Moria needs.

The implementation is valuable prior art for output prediction, depfile handling, linker inputs, and fail-closed classification. It is not a drop-in Cargo backend, but `cargo-reapi` should port proven generic logic rather than inventing it again.

## Decision

Keep Cargo as planner and intercept the exact compiler actions it schedules. Implement REAPI action caching and execution behind `RUSTC_WRAPPER`, with an explicit worker platform/toolchain contract and fail-closed eligibility. This avoids maintaining a second dependency graph and permits native macOS workers when artifacts must run on macOS.
