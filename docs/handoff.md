# cargo-reapi handoff: per-compile caching (read this first)

State as of 2026-10-15. The narrowing is finished and merged to `main`. The
native-rule narrowing and scheduling-key fix are merged to `main` at 33892a4
(see below); the coordinator launched the pre-release review of 33892a4.
None of it is **released**: the version is still 0.2.10, there is no tag, nothing is
published and nothing is installed. The whole-gate design and its review
history are in Git (`main` at 86d3e1c and earlier, including the previous
version of this file).

## Jason's decision (via the coordinator)

Per-compile rustc caching only, the sccache model: a compile is keyed by
compiler identity, arguments, keyed environment and every dep-info input,
verified by content. Cargo runs every time. Link steps and compiles with
native link inputs outside the project and toolchain run uncached. Build code
written to defeat the cache, and proc macros that read files they do not
declare, are out of scope as documented README assumptions. Release bar: no
ORDINARY findings in the pre-release review.

## What the narrowing did

Commits on `main` after 86d3e1c (from the `narrow-0216` branch):

- Removed the whole-gate restore, the sandbox, the metadata preflight,
  native-link discovery, the acceptance harness (`acceptance/`, `proof.rs`,
  `evidence.rs`, the auditor binaries, `contract`/`prove`), the rustc
  observer and Mach-O re-signing (only restored executables needed it, and
  link steps are never restored).
- Native inputs (`capture.rs`, `native_inputs`): link steps are ineligible.
  Each `-L` directory is canonicalized; one inside the workspace, target
  directory or package has every file directly in it keyed; a toolchain
  directory is allowed for lookup only; any other directory makes the compile
  ineligible. Each `-l` must be found in an inside or toolchain directory.
- A compiler wrapper configured through `--config` declines, like one in
  configuration files or the environment.
- Integration tests triaged: gate, sandbox and observer tests deleted; the
  rest count compiles from the action log. Code under test lives in
  libraries because binaries are link steps.
- README and CHANGELOG `[Unreleased]` rewritten: what is cached, what runs
  fresh, the two assumptions. `docs/` and `benchmarks/` are historical.

## Tests

`cargo test --all-targets --features test-hooks`, fmt and clippy
`-D warnings` clean: 108 tests (unit 60, action_cache 6, capture 22,
coalescing 1, dependency_relocation 1, environment 9, maintenance 4,
native_libraries 3, pipelining 1, reinstall 1). macOS was not compiled locally
(only the Linux target is installed); CI covers it.

Regressions the brief asked for:
`an_outside_static_library_replaced_makes_the_link_step_run_fresh`,
`a_library_bundling_an_outside_static_archive_runs_uncached`,
`a_static_archive_bundled_from_inside_the_workspace_keys_the_compile`
(tests/native_libraries.rs);
`a_file_included_from_outside_the_package_keys_the_compile`,
`dep_info_inputs_are_checked_as_rustc_read_them`,
`an_action_entry_of_an_older_schema_is_a_miss_not_an_error`
(tests/action_cache.rs).

Mutation checks (each reverted): making link steps eligible, ignoring outside
`-L`, not hashing inside `-L` directories, accepting any dep-info input,
skipping the schema-version check, ignoring `--config` wrappers and never
finding inside `-l` libraries each fail a named test. Re-adding the old skip
of directories named `target` fails `a_source_directory_named_target_is_hashed`
only together with disabling the dep-info check: the package walk and the
dep-info check each catch it alone.

## Environment keying

The UI lane saw OLDPWD keyed. The current rule (`CompileEnvironment::for_compile`)
is already an allowlist: toolchain families (`CARGO*`, `RUST*`, `LD_*`, `CC`,
`PATH`, `HOME`, locale and similar), variables Cargo set for the compile,
Cargo `[env]`, build-script `rustc-env`/`rerun-if-env-changed`, and names
learned from `env!`/`option_env!` dep-info. The whole environment is keyed
only when the compile's variables cannot be determined, which in Moria
happens only for Cargo's uncached probe compiles. No cacheable Moria compile
keyed OLDPWD; `shell_history_and_unrelated_variables_do_not_change_action_keys`
covers it. The observation came from the installed 0.2.10, whose whole-gate
key hashed the entire environment. No change made.

## Moria measurement (warm cache, clean checkout)

Scratch release build of `narrow-0216` with Moria's environment and a separate
cache, moria-v2 master 847db36a, `nice -n 5` through `build-slot.sh` on a
loaded machine (load 15-30 warm, about 79 cold), so walls are noisy.

