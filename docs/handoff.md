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
- Hashing has no exclusions by name except `.git` (`excluded_from_tree`, and
  the same rule in `capture.rs` `is_ignored`). The strict sandbox denies reads
  of every `.git` and gitdir under the hashed trees (`hermetic::git_entries`,
  `expand_around`), so a build script gets no Git state under caching.
- Gate rule (README "When a gate hit may touch the target"): a snapshot is
  restored only into a clean target (absent, or only `cargo-reapi/`,
  `.rustc_info.json`, `CACHEDIR.TAG`), a producer publishes only from one, and
  a target the marker still matches exactly (`TargetMarker`: key plus the
  identity of every file, and no other file) is a hit with nothing to restore.
  Any other state skips the gate (`GateSnapshot.clean`), and Cargo runs with
  the per-action cache. Snapshots carry per-file digests (`outputs` in the
  manifest), verified before a restore; a failure evicts the snapshot.
- Per-action cache: the manifest (`ACTION_MANIFEST_VERSION` 2) lists
  `dep_inputs`, every file in rustc's own dep-info with its digest
  (`capture::read_dep_inputs`, `dep_inputs_match`); a hit needs them all.
- `run` is its `build` gate (`build_arguments`) followed by `run_with` the
  original arguments with no project, i.e. unsandboxed, per-action cache only.
