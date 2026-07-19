# Historical self-reported Moria experiment — 2026-07-19

> Historical unaudited evidence, retained for comparison. This run met its
> timing and self-reported action-count thresholds, but it predated the external
> `rustc` observer and the adversarial invalidation contract. It is **not** a
> current acceptance result and does not prove that zero physical compiler work
> occurred.

This was the first timing pass under the earlier immutable contract embedded
from `acceptance/contract.toml`. It superseded the 2026-07-18 bounded experiment
at the time, but was later rejected as acceptance evidence when the harness was
made independent of cargo-reapi's own classifications.

The proof ran on native Apple Silicon against Moria's real Bevy/wgpu workspace.
A cold producer completed the canonical `fmt`, `check`, `clippy`, and `test`
gate and published durable target snapshots into a shared CAS. The producer
worktree was then renamed out of service before any measured consumer started.

| Cohort | Members | Simultaneous | Elapsed | Limit | Cacheable physical actions |
| --- | ---: | --- | ---: | ---: | ---: |
| Single | 1 | yes | 15.431 s | 60 s | 0 |
| N | 5 | yes; every member started before any completed | 54.808 s | 120 s | 0 |
| 2N stress | 10 | yes; every member started before any completed | 110.513 s | 120 s | 0 |

Every member ran the full canonical gate and recorded three logical
`gate-snapshot-hit` events for `check`, `clippy`, and `test`. `fmt` and Cargo's
non-cacheable control probes remained local and were not counted as cacheable
physical compiler or linker actions. All member commands exited successfully.

The pinned Bevy producer-deletion fixture also passed on the same implementation.
Its fresh producer compiled and linked in 7m03s. After that producer was deleted,
the consumer restored the linked artifact, completed Cargo's gate in 0.28s, and
executed the binary with its embedded path resolving to the consumer worktree.

The executable contract hash for the passing Moria report was
`9241b245864d9b6062d5d6f76880dcce7a2d65133f47f7efa35cbe2c0b60001e`.
The report was produced by `acceptance/run-moria-local.sh`; that runner exposes
no population, concurrency, or timing override flags.
