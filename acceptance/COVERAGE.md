# Qualification coverage and traceability

This qualification is exhaustive against the acceptance threat model in
`ACCEPTANCE_CRITERIA.md`; it is not a claim that every Rust project, operating
system, filesystem, native toolchain, or remote-execution service has been
tested. This document maps every acceptance area to the code that exercises it,
the receipt that records it, and the evidence a skeptical reviewer can expect a
fresh run to generate.

Raw evidence is never committed. OS event streams, observer stderr, selection
configuration, receipt trees, caches, restored binaries, and aggregate reports
are generated artifacts used during verification and then discarded. Git holds
the runners and verifier needed to regenerate them plus concise benchmark
records containing the resulting identities, pass matrix, and measurements.

`run-platform-qualification.sh` is the platform-level entry point. It invokes
the runners below and will not write a passing `batch.json` unless all 11
required platform receipts exist. `cargo reapi prove aggregate` independently
rehashes every recursively referenced artifact, checks receipt and platform
identity, and requires matching complete macOS and Linux batches.

## Requirement-to-evidence map

| Acceptance area | What is exercised | Runner | Receipt | Independent or behavioral evidence |
| --- | --- | --- | --- | --- |
| Environment and provenance | OS/architecture, filesystem profile, Cargo/Rust identity, contract and criteria digests, source tree, runner and executable identities | `run-moria-local.sh` | `environment` | Intrinsic run-start record, platform profile, environment report |
| Exact mutation | Mutate a leaf crate; require exactly the leaf and transitive dependants to rebuild; exclude the unrelated crate; execute changed behavior | `run-adversarial.sh` | `adversarial` | macOS `eslogger` or Linux `strace` exec arguments produce the rebuild set; wrapper attribution is only a cross-check |
| Poison rejection | Add a deliberately failing dependency test after restore and require the gate to reject it | `run-adversarial.sh` | `adversarial` | Gate exit status, test output, and observed compiler activity |
| Flags and Cargo configuration | Environment `RUSTFLAGS`, encoded flags, workspace/ancestor/Cargo-home configuration, profile, feature, and target changes | `run-adversarial.sh` | `adversarial` | Fresh-cache behavioral tests plus OS process observation |
| External/generated inputs | External path dependency, build-script file/environment input, proc-macro environment, undeclared build/proc-macro reads, and deterministic local network access | `run-adversarial.sh` | `adversarial` | Correct behavior after invalidation or fail-closed rejection without publication |
| Cold-miss coalescing | Two identical simultaneous misses, one producer and one waiter; repeat with a failing producer | `run-adversarial.sh` | `coalescing` | OS exec attribution, coalescing result, waiter behavior, and absence of partial publication |
| Portable-copy isolation | Force the platform-neutral copy fallback and mutate the consumer independently | `run-adversarial.sh` | `portable-copy-isolated` | Focused isolation test log |
| Linked Bevy integrity | Restore a pinned Bevy application and integration-test binary after producer deletion and relocation; compare with a fresh control | `run-bevy-integrity.sh` | `bevy-integrity` | Application output, exit status, test enumeration/behavior, consumer paths, zero observed warm compiler/linker work; macOS signatures or Linux ELF inspection |
| Resource ledger | Simultaneous distinct cold Moria lanes plus a Bevy link; process-tree RSS, swap, overlap, lease owners, and action identities | `run-resources.sh` | `resources` | External process monitor samples and action/member logs |
| Stall classification | A deliberately idle 400-second process must be terminated and classified after 300 seconds | `run-resources.sh` | `resources` | Auditor timing, exit status, stderr, and structured stall report |
| Copy-on-write selection | APFS clone selection on macOS or reflink/fallback selection and shared extents on Linux | `run-moria-local.sh` | `macos-clone` or `linux-copy-mechanism` | Runtime branch trace; Linux filesystem/extent report where available |
| Moria population 1 | Deleted producer, empty consumer target, complete canonical gate | `run-moria-local.sh` | `moria-single` | Gate/member reports, wrapper action logs, raw OS events, selection configuration, and independent OS audit |
| Moria population 5 | Five complete gates start before any completes; no hidden gate cap | `run-moria-local.sh` | `moria-five` | Same evidence as population 1 plus simultaneous timestamps |
| Moria population 10 | Ten complete gates start before any completes; no hidden gate cap | `run-moria-local.sh` | `moria-stress` | Same evidence as population 1 plus simultaneous timestamps |
| Bro integration | Bro starts five simultaneous complete Moria gates through cargo-reapi's public standalone boundary | `run-bro-five.sh` | `bro-five` | Bro CLI output and harness source, producer-retirement record, member logs, raw OS events, and independent OS audit |

The canonical Moria gate in every population and Bro consumer is exactly:

1. `cargo fmt --all -- --check`
2. `cargo check --all-targets`
3. `cargo clippy --all-targets -- -D warnings`
4. `cargo test`

The warm-cache claim has two independent checks: cargo-reapi must report zero
cacheable physical actions, and the OS observer must report zero compiler or
linker executions. A self-reported zero is insufficient.

## Receipt completeness

Each platform batch contains exactly 11 required receipts:

1. `environment`
2. `adversarial`
3. `bevy-integrity`
4. `coalescing`
5. `resources`
6. `portable-copy-isolated`
7. `macos-clone` or `linux-copy-mechanism`
8. `moria-single`
9. `moria-five`
10. `moria-stress`
11. `bro-five`

The verifier treats a missing, failed, stale, mismatched, contradictory, or
digest-invalid receipt as a platform failure. The aggregate additionally
rejects mismatched criteria, contract, implementation tree, or source revision
between platforms.

## Deliberate limits

The current qualification does not establish:

- Windows support;
- live execution against a production REAPI service—the adapter is tested
  against a behaviorally faithful fake `rewrapper`, and live-service validation
  remains a separate milestone;
- compatibility with every Rust crate, native build system, arbitrary
  `build.rs`, proc macro, target triple, SDK, or linker;
- performance equivalence across machines or storage devices—the fixed clocks
  are recorded references, while correctness is evaluated separately;
- qualification of filesystems beyond the recorded APFS and Linux XFS/reflink
  runs, except for the isolated portable-copy fallback test;
- architectures other than the recorded macOS arm64 and Linux x86_64 hosts;
- safety for undeclared nondeterminism outside the enforced sandbox boundary.

Strict mode rejects unsupported or undeclared effects instead of treating them
as cacheable. These limits define the edge of the claim; they are not silently
counted as passing coverage.

See `REPRODUCING.md` for exact platform setup, evidence generation, aggregate
verification, benchmark recording, and disposal instructions.
