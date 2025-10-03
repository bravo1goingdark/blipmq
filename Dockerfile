# Multi-stage optimized Docker build for BlipMQ
# Build stage
FROM rust:1.75-slim as builder

# Install system dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    clang \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./

# Create dummy source to cache dependencies
RUN mkdir -p src/bin src/core/fbs && \
    echo "fn main() {}" > src/bin/blipmq.rs && \
    echo "fn main() {}" > src/bin/blipmq-cli.rs && \
    echo "" > src/lib.rs && \
    echo 'table DummyTable {}' > src/core/fbs/blipmq.fbs

# Build dependencies (cached layer)
RUN cargo build --release --features mimalloc && \
    rm -rf src target/release/deps/blipmq* target/release/deps/libblipmq*

# Copy source code
COPY . .

# Build optimized release binary
ENV RUSTFLAGS="-C target-cpu=native -C opt-level=3 -C lto=fat -C codegen-units=1 -C panic=abort"
RUN cargo build --release --features mimalloc && \
    strip target/release/blipmq && \
    strip target/release/blipmq-cli

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create app user
RUN groupadd -r blipmq && useradd -r -g blipmq -s /bin/false blipmq

# Create directories
RUN mkdir -p /app/config /app/logs /app/wal && \
    chown -R blipmq:blipmq /app

WORKDIR /app

# Copy binaries
COPY --from=builder /app/target/release/blipmq /app/target/release/blipmq-cli ./

# Copy configuration files
COPY blipmq.toml ./blipmq-example.toml
COPY config/blipmq-production.toml ./config/

# Create default production config
COPY config/blipmq-production.toml ./blipmq.toml

# Set proper permissions
RUN chmod +x blipmq blipmq-cli && \
    chown -R blipmq:blipmq /app

# Switch to app user
USER blipmq

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD ./blipmq-cli --addr 127.0.0.1:7878 pub healthcheck ping || exit 1

# Expose ports
EXPOSE 7878 9090

# Default command
CMD ["./blipmq", "start", "--config", "blipmq.toml"]

# Labels
LABEL org.opencontainers.image.title="BlipMQ"
LABEL org.opencontainers.image.description="Ultra-lightweight, high-performance message queue written in Rust"
LABEL org.opencontainers.image.vendor="BlipMQ Project"
LABEL org.opencontainers.image.source="https://github.com/bravo1goingdark/blipmq"
LABEL org.opencontainers.image.documentation="https://github.com/bravo1goingdark/blipmq/blob/main/README.md"
