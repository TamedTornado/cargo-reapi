# Contributor rules

Run commands from the repository root:

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

Cargo is the authoritative build planner. Do not introduce generated Bazel, Buck, or another parallel dependency graph. Reuse the official Remote Execution API and reclient protocols; do not create an incompatible remote-execution service.

Remote eligibility fails closed. An action may run remotely only when its complete input set, output set, toolchain identity, platform contract, arguments, working directory, and relevant environment are represented in the action. Never upload credentials or broad home-directory contents as action inputs.

Preserve wrapper transparency: local and capture modes must return the wrapped compiler's exit status and stdout/stderr semantics. Proc macros and native test artifacts must match the coordinator platform. Build scripts remain local until their filesystem and environment effects are sandboxed and declared.

Use real Cargo invocations in integration tests. Synthetic parser tests are useful but cannot alone prove wrapper compatibility.
