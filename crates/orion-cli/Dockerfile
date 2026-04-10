# Planner stage: generate a recipe for dependency caching
FROM rust:1.93-slim AS planner
RUN cargo install cargo-chef --locked
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
RUN cargo chef prepare --recipe-path recipe.json

# Builder stage: cache dependencies, then build
FROM rust:1.93-slim AS builder

WORKDIR /app

RUN cargo install cargo-chef --locked

# Cook dependencies (cached unless Cargo.toml/Cargo.lock change)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Build application (only this layer rebuilds on source changes)
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
COPY build.rs ./

RUN cargo build --release --locked

# Runtime stage
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

RUN groupadd --system orion && useradd --system --gid orion --no-create-home orion

WORKDIR /app
RUN mkdir -p /home/orion/.orion && chown -R orion:orion /home/orion /app

COPY --from=builder --chown=orion:orion /app/target/release/orion-cli /usr/local/bin/orion-cli

USER orion

# Default MCP HTTP server port
EXPOSE 8081

ENTRYPOINT ["orion-cli"]
