# Builds from the WORKSPACE ROOT context:
#   docker build -f crates/orion-cli/Dockerfile .
# The workspace manifest set must be complete for cargo metadata to resolve,
# so the whole crates/ tree rides in — but every cargo invocation is scoped
# with -p/--package orion-cli, so the server's heavy native deps (librdkafka,
# vendored OpenSSL) are never compiled here.
#
# Digest-pinned to the same base images as the server Dockerfile so a rebuild
# months from now resolves the same layers; dependabot bumps the tag+digest.
FROM rust:1.93-slim@sha256:c0a38f5662afdb298898da1d70b909af4bda4e0acff2dc52aea6360a9b9c6956 AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# Planner stage: generate a recipe for dependency caching
FROM chef AS planner
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

# Builder stage: cache dependencies, then build
FROM chef AS builder

# Cook dependencies (cached unless the manifests/lockfile change). --locked to
# match the build below.
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --package orion-cli --recipe-path recipe.json

# Build application (only this layer rebuilds on source changes)
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/

# .dockerignore excludes .git/, so build.rs cannot ask git for the commit;
# CI passes it as a build-arg (--build-arg GIT_HASH=<sha>) and build.rs
# prefers the env var. Left unset, `orion-cli --version` reports unknown.
ARG GIT_HASH
ENV GIT_HASH=${GIT_HASH}

RUN cargo build --release --locked -p orion-cli

# Runtime stage
FROM debian:trixie-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd

# MCP registry ownership verification (must match server.json "name")
LABEL io.modelcontextprotocol.server.name="io.github.GoPlasmatic/orion"
LABEL org.opencontainers.image.title="Orion CLI" \
      org.opencontainers.image.description="CLI and MCP server for the Orion declarative services runtime" \
      org.opencontainers.image.source="https://github.com/GoPlasmatic/Orion" \
      org.opencontainers.image.vendor="Plasmatic" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

RUN groupadd --system orion && useradd --system --gid orion --no-create-home orion

WORKDIR /app
RUN mkdir -p /home/orion/.orion && chown -R orion:orion /home/orion /app

COPY --from=builder --chown=orion:orion /app/target/release/orion-cli /usr/local/bin/orion-cli

USER orion

# Default MCP HTTP server port
EXPOSE 8081

ENTRYPOINT ["orion-cli"]
