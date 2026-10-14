# cargo-reapi handoff: 0.2.11 fail-closed selection

State as of 2026-10-14. The work is on `main` (see the commit that adds this
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
   refusals below, so lanes know to spell out aliases and avoid `--config`
   and `-Z`.

## Direction: fail closed, do not emulate Cargo

Astra's review of v0.2.9..v0.2.10 found 13 findings, 7 high. Findings 3–8 and
12 all came from emulating Cargo's alias expansion, configuration precedence,
array merging and option scanning; every fix opened another divergence. The
coordinator's direction: stop reimplementing Cargo's CLI and configuration
semantics and refuse whatever cannot be read exactly. A refusal must never
break the ordinary `moria_cargo` invocations (build, check, clippy, test, run
with `-p`, `--profile`, `-q`, `--test`, `--no-run`, `--all-targets`,
`--workspace`, `--locked`, `--bin`, `--all-features`,
`--no-default-features` and `--` arguments).

What 0.2.11 does (see `src/selection.rs`, `src/project.rs`, README
"Strict snapshot execution" and `CHANGELOG.md` `[Unreleased]`):

- User-defined aliases are refused, from `[alias]` in any Cargo configuration
  file (both `.cargo/config` and `.cargo/config.toml`, every ancestor and
  Cargo's home) or `CARGO_ALIAS_<NAME>`. Built-in commands need no lookup; a
  non-built-in command (`b`, `clippy`, ...) is refused when a configuration
  file cannot be read completely (`include`, finding 13, documented).
- `--config` is refused except `KEY=VALUE` for `profile.*`, `term.*`,
  `build.jobs`; `-Z`, `-C`, `--` before the subcommand, unknown global
  options and short clusters holding a value option (`-qm`) are refused.
- Plain documented forms are read: `--manifest-path`/`-m`, `--locked`,
  `--frozen`, `--offline` (deduplicated), `--features`/`-F` including empty
  `-F=`, `--all-features`, `--no-default-features`, `-p`/`--workspace`/
  `--exclude`, `--target-dir`.
- Features without a package selection reach `cargo metadata` exactly. With
  a package selection metadata cannot apply them, so it resolves every
  feature, and that combination with `--offline`/`--frozen` is refused.
- In the strict sandbox the driver runs Cargo's **full** metadata once
  (`Project::resolve`), which also acquires sources. The gate, the sandbox
  policy and the target check all use it; their separate metadata runs are
  gone.
- The target directory is canonicalized (deepest existing ancestor for a new
  one) and refused when it is or contains the working directory, selected
  workspace, any package in the resolve, any directory holding target
  sources or a declared input, or lies inside a source directory or input.
- The gate hashes the selected workspace root and every local package by its
  canonical directory.
- An empty `CARGO_HOME` falls back to `$HOME/.cargo` everywhere.

## Verdicts on Astra's 0.2.10 findings

Every regression drives real Cargo (`tests/project_selection.rs` unless
noted) and was mutation-checked: disabling the fix makes it fail.

| # | Finding | Verdict | Regression |
|---|---------|---------|------------|
| 1 | Target in `src` / outside path dependency overwrote source | Fixed: full metadata, canonical paths, source directories | `a_target_directory_at_inside_or_linked_to_a_source_directory_is_refused` (config and `--target-dir`, `src`, `src/out/new`, symlink), `a_path_dependency_outside_the_workspace_is_never_the_target` |
| 2 | Symlinked package counted as hashed | Fixed: canonical package roots; selected workspace root hashed too | `a_path_dependency_reached_through_a_symlink_is_hashed_by_its_real_directory`, `a_package_selected_through_a_symlink_is_hashed_by_its_real_directory` |
| 3 | `--config` used during alias expansion | Fixed by refusal | `options_that_could_change_the_selection_unseen_are_refused` |
| 4 | `.cargo/config` beside `config.toml` | Fixed by refusal (either file) | `user_aliases_are_refused_wherever_they_are_defined` |
| 5 | Array aliases merged across layers | Fixed by refusal (CARGO_HOME included) | same |
| 6 | Expansion stops at global options; `cargo -- ci`; `-vZ` | Fixed by refusal | same, plus unit tests in `selection.rs` |
| 7 | `-F=` consumed the next option | Fixed | `an_explicitly_empty_feature_value_keeps_the_lockfile_policy` |
| 8 | Repeated `--locked` rejected by metadata | Fixed: deduplicated | `a_repeated_lockfile_flag_reaches_metadata_once` |
| 9 | Sibling virtual workspace unreadable | Fixed, and its manifest is hashed | `a_sibling_virtual_workspace_is_readable_and_its_manifest_is_hashed` |
| 10 | `--all-features` broke offline subset build | Fixed: exact features without `-p`; refused offline with `-p` | `an_offline_feature_build_acquires_only_what_its_features_need` (local file registry) |
| 11 | New nested target directory failed | Fixed | `a_new_nested_target_directory_is_created` |
| 12 | Empty `CARGO_HOME` | Fixed | `user_aliases_are_refused_wherever_they_are_defined` |
| 13 | `include` refuses `b` | Kept fail closed, documented | `configuration_that_includes_more_is_refused_only_where_an_alias_could_hide` |
| flaky | `tests/maintenance.rs` 60 s deadline | Replaced by lock-cycle detection from `/proc/locks` plus process tree; detector self-tested | `the_lock_cycle_check_sees_processes_waiting_on_each_other` |
| flaky | `tests/capture.rs` 1.5 s hold | Replaced by a coalesce signal (`CARGO_REAPI_ACTION_CACHE_TEST_COALESCE_SIGNAL`): the producer holds its lock until the coalescer says it is waiting | `concurrent_identical_actions_execute_once_and_coalesce_on_the_lock` |

Notes for the reviewer:

- Mutating only the package-root canonicalization does not fail the
  `--manifest-path link/Cargo.toml` test, because the selected workspace root
  (canonical) is also hashed; the symlinked path-dependency test covers it.
- Known residual: a directory inside a package root that no target's
  `src_path` names (an `include_str!` data directory, a `#[path]` module
  elsewhere) is not known to Cargo's metadata, so a target configured there is
  not refused. The sandbox denies reads outside known inputs, which limits
  this, but it is not closed.
- A smoke run of the branch binary on `/mnt/build/moria-v2` (`fetch --locked`,
  private cache and target) passed, with a gate hit on the second run.

## Next steps

1. Report the commit to the coordinator for Astra's review.
2. Fix review findings with mutation-checked real-Cargo regressions.
3. Release per the rule above and update the Moria skill.
