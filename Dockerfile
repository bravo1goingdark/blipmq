# Build stage
FROM rust:1.75 as builder

WORKDIR /app

# Copy source code
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
COPY build.rs ./

# Build release binary
RUN cargo build --release --bin blipmq-v2

# Runtime stage
FROM debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder
COPY --from=builder /app/target/release/blipmq-v2 /usr/local/bin/blipmq-v2

# Configuration
ENV RUST_LOG=info
ENV BLIPMQ_LISTEN=0.0.0.0:9999

# Expose port
EXPOSE 9999

# Run broker
CMD ["blipmq-v2"]
