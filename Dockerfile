# Multi-stage hardened build for zero-overhead, zero-leak runtime
FROM rust:alpine AS builder

WORKDIR /build

# Install build dependencies
RUN apk add --no-cache musl-dev

# Copy manifests first for dependency caching
COPY Cargo.toml ./

# Create dummy source to cache compiled dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Copy real source code
COPY src ./src

# Build production binary with LTO and strip
RUN cargo build --release

# Stage 2: Hardened runtime environment
FROM alpine:3.21

# Install CA certificates for outbound HTTPS calls to Alpaca API
RUN apk add --no-cache ca-certificates tzdata

# Create secure non-root user with explicit UID/GID
RUN addgroup -g 10001 trader && \
    adduser -u 10001 -G trader -s /bin/sh -D trader

# Create directories with restricted permissions
WORKDIR /app
RUN mkdir -p /data && chown -R trader:trader /app /data

# Copy binary from builder stage
COPY --from=builder --chown=trader:trader /build/target/release/anomalies-trading-engine /app/trading-engine

# Copy default parameters
COPY --chown=trader:trader params.json /app/params.json

# HARD CONSTRAINTS:
# 1. Non-root user
USER trader

# 2. Strict Network Isolation: NO ports are exposed.
# Container only makes outbound HTTPS requests to paper-api.alpaca.markets & data.alpaca.markets.

# 3. Persistent state volume
VOLUME ["/data"]

ENTRYPOINT ["/app/trading-engine"]
CMD ["--params", "/app/params.json", "--env", "/app/.env", "--data", "/data"]
