FROM rust:1-slim-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
COPY --from=builder /src/target/release/cargo-reapi /usr/local/bin/cargo-reapi
ENTRYPOINT ["/usr/local/bin/cargo-reapi"]
