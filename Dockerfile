# Stage 1: Build
FROM rust:1.92-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    protobuf-compiler \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Create workspace skeleton for dependency caching
COPY Cargo.toml rust-toolchain.toml ./
COPY crates/core/Cargo.toml crates/core/
COPY crates/storage/Cargo.toml crates/storage/
COPY crates/embedders/Cargo.toml crates/embedders/
COPY crates/consolidators/Cargo.toml crates/consolidators/
COPY crates/forgetters/Cargo.toml crates/forgetters/
COPY crates/server/Cargo.toml crates/server/
COPY crates/client/Cargo.toml crates/client/
COPY crates/cli/Cargo.toml crates/cli/

# Create dummy source files for dependency-only build
RUN mkdir -p crates/core/src crates/storage/src crates/embedders/src \
    crates/consolidators/src crates/forgetters/src crates/server/src \
    crates/client/src crates/cli/src && \
    for d in core storage embedders consolidators forgetters server client; do \
        echo "fn main() {}" > crates/$d/src/lib.rs; \
    done && \
    echo "fn main() {}" > crates/server/src/main.rs && \
    echo "fn main() {}" > crates/cli/src/main.rs

# Build dependencies only (this layer is cached)
RUN cargo build --release -p merkur-server -p merkurctl && \
    rm -rf target/release/deps/merkur* target/release/merkur-server target/release/merkurctl

# Copy actual source
COPY crates/ crates/

# Build binaries
RUN cargo build --release -p merkur-server -p merkurctl --features ollama,openai && \
    cp target/release/merkur-server /usr/local/bin/ && \
    cp target/release/merkurctl /usr/local/bin/

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/merkur-server /usr/local/bin/merkur-server
COPY --from=builder /usr/local/bin/merkurctl /usr/local/bin/merkurctl

RUN mkdir -p /var/lib/merkur/data

ENV MERKUR_SERVER_HOST=0.0.0.0
ENV MERKUR_SERVER_PORT=1934
ENV MERKUR_STORAGE_TYPE=sqlite
ENV MERKUR_STORAGE_SQLITE_PATH=/var/lib/merkur/data/merkur.db

EXPOSE 1934

HEALTHCHECK --interval=30s --timeout=3s \
    CMD curl -sf http://localhost:1934/v1/health || exit 1

ENTRYPOINT ["merkur-server"]
