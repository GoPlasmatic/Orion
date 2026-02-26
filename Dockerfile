# Build stage
FROM rust:1.93-slim AS builder

RUN apt-get update && apt-get install -y pkg-config cmake g++ curl libcurl4-openssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
RUN cargo fetch --locked

COPY src/ src/
COPY migrations/ migrations/

RUN cargo build --release --locked

# Runtime stage
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

RUN groupadd --system orion && useradd --system --gid orion --no-create-home orion

WORKDIR /app
RUN chown orion:orion /app

COPY --from=builder --chown=orion:orion /app/target/release/orion-server /usr/local/bin/orion-server
COPY --chown=orion:orion config.toml.example /app/config.toml.example

USER orion

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -f http://localhost:8080/health || exit 1

ENTRYPOINT ["orion-server"]
