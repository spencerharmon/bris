# Dockerfile for the `bris` CLI image used by the nightly replay CronJob.
#
# The collector image (crates/bris-collector/Dockerfile) ships ONLY the
# bris-collector binary; it does NOT contain the `bris` CLI (crate
# bris-cli, [[bin]] name = "bris") that owns the `replay` subcommand. This
# image builds and ships that CLI so the flux bris-collector deploy can run
#   bris replay --all-sessions --render-frames
# against the collector's durable store PVC.
#
# The `--render-frames` / ml-gravity path pulls in bris-vision's `ort`
# (ONNX Runtime) dependency (workspace Cargo.toml: ort with
# `download-binaries`), which links a native libonnxruntime shared library
# fetched at build time. That .so is emitted alongside the binary in
# target/release/ and must travel to the runtime image, so we copy every
# libonnxruntime*.so* next to the binary path and register it with ldconfig.
#
# INFRA-AGNOSTIC: this file contains no registry host, namespace, domain,
# or credential. Those are supplied entirely by the Gitea Actions workflow
# (.github/workflows/bris-cli-image.yml) from vars/secrets.

FROM rust:1.94-bookworm AS builder
WORKDIR /src
COPY . .
# bris-cli's dependency graph already enables bris-vision/bris-streaming
# `segmentation` + `ml-gravity`, so the release build produces the full
# vision/render path the CronJob's --render-frames needs.
RUN cargo build --release -p bris-cli --bin bris
# Collect the ONNX Runtime native lib(s) the ml-gravity path links, if any,
# into a stable staging dir for the runtime stage.
RUN mkdir -p /stage/lib \
    && find target/release -maxdepth 1 -name 'libonnxruntime*.so*' -exec cp -av {} /stage/lib/ \; \
    && ls -l /stage/lib || true

FROM debian:bookworm-slim
# ca-certificates for any HTTPS the CLI performs; libstdc++6 / libgcc are
# the C++ runtime the ONNX Runtime native lib depends on (present in the
# base but pinned here so the dependency is explicit).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/bris /usr/local/bin/bris
# Install the ONNX Runtime native lib(s) (if the ml-gravity build linked
# one dynamically) and refresh the loader cache so `bris replay
# --render-frames` can resolve them at runtime.
COPY --from=builder /stage/lib/ /usr/local/lib/
RUN ldconfig
ENTRYPOINT ["/usr/local/bin/bris"]
