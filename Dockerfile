# The API server.
#
# Multi-stage: the build stage carries the Rust toolchain and ~2GB of
# dependencies; the runtime stage carries a binary and a CA bundle. Shipping the
# builder would mean a compiler in production, which is both a large image and a
# larger attack surface.
FROM rust:1-trixie AS builder
WORKDIR /build

# Dependencies first, in their own layer. Copying the manifests and building a
# stub means a source-only change does not re-download and re-compile the whole
# dependency graph — which for this workspace (ONNX runtime, wasmi, sqlx) is the
# difference between a 30-second rebuild and a 10-minute one.
COPY Cargo.toml Cargo.lock ./
COPY crates/noted-db/Cargo.toml crates/noted-db/
COPY crates/noted-crdt/Cargo.toml crates/noted-crdt/
COPY crates/noted-index/Cargo.toml crates/noted-index/
COPY crates/noted-server/Cargo.toml crates/noted-server/
COPY crates/noted-plugin/Cargo.toml crates/noted-plugin/
RUN mkdir -p crates/noted-db/src crates/noted-crdt/src crates/noted-index/src \
             crates/noted-server/src crates/noted-plugin/src && \
    echo "fn main() {}" > crates/noted-server/src/main.rs && \
    touch crates/noted-db/src/lib.rs crates/noted-crdt/src/lib.rs \
          crates/noted-index/src/lib.rs crates/noted-plugin/src/lib.rs && \
    cargo build --release -p noted-server 2>/dev/null || true

COPY crates crates
# The stubs above leave stale fingerprints; touching the real sources forces a
# rebuild of OUR crates only, not the dependency graph.
RUN touch crates/*/src/lib.rs crates/noted-server/src/main.rs && \
    cargo build --release -p noted-server -p noted-index --bin noted-server --bin noted-index --features noted-index/embed

FROM debian:trixie-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 && \
    rm -rf /var/lib/apt/lists/*

# A non-root user. The server needs no privileges: it binds an unprivileged
# port, and its only writes are to the model cache.
RUN useradd --system --create-home --uid 10001 noted
USER noted
WORKDIR /home/noted

COPY --from=builder /build/target/release/noted-server /usr/local/bin/noted-server
COPY --from=builder /build/target/release/noted-index /usr/local/bin/noted-index

# The embedding model lands here. Mounted as a volume in compose so the ~400MB
# download survives a container rebuild — without it every image rebuild costs
# that download again.
# Created HERE, as the `noted` user, before the volume is mounted over it.
# Docker seeds a named volume from the image path's contents AND ownership — so
# if this directory does not exist in the image, the volume arrives owned by
# root and the non-root process cannot write the model into it. The failure
# reads as "Failed to retrieve onnx/model.onnx", which sounds like a network
# problem and is not.
RUN mkdir -p /home/noted/.fastembed_cache
ENV FASTEMBED_CACHE_PATH=/home/noted/.fastembed_cache
ENV BIND_ADDR=0.0.0.0:8787
EXPOSE 8787

CMD ["noted-server"]
