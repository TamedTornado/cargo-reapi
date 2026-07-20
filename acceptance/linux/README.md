# Linux qualification environment

The Linux qualification uses the configured x86_64 build server, not GitHub
Actions. The two official base images are digest-pinned in `Dockerfile`.
`run-qualification.sh` records the final built-image inspection alongside the
raw evidence, then runs the qualification as UID 1000 in a privileged container
so bubblewrap/seccomp and ptrace-based `strace` observation can fail closed.

The container installs the exact local tool versions used by the project:
Rust/Cargo 1.97.1, Node 22, pnpm 11.11.0, and Anthropic Sandbox Runtime 0.0.66.
Linux strict snapshots use bubblewrap plus the provider's seccomp helper;
OS-level compiler/linker observation uses full-argument `strace -f execve`.

```sh
acceptance/linux/run-qualification.sh /srv/cargo-reapi-qualification/source /srv/cargo-reapi-qualification/runs
```
