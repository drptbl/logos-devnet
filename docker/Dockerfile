# syntax=docker/dockerfile:1
#
# A standalone LEZ sequencer and the LEZ wallet, built from an upstream tag. Built from stdin, so
# it needs no build context:
#
#   docker build --build-arg LEZ_TAG=v0.3.1 -t ghcr.io/drptbl/logos-devnet-sequencer:v0.3.1 - < docker/Dockerfile
#
# `standalone` replaces Bedrock with an in-process mock, and `prove` runs guests in-process, so
# the image needs neither a Bedrock node nor r0vm. RISC0_SKIP_BUILD_KERNELS skips the GPU kernels
# `prove` would otherwise compile; the sequencer only executes, it never proves.

FROM rust:1-slim-trixie AS build
ARG LEZ_TAG
ARG FEATURES=standalone,prove
RUN test -n "$LEZ_TAG" || { echo "set --build-arg LEZ_TAG=<tag>" >&2; exit 1; }
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev libclang-dev clang cmake ninja-build \
        git ca-certificates curl unzip libpcsclite-dev \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 --branch "$LEZ_TAG" https://github.com/logos-blockchain/logos-execution-zone.git /lez
WORKDIR /lez
# The tag's rust-toolchain.toml picks the compiler; rustup installs it on first use. Two builds:
# in one, `prove` would reach the wallet too, which then links the kernels skipped here. The
# target cache is locked, so a concurrent build of another tag can't swap binaries before the copy.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/lez/target,sharing=locked \
    RISC0_SKIP_BUILD_KERNELS=1 cargo build --release --features "$FEATURES" -p sequencer_service \
    && cargo build --release -p wallet \
    && mkdir -p /out \
    && cp target/release/sequencer_service target/release/wallet /out/ \
    && strip /out/sequencer_service /out/wallet
# Upstream's debug config for this tag; the CLI patches it rather than shipping its own.
RUN mkdir -p /out/share \
    && config="$(find . -path ./target -prune -o -path '*sequencer/service/configs/debug/sequencer_config.json' -print | head -1)" \
    && test -n "$config" \
    && cp "$config" /out/share/sequencer_config.json

FROM debian:trixie-slim
ARG LEZ_TAG
LABEL org.opencontainers.image.source="https://github.com/drptbl/logos-devnet" \
      org.opencontainers.image.description="Standalone LEZ sequencer and wallet for local development" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      io.logos.lez.tag="$LEZ_TAG"
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3t64 libstdc++6 libpcsclite1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/sequencer_service /out/wallet /usr/local/bin/
COPY --from=build /out/share/sequencer_config.json /usr/share/logos-devnet/sequencer_config.json
# No CMD: the logos-devnet CLI's compose file says how to run each binary.
RUN sequencer_service --version && wallet --version
