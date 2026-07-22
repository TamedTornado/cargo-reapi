FROM rust:1-slim-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY acceptance/contract.toml acceptance/ACCEPTANCE_CRITERIA.md ./acceptance/
RUN cargo build --locked --release --bins

FROM debian:bookworm-slim
COPY --from=builder /src/target/release/cargo-reapi /usr/local/bin/cargo-reapi
COPY --from=builder /src/target/release/cargo-reapi-auditor /usr/local/bin/cargo-reapi-auditor
COPY --from=builder /src/target/release/cargo-reapi-exec-auditor /usr/local/bin/cargo-reapi-exec-auditor
ENTRYPOINT ["/usr/local/bin/cargo-reapi"]