- Sandbox provider failures are `SANDBOX_UNAVAILABLE` errors, not declines.
- `test-hooks` (Cargo.toml documents it as for the crate's own tests only)
  compiles in the coalescing signal and `CARGO_REAPI_ACTION_CACHE_TEST_MODE`.
  Test targets that need them have `required-features`; run the suite with
  `cargo test --features test-hooks`. The `/proc/locks` detector stays in
  `tests/maintenance.rs`.
- Remaining known residuals: a target that other tools write concurrently
  while a gate runs is outside the proof; the identity check trusts ctime and
  inode, which a privileged process can forge; rustc dep-info lists files, so
  a directory-walking proc macro reading unlisted files is not keyed (the
  sandbox limits what it can read to the workspace and package roots).

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
| 4 | Marker alone accepted as a hit | Marker carries every file's identity and the exact file set | `a_target_changed_by_another_cargo_run_is_left_to_cargo` |
| 5 | `--ignore-rust-version` dropped by preflight | Decline | `ignoring_the_rust_version_is_never_cached` (remove the refusal) |
| 6 | Metadata ran for help/version/non-build commands | Decide from the command before any metadata run | `commands_that_do_not_build_are_never_prepared_or_cached` (treat every command as cacheable) |
| 7 | Offline build needed unrelated dev-dependency sources | Metadata failure declines to plain Cargo | `offline_builds_do_not_need_unrelated_dependency_sources` (make the failure an error) |
| 8 | Refusals were errors, also in local/capture | Declines run Cargo; selection runs only where caching would | `an_alias_works_without_caching_and_in_the_local_backend` (propagate `select`'s error in the local path) |
| 2' | Preflight could write `Cargo.lock` | `cargo metadata --locked` | `a_missing_or_outdated_lockfile_is_never_written_by_preparation` (stop adding `--locked`) |
| git | `.git` unconditionally skipped | Superseded by the sandbox denying `.git` (0.2.12 #8) | `a_build_script_finds_no_git_state_under_caching` |
| moria | Ordinary `moria_cargo` invocations stay cached | `<workspace root>/target` exemption | `the_ordinary_moria_cargo_invocations_still_hit` (ten invocations restore in a clean second checkout), `cargos_default_target_directory_is_cached` (drop the exemption) |

### Review of f6d2540 (0.2.12 findings)

| # | Finding | Fix | Regression (mutation) |
|---|---------|-----|------------------------|
| 1 | Restore over an `include_str!` input in the default target | Restore only into clean targets; producers publish only from clean ones | `an_input_inside_cargos_default_target_is_never_restored_over_an_edit` (treat every target as clean) |
| 2 | Write through a symlinked `target/cargo-reapi` | Decline unless it is a real directory with no symlink inside | `a_symlinked_administrative_directory_is_declined_and_never_written_through`, directory and log cases (drop the check) |
| 3 | Legacy `--lockfile-path` with `[unstable]` | Unknown options decline; any `[unstable]` or `CARGO_UNSTABLE_*` declines | `unknown_options_and_unstable_configuration_run_as_plain_cargo` (accept unknown options; ignore `[unstable]`) |
| 4 | Included file outside the package not in the action key | dep-info inputs with digests per entry | `a_file_included_from_outside_the_package_keys_the_compile` (skip the comparison) |
| 5 | Whole-target snapshots rolled back other artifacts | Same clean-target rule | `a_restore_never_touches_artifacts_the_snapshot_did_not_build` (treat every target as clean) |
| 6 | Configured compiler overridden; custom rustdoc unhashed | Decline `build.rustc*`, wrappers and env forms; hash `RUSTDOC` | `a_configured_compiler_or_wrapper_is_declined_not_overridden`, `replacing_a_custom_rustdoc_in_place_misses` |
| 7 | `build.build-dir` outside the snapshot | Decline | `a_separate_build_directory_is_declined` |
| 8 | Git exclusion unsound | Sandbox denies `.git` and gitdir; not hashed | `a_build_script_finds_no_git_state_under_caching` (main clone and linked worktree; mutation lets the sandbox read it) |
| 9 | `run` skipped on a hit | `run` is its build gate plus a normal run | `run_caches_only_its_build_and_always_runs_the_program` (cache `run` whole) |
| 10 | Snapshot bytes unverified | Digests at publish, verified at restore, evict | `a_corrupted_snapshot_is_evicted_and_never_restored` (skip verification) |
| 11 | Provider failure announced as uncached | Own error, Cargo not run | `an_unavailable_sandbox_is_an_error_not_a_decline` (announce and fall back) |
| K | Test mode in release | `test-hooks` | release binary has no `ACTION_CACHE_TEST_MODE` or `COALESCE_SIGNAL` string |

### Review of 1097d82 (0.2.13 findings)

| # | Finding | Fix | Regression (mutation) |
|---|---------|-----|------------------------|
| 1 | A build script plants a link where the marker is written after the build | Driver writes in the target go through a handle with `openat`/`O_NOFOLLOW` (`beneath.rs`) | `a_link_planted_during_the_build_never_redirects_the_marker` (write with `fs::write`) |
| 2 | Snapshot verification skipped entries restoration copied | Verification walks every entry, whatever its name (`snapshot_files`) | `a_snapshot_entry_that_was_never_verified_is_never_restored` (verify with the excluding walk) |
| 3 | Fingerprint invalidation removed any `.fingerprint` directory | Only `<target>/<profile>/.fingerprint` and `<target>/<triple>/<profile>/.fingerprint` of directories holding `.cargo-lock` | `fingerprint_invalidation_removes_only_cargos_own_directories` (recursive removal) |
| 4 | Inputs under `target/cargo-reapi/` gave stale gate hits | `cargo-reapi/` counts as content for cleanliness; a marker hit needs it to hold only cargo-reapi's own entry names, no link | `an_input_beneath_the_administrative_directory_is_never_a_stale_hit` (exempt it and trust it) |
| 5 | Dep-info paths collapsed `..` lexically; backslashes dropped | Resolve with `canonicalize`, record and re-resolve the raw path; parse escaped spaces only | `dep_info_inputs_are_checked_as_rustc_read_them`, each input changed alone (lexical paths; backslash escape in both parsers) |
| 6 | Git dependencies expose Git state | **Not denied: Cargo opens a checkout's `.git` under the sandbox and re-clones, failing, when it cannot.** The gate key hashes the whole repository state of each Git dependency's checkout, `objects` by file names and sizes | `a_git_dependencys_repository_state_is_keyed` (drop the hashing) |
| 7 | Proc macros reading files outside dep-info | Held for Jason | |
| 8 | `run` built every binary | Gate `build --bin <name>` for the binary `run` builds (`--bin`, `--example`, `-p`, `default-run`, only binary); decline when it cannot be told | `run_builds_only_the_binary_cargo_run_would` (name no binary) |
| 9 | Failed gate preparation ran the program twice | The fallback runs the build arguments | `a_failed_gate_preparation_still_runs_the_program_once` (rerun the original arguments) |
| 10 | Schema-1 entries aborted builds | Version read first, other versions are misses; keys namespaced | `an_action_entry_of_an_older_schema_is_a_miss_not_an_error` (deserialize directly) |
| 11 | `doc --open` skipped the viewer on a hit | Declined | `doc_open_is_never_cached_and_opens_every_time` (cache it) |
| 12 | Audit variables reached the compiler | Passed only while an observer is the compiler, and the shim removes them before the real rustc; never counted as withheld | `an_audit_variable_never_reaches_the_compiler` (pass them) |

### Review of 89aecd3 (0.2.14 findings)

| # | Finding | Fix | Regression (mutation) |
|---|---------|-----|------------------------|
| 1 | A build plants a link in the cache's `access/gates` for the driver to write through | The sandbox policy denies writes to `gate-snapshots` and `access/gates` (`driver_only_cache_paths`) | `a_build_cannot_write_where_the_driver_records_after_it` (no deny) |
| 2 | Action log hard-linked to a source file | Append only to a regular file with `st_nlink == 1`, else replace the name | `a_hard_linked_action_log_is_replaced_never_appended_to`, unit test in `beneath.rs` (skip the check) |
| 3 | Directory-only changes to declared inputs | Directories keyed by name in `hash_tree` and in `observed_input_digest` | `a_new_empty_subdirectory_of_a_declared_directory_rebuilds`, unit test (drop both) |
| 4 | Input in `hermetic-tmp` invisible to marker hits | **Fallback taken:** `hermetic-tmp` stays in the target (its stable path is part of compile keys); administrative inputs listed by dep-info are recorded with digests and checked on marker hits | `a_file_the_build_keeps_in_its_temporary_directory_is_checked_on_a_hit` (skip the check) |
| 5 | Git `objects` metadata keyed by name | `objects/info` and `.idx`/`.keep`/`.rev` by content; alternates decline | `a_git_dependency_with_alternates_is_declined`, unit test `git_object_metadata_is_hashed_by_content_and_objects_by_name` |
| 6 | `run` ignored `default-members` | `-p`, else default members at the root, else the member | `run_follows_the_workspaces_default_members` (root package only) |
| 7 | Audit variables reached an ordinary compiler when `REAL_RUSTC` was set | Only a compiler with the shim's marker line gets them; every other has them removed | `audit_variables_never_reach_an_ordinary_compiler_even_with_a_real_rustc_set` (assume the shim) |
| 8 | Named run targets accepted unchecked | Glob, unknown, repeated and non-executable selections decline before any build | `run_selections_cargo_would_reject_are_declined_before_any_build` (skip validation) |

### Review of 6d49afd (0.2.15 findings): fixing classes

Two rules replace the per-sink fixes.

**One rule for gate inputs** (findings 1, 2). At publish the manifest's
`observed_inputs` records the digest of every input any action observed outside
the hashed trees (`collect_observed_inputs`): build-script `rerun-if-changed`
paths and variables (relative paths resolved against every package of the name
that declared them); every file the compiles' dependency files list that is
neither in a hashed tree, nor in an immutable registry or git source, nor in
the target's identity check (files beneath `cargo-reapi/` are recorded); and
the native inputs and linker the action cache discovered for links, which the
compiler wrapper records in `cargo-reapi/gate-inputs.jsonl`
(`record_link_inputs`). Every hit, marker or restore, re-verifies them all by
content; an input that cannot be verified is a miss. The old
`administrative_inputs` special case is gone.

**One write path** (findings 3, 4, 9). `src/safe_io.rs` and `src/beneath.rs`:
no link is followed; a file is modified in place only as a locked append to a
regular file with one link; anything else is replaced by a new file renamed
over its name; lock files are opened with `O_NOFOLLOW` and only locked.
Audit of every write cargo-reapi makes into storage a build can write:

| Write | Process | Path |
|-------|---------|------|
| gate marker | driver | `Beneath::write_file` (unlink, `O_EXCL`) |
| action log, hit record | driver | `Beneath::append_line` (single link, else replaced) |
| action log, compile records | wrapper (sandbox) | `safe_io::append_line`; the driver replaces a multiply linked or non-regular log before the sandbox binds it (`create_file_and_canonicalize`) |
| `gate-inputs.jsonl` | wrapper, driver removes | `safe_io::append_line`, `Beneath::remove_file` |
| srt policy | driver | `safe_io::write_replacing` |
| output and path-mapping records | wrapper | `create_new`, or `write_atomic` (temp file and rename) |
| cache locks (maintenance, action, gate, query, resource tokens) | both | `safe_io::open_lock` (`O_NOFOLLOW`, locked only, never written) |
| progress marker, access records | both | `safe_io::write_replacing` |
| cache entries, restored outputs | wrapper | temp file and rename (`write_atomic`, `NamedTempFile`) |
| `gate-snapshots`, `access/gates` | driver | denied to the sandbox's writes |
| fingerprint and marker removal | driver | `Beneath::remove_file`, Cargo's own directories only |

| # | Finding | Fix | Regression (mutation) |
|---|---------|-----|------------------------|
| 1 | Declared directory in `hermetic-tmp` | The one input rule above | `a_declared_directory_in_the_temporary_directory_is_checked_on_a_hit`; unit `a_relative_rerun_path_is_resolved_against_the_package_that_declared_it` (no relative resolution) |
| 2 | Whole-gate hit ignored native inputs | Link inputs recorded and verified | `a_native_library_replaced_between_runs_is_never_restored_stale` (no `record_link_inputs`) |
| 3 | Compile-action log hard-linked | `safe_io::append_line` and the pre-sandbox replacement | `hard_linked_log_and_policy_files_never_reach_the_lockfile_on_a_miss` (no replacement) |
| 4 | Policy file hard-linked | `write_replacing` | same test, policy case (`fs::write`) |
| 5 | Unframed file contents in tree hashing | Contents hashed apart and framed | unit `tree_hashing_frames_file_contents` (stream the bytes) |
| 6 | `multi-pack-index` treated as content-addressed | Allowlist: loose objects `xx/{38 hex}` and `pack/pack-{hex}.pack` only | unit `git_object_metadata_is_hashed_by_content_and_objects_by_name` (open the allowlist) |
| 7 | `run --all-targets` built | `run` rejects the build-only options | `run_with_options_or_arguments_cargo_would_reject_is_declined_before_any_build` (skip the check) |
| 8 | `run hello` became `build hello` | Positional arguments decline `run` | same test (skip the check) |
| 9 | Maintenance lock followed a link | `open_lock` | unit `a_lock_is_never_opened_through_a_link` (follow links); `a_planted_lock_link_never_creates_a_source_file` |
| 10 | Package ids declined | Matched by id as well as name | `run_accepts_cargos_own_package_id` (name only) |

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
