# Minimal Dockerfile for bris-collector.
#
# Two-stage build: cargo-chef-free for spike simplicity. Iterate
# to cargo-chef once the build cost matters.
FROM rust:1.94-bookworm AS builder
# These are native builds, including on ARM64. Do not use the Pi cross
# wrappers from .cargo/config.toml (their Nix toolchain is not in this image).
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=gcc \
    CC_aarch64_unknown_linux_gnu=gcc \
    CXX_aarch64_unknown_linux_gnu=g++ \
    AR_aarch64_unknown_linux_gnu=ar
WORKDIR /src
COPY . .
RUN cargo build --locked --release -p bris-collector

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/bris_collector /usr/local/bin/bris-collector
RUN mkdir -p /var/lib/bris-collector
EXPOSE 8443
ENTRYPOINT ["/usr/local/bin/bris-collector"]
