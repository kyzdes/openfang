# syntax=docker/dockerfile:1
FROM rust:1-slim-bookworm AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y pkg-config libssl-dev perl make && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask
COPY agents ./agents
COPY packages ./packages
# Optional build args for dev environments to speed up compilation
# Example: docker build --build-arg LTO=false --build-arg CODEGEN_UNITS=16 .
ARG LTO=true
ARG CODEGEN_UNITS=1
ENV CARGO_PROFILE_RELEASE_LTO=${LTO} \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CODEGEN_UNITS}
# Cache the registry, the git checkouts and target/ across builds. Without this a
# one-line change recompiles all 13 crates from scratch: ~9 minutes on 4 cores,
# which is the whole cost of iterating on a patch.
#
# sharing=locked, not the default "shared": two concurrent builds writing the same
# target/ corrupt each other's incremental state. Serialising them is cheaper than
# debugging the result.
#
# The binary is copied out inside this RUN on purpose. A cache mount is not part of
# the layer, so `COPY --from=builder /build/target/...` in the next stage would find
# nothing there — and would do it silently, producing an image whose openfang is
# missing or stale. Hence /openfang, which is a real layer file.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --bin openfang \
    && cp target/release/openfang /openfang

FROM rust:1-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    python3 \
    python3-pip \
    python3-venv \
    nodejs \
    npm \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /openfang /usr/local/bin/openfang
COPY --from=builder /build/agents /opt/openfang/agents
EXPOSE 4200
VOLUME /data
ENV OPENFANG_HOME=/data
ENTRYPOINT ["openfang"]
CMD ["start"]
