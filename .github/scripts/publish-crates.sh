#!/usr/bin/env bash
#
# Publish workspace crates to crates.io, skipping versions that are already
# live. Driven by .github/workflows/crates-publish.yml for both the rider
# crates and orion-server.
#
# Idempotency is the whole point: a re-run of the publish job, or a tag
# re-pushed after a downstream pipeline failed, must not fail trying to
# re-publish a version that is already live and unyankable-in-place.
#
# Two guards, because the first one is only advisory:
#
#  1. The sparse index (index.crates.io) is asked whether the version exists.
#     It is a CDN — no auth, no rate limit — unlike the crates.io JSON API this
#     replaced. The API is what broke the v1.1.0 release: it answered 429,
#     `curl -f` exited non-zero, the check read that as "not published yet",
#     and cargo then refused with `crate orion-api@1.0.0 already exists on
#     crates.io index`, failing the job before orion-server was ever published.
#     So an unreadable index is never treated as absence here: anything other
#     than 200/404 is retried, and a lookup that never resolves stops the
#     release rather than guessing in either direction.
#
#  2. cargo's own refusal is the authority. When it says the version already
#     exists, that is precisely the state the skip aims for — it is reported
#     and treated as success, which also covers a stale index read.

set -euo pipefail

UA="orion-release (github.com/GoPlasmatic/Orion)"

# Sparse-index path for a crate name: 1- and 2-character names live under `1/`
# and `2/`, 3-character under `3/{first}/`, everything longer under
# `{chars 1-2}/{chars 3-4}/`. See the cargo registry index specification.
index_path() {
  local crate="$1"
  case "${#crate}" in
    1) printf '1/%s' "$crate" ;;
    2) printf '2/%s' "$crate" ;;
    3) printf '3/%s/%s' "${crate:0:1}" "$crate" ;;
    *) printf '%s/%s/%s' "${crate:0:2}" "${crate:2:2}" "$crate" ;;
  esac
}

# 0 = the version is on the index, 1 = it is not, 2 = the index could not be read.
index_has_version() {
  local crate="$1" version="$2" url resp code body attempt
  url="https://index.crates.io/$(index_path "$crate")"

  for attempt in 1 2 3; do
    resp="$(curl -sS -w $'\n%{http_code}' --max-time 30 -A "$UA" "$url" || true)"
    code="${resp##*$'\n'}"
    body="${resp%$'\n'*}"
    case "$code" in
      200)
        # One JSON object per line; `vers` is the published version.
        if printf '%s' "$body" | grep -q "\"vers\":\"${version}\""; then
          return 0
        fi
        return 1
        ;;
      404) return 1 ;;  # the crate has never been published at all
      *)
        echo "  index lookup for ${crate} returned HTTP ${code:-none} (attempt ${attempt}/3)" >&2
        if [ "$attempt" -lt 3 ]; then
          sleep $((attempt * 10))
        fi
        ;;
    esac
  done
  return 2
}

publish_crate() {
  local crate="$1" version seen log status
  version="$(cargo pkgid -p "$crate" | sed 's/.*[@#]//')"

  set +e
  index_has_version "$crate" "$version"
  seen=$?
  set -e

  case "$seen" in
    0)
      echo "${crate} ${version} already on crates.io — skipping"
      return 0
      ;;
    2)
      echo "::error::could not read the crates.io index for ${crate} after 3 attempts; refusing to guess whether ${version} is published"
      return 1
      ;;
  esac

  echo "publishing ${crate} ${version}"
  log="$(mktemp)"
  set +e
  cargo publish --locked -p "$crate" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  set -e

  if [ "$status" -ne 0 ] && grep -q "already exists on crates.io index" "$log"; then
    echo "::notice::${crate} ${version} was already published — treating as success"
    status=0
  fi

  rm -f "$log"
  return "$status"
}

if [ "$#" -eq 0 ]; then
  echo "no crates to publish"
  exit 0
fi

for crate in "$@"; do
  publish_crate "$crate"
done
