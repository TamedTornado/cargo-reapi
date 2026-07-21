# Linux XFS schema-v3 qualification — 2026-07-21

Host: Ubuntu 24.04, Linux 6.8, x86_64, 20 logical CPUs, approximately
62.6 GiB RAM. The qualification data root was a 260 GiB sparse XFS loop volume
on the host's ext4 NVMe RAID1 filesystem, formatted with `reflink=1`.

Evidence-producing cargo-reapi revision:
`56a8c882688e5c8ed360237054492437d5671853`. Bro revision:
`3ea6630740ce5d0d1640d0ba15a017b46b3fa726`. Moria revision:
`6542cb53a636f191d9a4de72d476c3c4f06e3fe4`. Acceptance contract SHA-256:
`c833908214b7de8a7c593fee7799de7d1fbe5088b411b6d00fca7aa4ef4da500`.
Normative criteria SHA-256:
`77ba9509f276f28112c5aadde3fca1a66b106d0d625e91e61eeb7c6fa386f73c`.
Exact criteria document SHA-256:
`d1ab7cc47e7999ce07825ba6ab6c5cab083d5b834b368c067b6428364ef3db76`.

The schema-v3 Linux platform runner completed in approximately 1h55m and
sealed all 11 required receipts with `PASS` status and no reported violations.
The host AppArmor user-namespace setting was restored to its original value
after the container exited.

## Receipt matrix

| Required receipt | Result |
| --- | --- |
| `environment` | PASS |
| `adversarial` | PASS |
| `bevy-integrity` | PASS |
| `coalescing` | PASS |
| `resources` | PASS |
| `portable-copy-isolated` | PASS |
| `linux-copy-mechanism` | PASS |
| `moria-single` | PASS |
| `moria-five` | PASS |
| `moria-stress` | PASS |
| `bro-five` | PASS |

## Moria populations

Every consumer began with an empty target after producer deletion and ran the
complete canonical gate. Every member of the five- and ten-consumer populations
started before any member completed.

| Population | Elapsed | Fixed reference | Physical warm actions | OS-observed compiler/linker actions | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| One clean consumer | 6.455s | 60s | 0 | 0 | pass |
| Five simultaneous consumers | 10.818s | 120s | 0 | 0 | pass |
| Ten simultaneous consumers | 18.852s | 120s | 0 | 0 | pass |

## Bevy linked-binary integrity

The pinned Bevy fixture restored and relocated its linked application and test
binary in 9.944s against the 60s reference. Application behavior, test
behavior, consumer-only embedded paths, and ELF integrity passed. The external
Linux observer recorded zero compiler or linker executions in the restored
consumer.

## Bro integration

Bro launched five simultaneous canonical Moria jobs through cargo-reapi's
public boundary. All five completed in 32.015s against the 120s reference with
zero physical warm actions and zero OS-observed compiler/linker executions.

## Resource ledger and stall behavior

| Measurement | Result | Bound |
| --- | ---: | ---: |
| Peak aggregate build-process RSS | 9,323,200,512 bytes | 15 GiB maximum |
| Swap growth | 0 bytes | 512 MiB maximum |
| Peak simultaneous progress processes | 7 | at least 2 |
| Observed lease owners | 1,088 | nonzero |
| Observed action identities | 843 | at least 2 |

The deliberate no-progress command was classified as infrastructure and
terminated after 300.057s rather than completing naturally at 400s.

## Independent recursive verification

The first independent recursive verification attempt exposed a verifier defect:
it treated every referenced `.json` artifact as an object containing
`evidence_refs`, while Docker inspection evidence is a valid top-level JSON
array. Verifier revision `63e634d96735b70f78c99b43589a785692e86990`
repaired traversal without changing the evidence producer: every referenced
artifact is digest-verified, arbitrary JSON is a leaf, and a top-level object
with an explicit malformed `evidence_refs` field still fails closed.

The repaired Linux x86_64 verifier binary SHA-256 was
`de918d047bc9010d6d43f17293cb7cea568790a1644edd80171fbda1ea291f61`.
It rehashed the unchanged generated evidence and reported:

- Linux platform status: `PASS`;
- all 11 receipt statuses: `PASS`;
- recursively verified artifacts: 192;
- Linux violations: zero.

The verification report SHA-256 was
`c18d3c495e5ad2a294830e1ba0b99210c006fe8f8bc0a7e6415186ee238c7131`.
The enclosing two-platform command reported macOS `UNMET` because the previous
macOS raw evidence tree had already been intentionally discarded. That absence
does not alter this Linux platform result and is not presented as a combined
cross-platform aggregate pass.
