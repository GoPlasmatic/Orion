# Shared toolchain stage: cargo-chef is compiled from source ONCE here and
# inherited by both stages below — it used to be built independently in each,
# doubling the cold-cache cost for the same binary (T23).
# P18: digest-pinned (multi-arch manifest list) so a rebuild months from now
# resolves the same base layers; dependabot bumps the tag+digest pair.
FROM rust:1.98-slim@sha256:17d1ba895198f9934c6314ec5346a0d5115372f3243390c3d731e242f35c2f27 AS chef
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

# Codegen knobs for the `dist` profile, defaulted to exactly what
# [profile.dist] in the workspace Cargo.toml already says (release's
# codegen-units = 1, plus thin LTO). A plain `docker build` is therefore
# unchanged; overriding them is opt-in.
#
# They exist for ci.yml's docker-build, which builds this image on every push
# only to prove that it *builds* — the system deps, the chef stages, the
# runtime stage. Proving that does not need the slowest codegen settings Rust
# offers, and that build was 14.3 minutes (855s, measured 2026-08-29). CI
# passes CARGO_PROFILE_DIST_CODEGEN_UNITS=16 and CARGO_PROFILE_DIST_LTO=false;
# release builds pass nothing and get the real thing.
#
# Cargo reads these as profile overrides for any invocation, so they must be
# set before `cook` as well as the build: cooking dependencies under one
# profile and building under another makes cargo rebuild every dependency and
# defeats the chef cache entirely.
ARG CARGO_PROFILE_DIST_CODEGEN_UNITS=1
ARG CARGO_PROFILE_DIST_LTO=thin
ENV CARGO_PROFILE_DIST_CODEGEN_UNITS=${CARGO_PROFILE_DIST_CODEGEN_UNITS} \
    CARGO_PROFILE_DIST_LTO=${CARGO_PROFILE_DIST_LTO}

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
FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132 AS runtime

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