| invocation | cold wall | warm wall | warm hits | warm misses | proc-macro links | other links | native-uncached |
|---|---|---|---|---|---|---|---|
| build -p moria-showcase | 155.5 s | 55.5 s | 496 | 5 | 42 | 75 | 42 |
| check --workspace --all-targets | 116.1 s | 37.9 s | 520 | 6 | 38 | 14 | 56 |
| clippy --workspace --all-targets -- -D warnings | 38.9 s | 4.7 s | 44 | 0 | 0 | 1 | 21 |
| test --workspace --no-run | 142.5 s | 58.2 s | 31 | 0 | 0 | 47 | 6 |
| build -p moria-showcase --release | 327.3 s | 98.0 s | 515 | 5 | 42 | 75 | 42 |

"Other links" are mostly build-script compiles. Cargo timings of a warm
`build -p moria-showcase` (55.4 s wall), summed unit time: native-uncached
compiles 78 s (38 units), proc macros 54 s (42), cache hits 60 s (489),
misses 17 s (7), build-script compiles 25 s and runs 14 s (72 each), binary
link 10 s.

Why compiles run fresh:

- **Native-uncached:** pkg-config `-sys` crates emit
  `-L native=/usr/lib/x86_64-linux-gnu`, which Cargo passes to every
  dependent, so winit, wayland, smithay, parley and Moria's own `moria_ui`,
  `moria_vfx`, `moria_sfx` and `moria_showcase` libraries run uncached; so do
  `-l dl` and `-l pipewire-0.3` crates (not found inside).
- **Misses:** num_enum, libspa, pipewire, boa_wintertc, boa_runtime. A freshly
  linked proc-macro `.so` is not byte-identical across checkouts, so its
  dependents' `--extern` digests differ: the proc-macro cost cascades.

## Native-rule narrowing and scheduling keys (branch `native-narrowing`)

Approved by the coordinator; commits e737dbb..native-narrowing (6f5e6fd,
b0dcaa0, 3418cca, 89325b1, 33892a4). Fast-forwarded to `main` with the
coordinator's go-ahead. No bump, tag, publish or install came with it.

- `capture::native_inputs`: a metadata-only compile ignores `-L` and `-l`
  (and keys no `-L` directory files). An rlib compile ignores `-l` kinds it
  does not bundle (`bundles()`: only `static` without `-bundle` counts); a
  bundled `-l` must be found in an inside or toolchain directory. An outside
  `-L` directory sets `check_bundled_members` instead of making the compile
  ineligible. Link steps are unchanged (always uncached).
