# Planner stage: generate a recipe for dependency caching
FROM rust:1.93-slim AS planner
RUN cargo install cargo-chef --locked
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
RUN cargo chef prepare --recipe-path recipe.json

# Builder stage: cache dependencies, then build
FROM rust:1.93-slim AS builder

# perl is required by rdkafka's vendored OpenSSL build (kafka.auth TLS/SASL)
RUN apt-get update && apt-get install -y pkg-config cmake g++ curl libcurl4-openssl-dev perl && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked

WORKDIR /app

# Cook dependencies (cached unless Cargo.toml/Cargo.lock change)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Build application (only this layer rebuilds on source changes)
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
COPY migrations/ migrations/
COPY build.rs ./

# .dockerignore excludes .git/, so build.rs cannot ask git for the commit;
# CI passes it as a build-arg (--build-arg GIT_HASH=<sha>) and build.rs
# prefers the env var. Left unset, /health reports git_hash=unknown.
ARG GIT_HASH
ENV GIT_HASH=${GIT_HASH}

RUN cargo build --release --locked

# Runtime stage
FROM debian:trixie-slim

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

# Pinned numeric UID/GID: Kubernetes' runAsNonRoot check cannot verify a
# named image USER, so the Helm chart (and any PodSecurity `restricted`
# namespace) needs a numeric identity. Keep 10001 in sync with the chart's
# default podSecurityContext (runAsUser/runAsGroup/fsGroup).
RUN groupadd --system --gid 10001 orion && useradd --system --uid 10001 --gid orion --no-create-home orion

WORKDIR /app
RUN mkdir -p /app/data && chown -R orion:orion /app

COPY --from=builder --chown=orion:orion /app/target/release/orion-server /usr/local/bin/orion-server
COPY --chown=orion:orion config.toml.example /app/config.toml.example

# Numeric form so orchestrators can verify non-root without /etc/passwd.
USER 10001:10001

# Default data directory for SQLite database.
# Mount a persistent volume here to preserve data across container restarts.
# Note: SQLite WAL mode creates .wal and .shm sidecar files that must be
# on the same volume as the main database file.
VOLUME /app/data

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -f http://localhost:8080/health || exit 1

ENTRYPOINT ["orion-server"]
