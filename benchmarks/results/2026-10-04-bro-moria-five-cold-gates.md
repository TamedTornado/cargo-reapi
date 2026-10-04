# Bro/Moria field observation — five concurrent cold gates — 2026-10-04

This is a field observation from Bro's production Moria run, not a
proof-validated benchmark. No `cargo reapi prove` receipt set backs it, and it
does not measure RSS, swap or process ancestry. It records what the shared
compiler ledger and artifact cache did when five independent agents ran cold,
divergent quality gates on one host at the same time.

The machine exposed 20 logical CPUs and 125 GiB RAM (Linux 6.8.0-136, Docker
27.3.1). Each agent ran in its own Bro node container with its own workspace
and target directory; all five shared one artifact cache and one host-wide
resource ledger (`CARGO_REAPI_RESOURCE_LEDGER`).

Pinned revisions:

- cargo-reapi `e2bb32934cb085ba59c1c51ede0064207ebe87b3`
- Moria `f0857bef873df5319cfcfada1e907eb1ce8daadd` (each agent's accepted base)
- project environment
  `project-environments/moria-v2@sha256:3b79cb43e8d2c5f3cd365426598e3a87a1f6a5fa09330e500aeb11488858e903`

## Why this case differs from the July production pass

The [2026-07-22 production pass](2026-07-22-bro-moria-production.md) proved one
cold producer followed by five simultaneous **warm** gates, each of which hit
its gate snapshots and ran zero physical compiler actions. Here the five agents
were implementing five different issues, so each gate changed different source
and compiled its own invalidated crates. Five concurrent cold builds wanted
roughly five times the ledger's 20 CPU tokens.

Under cargo-reapi `4a893c1`, that load failed two of the five gates earlier the
same day with `infrastructure stall: no 1-CPU/7-GiB physical-action lease
became available within 300 seconds`: an action that waited 300 seconds for a
token was declared stalled even while the other gates kept completing actions.
`275284e` makes a wait a stall only after 300 seconds in which no lease in the
ledger is released, as the acceptance criteria define a stall.

## Ledger samples

Sampled every two minutes from inside the node containers. "Held" counts CPU
token locks reported by `lslocks` on the host.

| UTC | CPU tokens held | Progress marker changed | Infrastructure stalls |
| --- | ---: | :---: | ---: |
| 09:24 | 20 | yes | 0 |
| 09:26 | 6 | yes | 0 |
| 09:28 | 14 | yes | 0 |
| 09:30 | 13 | yes | 0 |
| 09:32 | 8 | yes | 0 |
| 09:34 | 4 | yes | 0 |
| 09:36 | 15 | yes | 0 |
| 09:38 | 1 | yes | 0 |
| 09:41 | 0 | yes | 0 |

The ledger reached its full 20-token capacity with all five gates compiling,
queued actions waited behind it, and no action was classified as stalled.

## Action outcomes

Combined histogram of the five action logs at 2026-10-04T09:42:21Z:

| Execution | Records |
| --- | ---: |
| `cache-hit` | 5,420 |
| `coalesced-hit` | 24 |
| `local-cache-miss` | 2,223 |
| `local-failed` | 5 |
| `local-ineligible` | 306 |
| **Total** | **7,978** |

Of 7,667 cacheable actions, 5,444 (71%) reused outputs: 5,420 from the shared
cache and 24 by coalescing onto another gate's in-flight producer. 2,223
executed physically. Per-gate reuse ranged from 38% (the gate whose change sat
lowest in the dependency graph) to 84%. The five `local-failed` records are
compiler errors in the agents' in-progress code. The `local-ineligible`
records are build scripts and capability probes, which run locally by design.

## Evidence boundary

The samples and histogram were read from live action logs and host lock state
during the run; the logs are disposable and were not retained. Bro is our
private harness, so this observation is not publicly reproducible. The public
concurrency runner described in
[`../../acceptance/REPRODUCING.md`](../../acceptance/REPRODUCING.md) still
exercises only warm consumers after one cold producer.
