# Compiler environment relocation audit

This audit records the path-bearing compiler environment covered by
`cargo-reapi` after the Moria integration-test dogfood failure on 2026-07-30.
It follows the environment categories documented by the
[Cargo Book](https://doc.rust-lang.org/cargo/reference/environment-variables.html)
and the custom values described in
[Cargo build scripts](https://doc.rust-lang.org/cargo/reference/build-scripts.html).

## Invariant

An action key may normalize a worktree-, package-, target-, or toolchain-root
path only if the bytes produced by that action are relocatable to the consumer
root. Otherwise two worktrees could share a key while a restored artifact still
contains the producer's absolute path.

Before executing rustc, cargo-reapi now scans every inherited environment value
for the declared package, workspace, target, and toolchain roots. Every
occurrence is replaced with the existing fixed-width execution slot. Artifact
publication converts execution slots to logical cache slots, and restoration
converts cache slots to the consumer's execution slots. Values outside those
declared roots remain unchanged and remain part of the action key, so a
different external path produces a miss rather than an unsafe hit.

Non-UTF-8 values cannot be consumed by Rust's `env!` macro. cargo-reapi still
relocates one when the entire value is a path under a declared root; otherwise
it remains unchanged and keyed.

## Audited surface

| Source | Path-bearing values | Result |
| --- | --- | --- |
| Cargo crate environment | `CARGO_MANIFEST_DIR`, `CARGO_MANIFEST_PATH`, `OUT_DIR`, `CARGO_TARGET_TMPDIR`, `CARGO_BIN_EXE_<name>` | Relocated generically; the dynamic binary prefix no longer requires a name inventory |
| Cargo/toolchain environment | `CARGO`, `RUSTC`, `RUSTDOC`, linker paths, and dynamic-library search paths | Declared toolchain/target/workspace root occurrences are relocated; external tool paths remain keyed as external values |
| `.cargo/config.toml [env]` | Arbitrary names, including `relative = true` values expanded by Cargo to absolute paths | Relocated by value rather than by variable name |
| Build-script `cargo::rustc-env` | Arbitrary names and values passed to rustc | Relocated by value rather than by variable name |
| Build-script metadata and unstable multiple-build-script output variables | `DEP_<links>_<key>`, `CARGO_DEP_<dep>_<key>`, and `<script>_OUT_DIR` may contain paths | Known-root occurrences are relocated without requiring prefix enumeration |
| Profile, feature, target, package, and cfg values | Primarily non-path values; package README/license fields and custom cfg strings may contain paths | Any known-root occurrence is relocated; all values remain keyed |
| External paths | SDKs, native dependencies, or operator paths outside declared roots | Not rewritten. Their literal value remains in the key, preventing cross-path hits |

The replacement algorithm reads only the original value. Overlapping roots are
matched longest-first and replacement text is never scanned again, so a package
root nested under a workspace root cannot be relocated twice.

## Regression evidence

`restored_action_relocates_integration_test_companion_binary_to_consumer_target`
creates producer and consumer worktrees with different path lengths. Its
fixture covers three independent path sources:

1. Cargo's dynamic `CARGO_BIN_EXE_companion`;
2. a config-relative custom variable from `.cargo/config.toml [env]`; and
3. an arbitrary absolute value emitted by `cargo::rustc-env` from `build.rs`.

The producer is deleted before the consumer runs. The test requires a physical
action-cache hit, executes the restored integration-test binary, launches the
restored companion binary, and canonicalizes both custom paths against the
consumer worktree. A producer path, a missing companion alias, or a cache miss
fails the test.

## Boundary

This closes compile-time environment relocation for the local shared-cache
backend used in production dogfooding. It does not promote the reclient adapter
to production-qualified status. Arbitrary customer-defined environment values
cannot safely be forwarded to a remote worker without distinguishing them from
ambient credentials; live REAPI validation and an explicit remote environment
declaration contract remain separate milestones.
