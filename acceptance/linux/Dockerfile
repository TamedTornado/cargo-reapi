FROM node:22-bookworm-slim@sha256:6c74791e557ce11fc957704f6d4fe134a7bc8d6f5ca4403205b2966bd488f6b3 AS node
FROM rust:1.97.1-bookworm@sha256:77fac8b98f9f46062bb680b6d25d5bcaabfc400143952ebc572e924bcbedc3fa

COPY --from=node /usr/local/ /usr/local/

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      binutils bubblewrap ca-certificates clang e2fsprogs elfutils git jq lsof perl \
      pkg-config ripgrep rsync socat strace util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && npm install --global @anthropic-ai/sandbox-runtime@0.0.66 pnpm@11.11.0 \
    && useradd --create-home --uid 1000 --user-group qualifier \
    && chown -R qualifier:qualifier /usr/local/cargo /usr/local/rustup /home/qualifier

ENV HOME=/home/qualifier
ENV PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin
USER qualifier
WORKDIR /work/cargo-reapi

CMD ["bash"]
