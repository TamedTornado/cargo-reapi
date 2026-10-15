# cargo-reapi handoff: per-compile caching (read this first)

State as of 2026-10-15. The narrowing is finished and merged to `main`. It is
**not released**: the version is still 0.2.10, there is no tag, nothing is
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

## Open proposals (not built; waiting for the coordinator)

1. **Sound narrowing of the native rule.** Verified with rustc: a
   metadata-only compile reads no native libraries, and an rlib compile reads
   only bundled static libraries (not `dylib`, default-kind or
   `static:-bundle`). So: ignore `-L`/`-l` for metadata-only compiles; ignore
   non-bundled `-l` kinds for rlib compiles; key bundled `-l static=` found
   inside; when outside `-L` directories are present, publish only if the
   produced rlib holds no members besides `lib.rmeta` and the crate's
   `*.rcgu.o` (the same key implies the same `#[link]` attributes, so the
   check carries to every hit). Makes check/clippy and the propagated-`-L`
   crates cacheable.
2. **Proc-macro and dylib caching.** Record the files the linker opens
   (`-Wl,--trace`, supported by GNU ld, lld and mold) when the action runs,
   store them with the entry as link inputs, and verify their digests on
   lookup like dep-info inputs, with the linker identity keyed. Restrict to
   proc-macro and dylib crate types whose traced outside inputs are the C
   toolchain's default libraries (crt objects, libc, libgcc, libm). Restoring
   the same `.so` also removes the miss cascade.

## Release follow-ons (not this lane's)

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
- Possible key splitting to look at: `CARGO_MAKEFLAGS`, `CARGO_BUILD_JOBS`,
  `RUSTC_WRAPPER` and `CARGO_REAPI_SRT` are keyed through the `CARGO*`/`RUST*`
  families; no misses came from them in the measurement.
