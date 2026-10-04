# syntax=docker/dockerfile:1

# --- Stage 1: generate the dependency recipe ---
FROM lukemathwalker/cargo-chef:latest-rust-1.95-slim AS chef
WORKDIR /app
RUN apt-get update && apt-get install -y pkg-config libssl-dev curl && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# --- Stage 2: cook dependencies (cached) then build the binary ---
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin alexa-mcp

# --- Stage 3: production runtime ---
FROM debian:trixie-slim AS runner
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && update-ca-certificates

WORKDIR /app
RUN useradd -r -s /bin/false appuser && chown -R appuser:appuser /app

COPY --from=builder /app/target/release/alexa-mcp /usr/local/bin/alexa-mcp
RUN chmod +x /usr/local/bin/alexa-mcp

USER appuser

ENV BIND_ADDR=0.0.0.0:8080
ENV RUST_LOG=alexa_mcp=info,tower_http=info

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

CMD ["/usr/local/bin/alexa-mcp"]
