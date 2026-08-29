#!/usr/bin/env bash
#
# Print the version of a workspace member, resolving workspace inheritance.
#
# The release workflows have to compare a tag against the version the binary
# will actually report, and they run before (and without) a cargo build, so
# they read the manifest. Every member now inherits `version.workspace = true`
# from the root `[workspace.package]`, so a bare `grep '^version = '` on a
# member manifest finds nothing — which read as an empty version and would have
# turned the tag/version check into a silent mismatch.
#
# A member may still pin a `version` of its own (that is how an out-of-band
# `orion-cli-vX.Y.Z` patch is cut between joint releases), so the pinned form
# is resolved first and the workspace value is the fallback.
#
# Usage: .github/scripts/crate-version.sh orion-server
set -euo pipefail

crate="${1:?usage: crate-version.sh <crate-name>}"
manifest="crates/${crate}/Cargo.toml"

[ -f "$manifest" ] || { echo "no manifest at $manifest" >&2; exit 1; }

line="$(grep -m1 '^version' "$manifest" || true)"
case "$line" in
  # `version.workspace = true` / `version = { workspace = true }`
  *workspace*) line="$(grep -m1 '^version = ' Cargo.toml || true)" ;;
esac

version="$(printf '%s' "$line" | cut -d'"' -f2)"
[ -n "$version" ] || { echo "could not read a version for $crate" >&2; exit 1; }
printf '%s\n' "$version"
