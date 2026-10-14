# cargo-reapi handoff: 0.2.11 cache only what can be proven

State as of 2026-10-15. The work is on `main` (see the commit that adds this
file). It is **not released**: the version is still 0.2.10, there is no tag,
nothing is published to crates.io and nothing is installed.

## Release rule

Jason's rule, effective for 0.2.11: Astra reviews the exact release commit
**before** `cargo publish`. The order is:

1. Fix on `main` and report the commit to the coordinator.
2. The coordinator runs Astra on that commit; fix what it finds and repeat.
3. Only when the coordinator says the review is clean: bump `Cargo.toml`,
   `Cargo.lock` and `CHANGELOG.md` (move `[Unreleased]` to `[0.2.11]`), tag
   `v0.2.11`, push, `cargo publish`, then
   `cargo install cargo-reapi --version 0.2.11 --locked`.
   Never install a branch or local build. Check that the clone has no
   untracked files before publishing: `cargo package` includes them.
4. Update the Moria skill (`.agents/skills/cargo-reapi/SKILL.md` on
   `moria-v2` master, last updated for 0.2.10 in `c04ed0a9`) to describe the
   declines below, so lanes know a declined invocation runs uncached.

## Direction: cache only what can be proven; everything else is plain Cargo

Astra's review of `558ecfd` (the first 0.2.11 candidate) found eight more holes
after three rounds of patching refusals, so the approach changed. The rule:
**cargo-reapi caches only what it can prove safe, and runs the user's exact
Cargo command uncached otherwise.** See README "What is cached, and what runs as
plain Cargo" and `CHANGELOG.md` `[Unreleased]`.

- A decline is never an error. `src/main.rs` (`prepare_cached_project`,
  `run_uncached`) prints `cargo-reapi: not caching (<reason>); running Cargo
  uncached` and runs the command: nothing restored, recorded or wrapped. A
  resolved project with a valid target keeps the strict sandbox (gate
  preparation failures); declines without a project, or because the target
  overlaps the project, run plain Cargo, because the sandbox policy assumes a
  disjoint target and nothing is cached there anyway.
- The checks apply only for `--backend cache` with the strict policy; local,
  capture and `--snapshot-policy off` never run `select`'s refusals.
- Only build/check/clippy/test/run/bench/doc (and `b c d r t`) are cached, and
  not `--help`/`--version`; this is decided before any metadata run
  (`Selection::is_cacheable`).
- Preflight never changes the project: `cargo metadata --locked` (or
  `--frozen`). Missing or outdated lockfile, metadata failure, an alternate
  lockfile (`CARGO_RESOLVER_LOCKFILE_PATH`, `resolver.lockfile-path`),
  `--ignore-rust-version`, a missing declared input and unreadable or
  `include`ing configuration all decline. Fixtures that run the gate need their
  lockfile first (`tests/common/mod.rs::ensure_lockfile`).
- Target directory: canonical path disjoint from the working directory, the
  selected workspace, every package root, source directory and declared input
  in both directions (`Project::validate_target`). The only exception is
  `<workspace root>/target`, Cargo's own default: `moria_cargo` uses
  `CARGO_TARGET_DIR=$checkout/target` and stays cached.
- Hashing has no exclusions by name (`excluded_from_tree`, and the same rule in
  `capture.rs` `is_ignored`): only the canonical target and the action log.
  `.git` is hashed except `objects`, `logs` and `info/refs` (also under `.git/modules/**`; `git repack` rewrites `info/refs`),
  `is_git_bulk`. Rationale in the README.
- A marker is `TargetMarker`: the snapshot key plus each restored file's
  identity (size, mtime, ctime, inode). A hit needs every identity to match;
  otherwise the snapshot is restored again. Restoration removes the snapshot's
  own paths first and fails if a symlinked directory is on the way
  (`clear_destinations`), so it writes nothing else.
- The coalescing signal is compiled only with the `test-hooks` feature
  (`tests/coalescing.rs` has `required-features`). Run the whole suite with
  `cargo test --features test-hooks`. The `/proc/locks` detector stays in
  `tests/maintenance.rs`. `CARGO_REAPI_ACTION_CACHE_TEST_MODE` (skips the gate)
  is still a runtime variable; it only reduces caching.
