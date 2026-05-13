# Minimal Dockerfile for bris-collector.
#
# Two-stage build: cargo-chef-free for spike simplicity. Iterate
# to cargo-chef once the build cost matters.
FROM rust:1.94-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p bris-collector

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/bris-collector /usr/local/bin/bris-collector
RUN mkdir -p /var/lib/bris-collector
EXPOSE 8443
ENTRYPOINT ["/usr/local/bin/bris-collector"]
