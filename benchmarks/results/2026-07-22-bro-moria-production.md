# Bro/Moria production dogfood — Linux XFS — 2026-07-22

This is a pass-only statistics record from Bro's isolated Moria deployment on
Linux 6.8.0 x86_64, Docker 27.3.1, and a 200 GiB XFS `reflink=1` volume. The
machine exposed 20 logical CPUs and approximately 62.6 GiB RAM. The complete
machine-readable record is
[`2026-07-22-bro-moria-production.json`](2026-07-22-bro-moria-production.json).

Pinned revisions:

- cargo-reapi `d09e14eb0c57b9241f82aa8e97afd0ec1e542478`
- Bro `235c6b553c323572a5f02154a139345a75817d2d`
- Moria `e466da505d1c28880f8f86151b12ba6ad1ec0823`
- acceptance contract `c833908214b7de8a7c593fee7799de7d1fbe5088b411b6d00fca7aa4ef4da500`

## Results

| Measurement | Result |
| --- | ---: |
| Singular cold producer | 3,125.608s |
| Cold peak process-tree RSS | 6,372,159,488 bytes |
| Cold swap growth | 144,199,680 bytes |
| Cold infrastructure stall | false |
| Five simultaneous warm gates | 24.695s |
| Warm reference | 120s |
| Warm peak process-tree RSS | 879,132,672 bytes |
| Warm swap growth | 0 bytes |

All five clean consumers started before any consumer completed. Their durations
were 24.695s, 24.660s, 24.632s, 24.673s, and 24.645s. Each consumer ran Bro's
complete `RustProjectGate`: `cargo fmt`, `cargo check --all-targets`, `cargo
clippy --all-targets -- -D warnings`, and `cargo test`.

Every consumer began with an empty target and recorded exactly three
`gate-snapshot-hit` events. Each independently produced:

- zero cacheable physical actions;
- zero externally observed compiler actions;
- no resource violation or infrastructure stall.

## Evidence boundary and reproduction

Bro is our private agentic coding harness. Its orchestration-specific runner is
therefore not a public reproduction surface. This record retains the measured
statistics, pinned Bro revision, acceptance-contract digest, and public Moria
and cargo-reapi revisions from that production run.

The public cargo-reapi runner exercises the same cache mechanics without Bro:

```sh
sudo -v
target/release/cargo-reapi-auditor run \
  --report /path/to/proof-report/resource-proof.json -- \
  acceptance/run-moria-local.sh \
  /path/to/Moria \
  /shared/cargo-reapi-cache \
  /path/to/proof-report \
  ssd
```

It cold-seeds one producer, retires that producer path, and launches complete
Moria gates in simultaneous clean consumers. The full procedure, external OS
observation, adversarial suite, and evidence disposal rules are documented in
[`../../acceptance/REPRODUCING.md`](../../acceptance/REPRODUCING.md).

The consumer command itself creates exactly five worktrees and rejects the run
unless all five overlap, finish within the fixed SSD reference, pass the full
gate, and report zero internal and externally observed compiler work. Raw proof
packages are reproducible generated artifacts and were deleted after these
statistics were extracted.

For continuing field results—including one-producer/four-waiter behavior on a
real Bevy action, live cache-eligibility defects, repairs, and production
resource samples—see the
[Moria agent-fleet dogfood case study](../../docs/case-studies/moria-agent-fleet.md).
