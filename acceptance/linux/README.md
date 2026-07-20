# Linux qualification environment

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
