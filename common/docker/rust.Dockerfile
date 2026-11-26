# syntax=docker/dockerfile:1
# Builds every Rust binary of the workspace once and ships them in a single
# small runtime image (tlb-rust). Services choose the binary via entrypoint.
# Build context: repository root.

FROM rust:1-slim-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY common/tlb-core ./common/tlb-core
COPY common/tools ./common/tools
COPY project-a/consumer ./project-a/consumer
COPY project-b/consumer ./project-b/consumer
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --workspace \
 && mkdir -p /out \
 && find target/release -maxdepth 1 -type f -perm -u+x -exec cp {} /out/ \;

FROM debian:12-slim
COPY --from=builder /out/ /usr/local/bin/
USER 1000:1000