- `capture::bundles_nothing_unkeyed` (called in `cache::finish_cached_execution`
  before `publish`): with `check_bundled_members`, every `.rlib` output must
  hold only `lib.rmeta`, `lib.rmeta-link` (an object rustc 1.97 writes that
  lists the crate's own objects; 1.93 does not) and `<crate>-*.rcgu.o` /
  `<crate>.*.rcgu.o` members
  (`src/rlib.rs`, read with the `ar` crate, GNU and BSD formats); otherwise the
  compile is logged `local-ineligible` (with no reason text, since the reason
  is found after the compile) and nothing is stored. The check is needed
  beside the command-line rule because `#[link(kind = "static")]` bundles an
  archive that never appears as `-l`. Such compiles are also remote-ineligible.
- `environment::is_scheduling_environment`: `CARGO_MAKEFLAGS`,
  `CARGO_BUILD_JOBS` and `CARGO_REAPI_*` still reach rustc (it joins the
  jobserver) but are not keyed. `cache::publish` refuses to store a compile
  whose dep-info tracks one of them (`env!`), so the key stays sound.
- Known and out of scope (hostile build code): outside `-L crate=`,
  `-L dependency=` and `-L all` directories could in principle serve an
  `extern crate` of a sysroot name; this was already the case before.

Regressions (each shown failing under a targeted mutation, then reverted):
`a_check_with_a_propagated_outside_search_directory_is_a_hit_the_second_time`
(metadata rule removed),
`a_library_searching_an_outside_directory_without_bundling_is_cached`
(every `-l` treated as bundled),
`a_library_bundling_an_outside_static_archive_runs_uncached` (the `#[link]`
case; member check disabled),
`a_static_library_named_by_link_lib_inside_the_workspace_keys_the_compile`
(inside directory files not keyed),
`a_library_naming_an_outside_static_archive_by_link_lib_runs_uncached`
(tests/native_libraries.rs);
`job_scheduling_and_wrapper_settings_do_not_change_action_keys` (scheduling
names keyed again),
`a_compile_that_reads_an_unkeyed_scheduling_variable_is_not_stored` (publish
refusal removed) (tests/environment.rs); unit test
`only_the_metadata_and_the_crates_own_objects_are_its_own` (src/rlib.rs).
Suite: 118 tests pass with `--features test-hooks`; fmt and clippy
`-D warnings` clean. The local default toolchain is 1.93, which writes no
`lib.rmeta-link`; the native and environment tests also pass under
`cargo +1.97.1`, and there rejecting `lib.rmeta-link` fails
`a_library_searching_an_outside_directory_without_bundling_is_cached`. The
first Moria measurement found that defect (34 rlibs refused).

### Moria measurement after the narrowing

Same method as above, but run for both binaries in the same session:
scratch release builds of `main` e737dbb (before) and `native-narrowing`
(after), Moria 847db36a, Moria's environment with a separate cache per
binary, `nice -n 5` through `build-slot.sh`; each invocation in its own fresh
checkout, cold then warm. Counts are action-log rows; "probe" rows are
Cargo's and build scripts' uncached probe compiles (no declared outputs),
22 in each run. Load 9 to 20, so walls are noisy.

| warm invocation | wall | hits | misses | proc-macro links | other links | native-uncached | probes |
|---|---|---|---|---|---|---|---|
| check --workspace --all-targets, before | 48.1 s | 567 | 6 | 42 | 77 | 56 | 22 |
| check --workspace --all-targets, after | 20.8 s | 606 | 23 | 42 | 77 | 0 | 22 |
| build -p moria-showcase, before | 64.3 s | 496 | 5 | 42 | 75 | 37 | 22 |
| build -p moria-showcase, after | 52.4 s | 527 | 11 | 42 | 75 | 0 | 22 |

Cold walls: check 84.3 s before, 84.3 s after; showcase 138.7 s before,
126.8 s after.

The remaining misses come from compiles whose inputs differ between
checkouts, plus everything that depends on them, which is now cacheable and
so shows up as a miss rather than as uncached. Proc-macro `.so` files are
relinked differently in each checkout (num_enum, boa_wintertc, boa_runtime and
their dependents). `libspa-sys` has cc-built `static_fns.o` and
`liblibspa-rs-reexports.a` in `OUT_DIR` whose bytes depend on the checkout
path, so libspa_sys, libspa, pipewire_sys, pipewire, cpal and moria_sfx miss.
Each of these is keyed correctly; the inputs really do differ.

## Deferred (not approved): proc-macro and dylib caching

Record the files the linker opens (`-Wl,--trace`, supported by GNU ld, lld
and mold) when the action runs, store them with the entry as link inputs,
and verify their digests on lookup like dep-info inputs, with the linker
identity keyed. Restrict to proc-macro and dylib crate types whose traced
outside inputs are the C toolchain's default libraries (crt objects, libc,
libgcc, libm). Restoring the same `.so` also removes the miss cascade.
Measured cost of not doing it, per warm `build -p moria-showcase`: 42
proc-macro link units, 54 s of summed unit time, plus the 5 to 6 dependent
misses (a relinked proc-macro `.so` differs across checkouts, so its
dependents' `--extern` digests differ).

## Release follow-ons (not this lane's)

- Before a release, run the native tests under the newest stable toolchain
  (`cargo +<newest> test --features test-hooks --test native_libraries`) as
  well as the default: rustc's rlib layout changes show up there first.
- Release rule: Astra reviews the exact release commit before the bump; fix
  findings and repeat; then bump `Cargo.toml`, `Cargo.lock` and the CHANGELOG,
  tag, push, `cargo publish`, `cargo install --version <v> --locked`. Never
  install a branch build. Check the clone has no untracked files first.
- Only after the new version is installed: `/mnt/build/g1-tools/moria-env.sh`
  drops `CARGO_REAPI_SRT` and `CARGO_REAPI_TARGET_ROOT` (0.2.10 still reads
  them). `CARGO_REAPI_CACHE_DIR`, `_ACTION_LOG`, `_RESOURCE_LEDGER`,
  `_RESOURCE_CPU_CAPACITY` and `_BACKEND` are still read.
- Update the Moria skill `.agents/skills/cargo-reapi/SKILL.md` for the new
  behavior: link steps and outside-native compiles run uncached.
- `RUSTC_WRAPPER` is still keyed through the `RUST*` family; no misses came
  from it in the measurement.
