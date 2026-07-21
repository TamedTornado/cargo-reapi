# Reproducing the qualification

The acceptance evidence tree is a generated build artifact. It is intentionally
not committed and does not need to be retained after its claims have been
independently verified and the resulting statistics have been recorded. The
durable, reviewable deliverable is this repository: the criteria, pinned
platform definitions, self-identifying runners, receipt assembler, and
fail-closed verifier needed to recreate the evidence.

Run both platforms from the same clean cargo-reapi revision. Each runner records
that revision, the implementation-tree digest, its own digest, the exact and
normative criteria identities, the toolchain, and the platform identity at the
start of the experiment. The aggregate verifier rejects mismatched platform
runs.

## Source layout

Use three sibling, clean Git checkouts. The directory names are significant to
the Linux wrapper:

```text
/path/to/qualification-source/
├── cargo-reapi/
├── bro/
└── moria/
```

The cargo-reapi checkout must be at the revision whose result is being
reproduced. Use the Bro and Moria revisions named by that benchmark record when
reproducing a historical number. A new qualification may use intentionally
selected newer revisions, but its result is a new benchmark rather than a
reproduction of the old one. Moria must be completely clean, including
untracked files. Install Bro's locked dependencies before the macOS run:

```sh
pnpm --dir /path/to/qualification-source/bro install --frozen-lockfile
```

## macOS/arm64

Requirements:

- Apple Silicon macOS with the cache and evidence directories on APFS;
- Rust/Cargo 1.97.1 with `rustfmt` and `clippy`;
- Node 22, pnpm 11.11.0, `jq`, `rg`, and Perl;
- `@anthropic-ai/sandbox-runtime@0.0.66`;
- Full Disk Access for the terminal or agent process that launches the run;
- the repository's narrowly scoped passwordless rule for `/usr/bin/eslogger`.

Prepare the observer and binaries:

```sh
npm install --global @anthropic-ai/sandbox-runtime@0.0.66
./acceptance/install-macos-eslogger-sudoers.sh
cargo build --release --locked --bins
sudo -n -l /usr/bin/eslogger
```

Choose new, empty cache and evidence paths on the APFS SSD, then run the entire
platform qualification from the cargo-reapi repository root:

```sh
./acceptance/run-platform-qualification.sh \
  /path/to/qualification-source/moria \
  /path/to/qualification-source/bro \
  /path/to/generated/macos-cache \
  /path/to/generated/macos-evidence \
  ssd
```

Success produces `macos-evidence/batch.json` plus all 11 receipts and their
recursively referenced raw artifacts. A failed command is a failed experiment;
do not publish statistics from it.

## Linux/x86_64

The Linux wrapper supplies the pinned Rust, Node, pnpm, sandbox-runtime, and OS
observer environment from `acceptance/linux/Dockerfile`. It requires Docker,
an Ubuntu host on which the temporary AppArmor user-namespace setting can be
changed, and enough storage for the cold seed and ten independent targets. The
wrapper records the setting before/during/after the run and restores the exact
original value from its exit trap.

On an ext4 host, create a disposable XFS loop volume so the qualification can
prove reflink copy-on-write rather than exhaust the disk with full copies. Pick
explicit paths and a size appropriate to the workload:

```sh
sudo ./acceptance/linux/setup-xfs-reflink-volume.sh \
  /absolute/path/cargo-reapi-qualification-xfs.img \
  /absolute/path/cargo-reapi-qualification-xfs \
  260G
```

Then run, from the cargo-reapi repository root:

```sh
./acceptance/linux/run-qualification.sh \
  /path/to/qualification-source \
  /absolute/path/cargo-reapi-qualification-xfs/runs
```

The script prints the generated evidence directory on success. It has the form
`.../runs/<UTC-run-id>/evidence` and contains `batch.json`, all 11 receipts,
the raw `strace` process observations, sandbox evidence, and the recursively
bound artifacts. The sparse XFS image and every generated run directory are
disposable after verification and statistics extraction.

## Independent cross-platform verification

The aggregate verifier expects the two generated platform evidence roots under
these exact names. Co-locate them temporarily on any machine; copying them does
not weaken verification because every referenced artifact is rehashed:

```text
/path/to/generated/aggregate/
├── macos-arm64/   # contents of macos-evidence
└── linux-x86_64/  # contents of the Linux .../evidence directory
```

Build the verifier from the revision being evaluated, then run:

```sh
cargo build --release --locked --bins
./target/release/cargo-reapi prove aggregate \
  --root /path/to/generated/aggregate \
  --report /path/to/generated/aggregate-proof.json
```

The command exits nonzero if either platform, any required receipt, any common
identity, or any recursively referenced artifact is missing, failed, or has a
digest mismatch. A successful aggregate is the hostile-review proof. The
aggregate tree and report may then be deleted; their reproducibility, not their
retention, supports the committed benchmark claim.

## Recording a benchmark

Commit only a concise result under `benchmarks/results/`. Record enough identity
and measurements to reproduce and compare the run:

- cargo-reapi, Bro, and Moria revisions;
- contract, normative criteria, and exact criteria-document digests;
- OS, architecture, filesystem/copy mechanism, CPU count, RAM, Rust, and Cargo;
- all Moria 1/5/10 elapsed times and fixed references;
- physical and OS-observed compiler/linker counts;
- Bevy parity time and outcome;
- Bro five-job time and outcome;
- resource maxima, overlap, swap growth, and stall result;
- receipt count, recursive artifact count, and aggregate verifier outcome.

Never copy a `PASS` claim from a partial or failed run. Raw evidence is not a
repository asset and must not be committed. Anyone disputing a benchmark can
checkout its named revisions and regenerate the same independently verifiable
proof using the commands above.
