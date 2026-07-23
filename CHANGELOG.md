# Changelog

All notable changes to `cargo-reapi` are documented in this file.

## [Unreleased]

## [0.1.0] - 2026-07-23

Initial public release.

- Keeps Cargo authoritative by observing the compiler and linker actions Cargo
  schedules through `RUSTC_WRAPPER`.
- Adds verified cross-worktree action caching and exact whole-gate snapshots,
  including concurrent miss coalescing and content-addressed output storage.
- Keys declared inputs, toolchain identity, platform, arguments, working
  directory, relevant environment, native link inputs, and outputs.
- Restores artifacts into independent worktrees with fixed-width path
  relocation, digest verification, and macOS executable re-signing.
- Adds host-wide CPU and memory admission for physical work, cache inspection
  and garbage collection, environment diagnostics, and proof tooling.
- Adds a reclient transport adapter for eligible Remote Execution API actions.
  Validation against a live production REAPI service remains future work.
- Qualifies the local shared-cache path independently on macOS/arm64 APFS and
  Linux/x86_64 XFS with real Cargo, Bevy, and Moria workloads.

Known limitations and the precise qualified boundary are documented in the
[README](README.md#known-limitations-and-roadmap).

[Unreleased]: https://github.com/TamedTornado/cargo-reapi/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/TamedTornado/cargo-reapi/releases/tag/v0.1.0
