# Linux qualification environment

This page describes the Linux-specific mechanism. See
[`../REPRODUCING.md`](../REPRODUCING.md) for the canonical source layout,
matching macOS run, aggregate verification, statistics record, and explicit
disposal policy. The evidence directory produced here is generated output and
is not committed.

The Linux qualification uses the configured x86_64 build server, not GitHub
Actions. The two official base images are digest-pinned in `Dockerfile`.
`run-qualification.sh` records the final built-image inspection alongside the
raw evidence, then runs the qualification as container root in a privileged
container. Root is confined to the disposable container and is required so
nested bubblewrap can configure its loopback namespace; the sandboxed build
itself still runs under bubblewrap/seccomp, and `strace` supplies OS exec
observation.

The container installs the exact local tool versions used by the project:
Rust/Cargo 1.97.1, Node 22, pnpm 11.11.0, and Anthropic Sandbox Runtime 0.0.66.
Linux strict snapshots use bubblewrap plus the provider's seccomp helper;
OS-level compiler/linker observation uses full-argument `strace -f execve`.
Ubuntu 24.04's AppArmor user-namespace restriction strips the capabilities
needed by the provider's nested seccomp namespace. The host wrapper records the
original value, applies the provider-documented value only for the experiment,
and restores the exact original value from an EXIT trap.

```sh
acceptance/linux/run-qualification.sh /srv/cargo-reapi-qualification/source /srv/cargo-reapi-qualification/runs
```

## Reflink qualification volume

The ten-worktree qualification must not amplify one linked snapshot into ten
physical copies. On an ext4-only host, create a disposable sparse XFS loop
volume with reflink support and use it for the result base:

```sh
sudo acceptance/linux/setup-xfs-reflink-volume.sh
acceptance/linux/run-qualification.sh \
  /home/chandler/cargo-reapi-qualification/source \
  /home/chandler/cargo-reapi-qualification-xfs/runs
```

The setup helper refuses to format an existing non-XFS image. It verifies that
the mounted XFS filesystem reports `reflink=1`; the qualification must still
generate and bind its own clone-selection and shared-extent evidence. The mount
is deliberately not persistent across reboot. After aggregate verification and
statistics extraction, tear it down explicitly with
`sudo umount /home/chandler/cargo-reapi-qualification-xfs`; the evidence tree
and sparse image may then be deleted.
