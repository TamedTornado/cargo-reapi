# Moria local acceptance — 2026-07-18

> Historical failed proof, retained as evidence. This run serialized five logical
> gates through a two-process admission cap and still executed cacheable actions.
> It does **not** satisfy the current embedded contract in
> `acceptance/contract.toml`, particularly PAR-1, PERSIST-2, or SCALE-1.

## Scope

This run exercised the local shared-CAS backend against Moria's real Bevy dependency graph. It did not use GitHub CI, a PR workflow, or a remote REAPI service.

Five detached worktrees at Moria commit `6542cb5` used independent `CARGO_TARGET_DIR` roots, one shared cargo-reapi cache, and `cargo check -p moria-world --lib -j 1`. Physical admission was incorrectly bounded to two simultaneous Cargo processes. The cache was pre-populated by one cold worktree before the five-worktree measurement.

The same standalone source was also built as a Linux image and consumed by Bro's 2-CPU/4-GiB build-worker. A cold Linux `moria-world` check completed successfully through the mounted cargo-reapi executable and named cache volume.

## Results

| Worktree | Cache hits | Cacheable local misses | Fail-closed local actions | Wall time |
| --- | ---: | ---: | ---: | ---: |
| one | 63 | 22 | 64 | 143.88 s |
| two | 63 | 22 | 64 | 143.88 s |
| three | 63 | 22 | 64 | 143.51 s |
| four | 63 | 22 | 64 | 143.50 s |
| five | 63 | 22 | 64 | 139.65 s |

The three bounded admission waves completed in approximately 427 seconds (7m07s), below the 15-minute five-worktree target. All five commands exited successfully. A same-target repeat completed in 0.43 seconds.

The deployed Linux worker's cold run completed in 2m15s and recorded 85 local cache misses plus 62 fail-closed local actions, proving that Bro executed real Cargo actions through the standalone Linux artifact rather than merely discovering the command.

## Unresolved linker boundary

This is not a claim that the full warm-gate goal is solved. Every fresh worktree still rebuilt 22 otherwise-cacheable downstream actions. The first divergent inputs are locally linked proc-macro `.dylib` files whose bytes differ between worktrees; their changed digests correctly invalidate consumers and then cascade through Bevy metadata.

Pretending those artifacts have stable identity would permit stale or incompatible outputs. cargo-reapi therefore continues to reject link actions until linker binaries, native libraries, response files, SDK inputs, embedded paths, debug information, and output determinism are represented and tested. The current result proves bounded admission, ordinary metadata reuse, single-flight behavior, and fail-closed correctness. It does not satisfy the 60-second fresh-worktree warm-gate goal.
