# macOS APFS current-schema qualification — 2026-07-21

Host: Apple Silicon macOS 26.2, Darwin 25.2.0, 10 logical CPUs, 32 GiB
physical memory, internal APFS SSD. Cargo and Rust were 1.97.1. Strict
whole-gate execution used `@anthropic-ai/sandbox-runtime@0.0.66` with macOS
Seatbelt; independent process observation used `eslogger` exec events with full
arguments.

Evidence-producing cargo-reapi revision: `56a8c88`. Independent verifier
revision: `4ae42b8`. Acceptance contract SHA-256:
`c833908214b7de8a7c593fee7799de7d1fbe5088b411b6d00fca7aa4ef4da500`.
Normative criteria SHA-256:
`77ba9509f276f28112c5aadde3fca1a66b106d0d625e91e61eeb7c6fa386f73c`.
Exact criteria document SHA-256:
`d1ab7cc47e7999ce07825ba6ab6c5cab083d5b834b368c067b6428364ef3db76`.

The current-schema macOS platform batch passed all 11 required receipts. The
independent recursive verifier rehashed 152 retained evidence artifacts and
reported every macOS receipt PASS with no macOS violations. Publication-grade
multi-platform aggregate acceptance remains pending until a matching
current-schema Linux batch exists.

## Moria populations

Every consumer began with an empty target after producer deletion and ran the
complete canonical gate: format, check all targets, clippy all targets with
warnings denied, and tests. Every member in each multi-consumer population
started before any member completed.

| Population | Elapsed | Fixed reference | Physical warm actions | OS-observed compiler/linker actions | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| One clean consumer | 8.302s | 60s | 0 | 0 | pass |
| Five simultaneous consumers | 14.264s | 120s | 0 | 0 | pass |
| Ten simultaneous consumers | 25.016s | 120s | 0 | 0 | pass |

APFS copy-on-write selection was recorded at the implementation branch
`src/gate.rs:clone_tree_with_preference`. The trace records `/bin/cp -cR`, a
successful clone attempt, and `selected_method: copy-on-write` for producer
publication and consumer restoration.

## Bevy linked-binary integrity

The pinned Bevy 0.19 fixture restored its application and integration-test
binary into a differently sized path in 29.231s against the 60s performance
reference. The restored and fresh-control application output, test listing,
test behavior, and exit statuses matched. Embedded paths resolved to the
consumer, macOS signatures were valid, and both the wrapper and `eslogger`
reported zero restored-consumer compiler/linker actions.

## Adversarial correctness

The exact leaf mutation produced the OS-derived rebuild set
`{leaf, mid, adversarial_app}`. The wrapper-derived set matched exactly, the
unrelated crate did not rebuild, 19 selected OS events were attributed, and no
event was invalid. Poison rejection; environment, encoded, workspace,
ancestor, Cargo-home, profile, feature, and target changes; external path and
build-script inputs; proc-macro environment; undeclared filesystem reads;
network denial; successful coalescing; and failing-producer propagation all
passed from fresh isolated caches.

## Bro integration

Bro launched five simultaneous canonical Moria jobs through cargo-reapi's
public standalone interface. All five began before any completed, completed in
14.751s against the 120s reference, and recorded zero physical warm actions
and zero OS-observed compiler/linker actions.

## Resource ledger and stall behavior

The externally monitored resource proof ran two distinct cold Moria check
lanes concurrently, followed by a linked Bevy test build under the same ledger.

| Measurement | Result | Bound |
| --- | ---: | ---: |
| Peak aggregate build-process RSS | 3,502,292,992 bytes | 15 GiB maximum |
| Swap growth | 0 bytes | 512 MiB maximum |
| Peak simultaneous progress processes | 7 | at least 2 |
| Observed lease owners | 700 | nonzero |
| Observed action identities | 675 | at least 2 |

The deliberate no-progress command was classified as infrastructure and
terminated after 300.137s, before its 400-second natural completion. It exited
nonzero and was not presented as an agent code/test failure.

## Reproduction

```sh
cargo build --release --bins
./acceptance/run-platform-qualification.sh \
  /path/to/moria /path/to/bro \
  /path/to/fresh-cache /path/to/fresh-evidence ssd
```

The complete platform run took 5,743.994s (approximately 95m44s), dominated by
the deliberately cold Moria, Bro exact-environment, Bevy-control, resource, and
five-minute stall phases. Warm population and restore timings are reported
separately above.
