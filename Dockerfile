# Shared toolchain stage: cargo-chef is compiled from source ONCE here and
# inherited by both stages below — it used to be built independently in each,
# doubling the cold-cache cost for the same binary (T23).
# P18: digest-pinned (multi-arch manifest list) so a rebuild months from now
# resolves the same base layers; dependabot bumps the tag+digest pair.
FROM rust:1.97-slim@sha256:8e8cf8f7fd54a2d23d5a743b3a03f56e26b6c774276c33fa0595111704ebb15c AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# Planner stage: generate a recipe for dependency caching. The workspace
# manifest set must be complete for cargo metadata to resolve, so the whole
# crates/ tree rides in (the CLI crate is small; .dockerignore already trims
# the context).
FROM chef AS planner
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/
RUN cargo chef prepare --recipe-path recipe.json

# Builder stage: cache dependencies, then build
FROM chef AS builder

# perl is required by rdkafka's vendored OpenSSL build (kafka.auth TLS/SASL)
RUN apt-get update && apt-get install -y pkg-config cmake g++ curl libcurl4-openssl-dev perl && rm -rf /var/lib/apt/lists/*

# Cook dependencies (cached unless Cargo.toml/Cargo.lock change). --locked to
# match the build below: without it, cook could resolve different dependency
# versions than the build then verifies — quietly defeating both the cache
# and the lockfile guarantee (T23).
COPY --from=planner /app/recipe.json recipe.json
# P17: the `dist` profile (thin LTO on top of release), matching how the
# GitHub-release archives are built — same version string, same optimization,
# instead of two measurably different binaries sharing a version.
RUN cargo chef cook --profile dist --locked --recipe-path recipe.json

# Build application (only this layer rebuilds on source changes)
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/

# .dockerignore excludes .git/, so build.rs cannot ask git for the commit;
# CI passes it as a build-arg (--build-arg GIT_HASH=<sha>) and build.rs
# prefers the env var. Left unset, /health reports git_hash=unknown.
ARG GIT_HASH
ENV GIT_HASH=${GIT_HASH}

RUN cargo build --profile dist --locked -p orion-server

# Runtime stage. Named so `docker build --target` can address it, like the
# stages above (T23).
FROM debian:trixie-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd AS runtime

# OCI identity on the image itself (T23): docker-release.yml injects the full
# metadata-action label set at push time, but a local `docker build` — the
# command the docs give — used to produce an entirely unlabeled image. The
# dynamic pair (version, revision) still comes from CI; these are the static
# facts.
LABEL org.opencontainers.image.title="Orion" \
      org.opencontainers.image.description="Declarative services runtime: channels + workflows over REST/Kafka, single binary" \
      org.opencontainers.image.source="https://github.com/GoPlasmatic/Orion" \
      org.opencontainers.image.documentation="https://docs.goplasmatic.io/" \
      org.opencontainers.image.vendor="Plasmatic" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

# Pinned numeric UID/GID: Kubernetes' runAsNonRoot check cannot verify a
# named image USER, so the Helm chart (and any PodSecurity `restricted`
# namespace) needs a numeric identity. Keep 10001 in sync with the chart's
# default podSecurityContext (runAsUser/runAsGroup/fsGroup).
RUN groupadd --system --gid 10001 orion && useradd --system --uid 10001 --gid orion --no-create-home orion

WORKDIR /app
RUN mkdir -p /app/data && chown -R orion:orion /app

COPY --from=builder --chown=orion:orion /app/target/dist/orion-server /usr/local/bin/orion-server
COPY --chown=orion:orion crates/orion-server/config.toml.example /app/config.toml.example
# The image redistributes the Apache-2.0 binary; ship the license with it (P16).
COPY LICENSE /usr/share/doc/orion-server/LICENSE

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