- Remaining known residuals: the snapshot holds the whole target as it stood at
  publication, not only what the gated run wrote; a target that other tools
  write concurrently while a gate runs is outside the proof; the identity check
  trusts ctime and inode, which a privileged process can forge.

Test environment on this workstation: set `TMPDIR` under `/mnt/build`,
`CARGO_REAPI_COW_TEST_DIR=/mnt/build/cow-test` (reflink XFS) and
`CARGO_REAPI_SRT` to the pinned runtime's `dist/cli.js`
(`~/.local/share/moria-sandbox-runtime/node_modules/.pnpm/@anthropic-ai+sandbox-runtime@0.0.66/node_modules/@anthropic-ai/sandbox-runtime/dist/cli.js`);
the `srt` in `node_modules/.bin` is a different package and is refused.

## Verdicts on Astra's 0.2.11 findings

Every regression drives real Cargo in `tests/project_selection.rs` and was
mutation-checked: disabling the fix makes it fail.

| # | Finding | Fix | Regression (mutation that makes it fail) |
|---|---------|-----|-------------------------------------------|
| 1 | Target in an `include_str!` data directory overwrote edits | Target disjoint from every project directory, both directions; only `<workspace root>/target` exempt | `a_target_directory_that_is_an_include_str_data_directory_never_overwrites_it` (drop the workspace, cwd, package-root and input checks) |
| 2 | Alternate lockfile neither hashed nor protected | Decline on `CARGO_RESOLVER_LOCKFILE_PATH` and `resolver.lockfile-path` | `an_alternate_lockfile_is_never_cached` (remove either check) |
| 3 | Source directory named `target` skipped by the key | No exclusion by name in the gate key or rustc input discovery | `a_source_directory_named_target_is_hashed` (re-add the name rule in `gate.rs`, or in `capture.rs`) |
| 4 | Marker alone accepted as a hit | Marker carries each output's identity; mismatch restores | `a_gate_hit_restores_outputs_that_another_cargo_run_replaced` (compare the key only) |
| 5 | `--ignore-rust-version` dropped by preflight | Decline | `ignoring_the_rust_version_is_never_cached` (remove the refusal) |
| 6 | Metadata ran for help/version/non-build commands | Decide from the command before any metadata run | `commands_that_do_not_build_are_never_prepared_or_cached` (treat every command as cacheable) |
| 7 | Offline build needed unrelated dev-dependency sources | Metadata failure declines to plain Cargo | `offline_builds_do_not_need_unrelated_dependency_sources` (make the failure an error) |
| 8 | Refusals were errors, also in local/capture | Declines run Cargo; selection runs only where caching would | `an_alias_works_without_caching_and_in_the_local_backend` (propagate `select`'s error in the local path) |
| 2' | Preflight could write `Cargo.lock` | `cargo metadata --locked` | `a_missing_or_outdated_lockfile_is_never_written_by_preparation` (stop adding `--locked`) |
| git | `.git` unconditionally skipped | Hash all but `objects`, `logs`, `info/refs` | `git_state_read_by_a_build_script_is_keyed_but_repacking_objects_is_not` (skip `.git` wholesale: stale hit; hash objects: no hit after `git repack`) |
| moria | Ordinary `moria_cargo` invocations stay cached | `<workspace root>/target` exemption | `the_ordinary_moria_cargo_invocations_still_hit` (ten invocations hit on their second run), `cargos_default_target_directory_is_cached` (drop the exemption) |

Decline reasons each have a stderr assertion (`assert_declined`): non-build
command, `--help`/`--version`, alias, `--config`/`-Z`/`--` option, include,
`--ignore-rust-version`, alternate lockfile (both forms), missing declared
input, missing or outdated lockfile, metadata failure, target overlap
(`.`, `src`, `src/out/new`, a symlink to `src`, a data directory, a path
dependency, a nested `--target-dir`), `-p` with features offline, `-qm`.
Unit tests cover `clear_destinations` (never writes through a symlink) and
`is_git_bulk`. A release build contains no `COALESCE_SIGNAL` string.

## Next steps

1. Report the commit range to the coordinator for Astra's review.
2. Fix review findings with mutation-checked real-Cargo regressions.
3. Release per the rule above and update the Moria skill so lanes know that
   declined invocations run uncached, with the reason on stderr, and that a
   cached build needs `Cargo.lock` to exist and the target at
   `<checkout>/target` or outside the checkout.
