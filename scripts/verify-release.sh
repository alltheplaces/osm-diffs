#!/usr/bin/env sh
#
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Verify that a release actually came out right, not just that
# `.github/workflows/release.yml` reported success:
#   - waits for (or, if it's already done, immediately checks) that
#     workflow's run for the given tag to complete successfully
#   - confirms both a build-provenance and an SBOM attestation exist for
#     each per-architecture image (see docs/SUPPLY_CHAIN_SECURITY.md for
#     what these are and why both should exist)
#
# Usage:
#   ./scripts/verify-release.sh vX.Y.Z
#
# This is called automatically as the last step of cut-release.sh, but is
# also safe to run standalone, any time after a tag was pushed -- e.g. if
# cut-release.sh didn't get to finish (the machine running it crashed,
# the connection dropped, ...), or to double-check an older release.
#
# Requirements: gh (authenticated -- run `gh auth login` first), jq

set -eu

RUN_WAIT_TIMEOUT=2700  # 45 min: ~20 min build+manifest+attest, plus slack
RUN_POLL_INTERVAL=20
RUN_LIST_LIMIT=50  # generous, so this still finds older releases

TAG="${1:?usage: verify-release.sh vX.Y.Z}"

for cmd in gh jq; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "ERROR: $cmd is not installed" >&2; exit 1; }
done
gh auth status >/dev/null 2>&1 || { echo "ERROR: gh is not authenticated -- run 'gh auth login'" >&2; exit 1; }

REPO="$(gh repo view --json nameWithOwner -q .nameWithOwner)"

# ── wait for (or check) release.yml's run for this tag ──────────────────────
echo "Looking for release.yml's run for ${TAG}..."
ELAPSED=0
RUN_ID=""
CONCLUSION=""
while :; do
  RUN_JSON="$(gh run list --workflow=release.yml --limit "$RUN_LIST_LIMIT" \
    --json databaseId,headBranch,status,conclusion \
    -q "[.[] | select(.headBranch == \"${TAG}\")] | .[0] // empty")"
  if [ -n "$RUN_JSON" ]; then
    RUN_ID="$(echo "$RUN_JSON" | jq -r .databaseId)"
    STATUS="$(echo "$RUN_JSON" | jq -r .status)"
    CONCLUSION="$(echo "$RUN_JSON" | jq -r .conclusion)"
    [ "$STATUS" = "completed" ] && break
  fi
  if [ "$ELAPSED" -ge "$RUN_WAIT_TIMEOUT" ]; then
    echo "ERROR: timed out after ${RUN_WAIT_TIMEOUT}s waiting for release.yml" >&2
    echo "  to finish for ${TAG}. Check its status manually and re-run this" >&2
    echo "  script once it's done." >&2
    exit 1
  fi
  sleep "$RUN_POLL_INTERVAL"
  ELAPSED=$((ELAPSED + RUN_POLL_INTERVAL))
done

if [ "$CONCLUSION" != "success" ]; then
  echo "ERROR: release.yml's run for ${TAG} did not succeed (conclusion=${CONCLUSION})." >&2
  echo "  https://github.com/${REPO}/actions/runs/${RUN_ID}" >&2
  exit 1
fi
echo "release.yml succeeded: https://github.com/${REPO}/actions/runs/${RUN_ID}"

# ── fetch the per-architecture digests from that run's artifacts ────────────
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

gh run download "$RUN_ID" -n artifacts-amd64 -D "${WORKDIR}/amd64" >/dev/null
gh run download "$RUN_ID" -n artifacts-arm64 -D "${WORKDIR}/arm64" >/dev/null
AMD64_DIGEST="$(cat "${WORKDIR}/amd64/digest.txt")"
ARM64_DIGEST="$(cat "${WORKDIR}/arm64/digest.txt")"

# ── confirm both attestation types exist for each digest ────────────────────
# Note: `gh attestation verify`'s default --predicate-type filter only shows
# build provenance, hiding the SBOM attestation; query the raw API instead.
check_attestations() {
  arch="$1"; digest="$2"
  types="$(gh api "repos/${REPO}/attestations/${digest}" \
      -q '.attestations[].bundle.dsseEnvelope.payload' \
    | while read -r p; do echo "$p" | base64 -d | jq -r .predicateType; done \
    | sort -u)"
  echo "${arch} (${digest}):"
  echo "$types" | sed 's/^/  /'
  echo "$types" | grep -qx 'https://slsa.dev/provenance/v1' \
    || { echo "ERROR: missing build-provenance attestation for ${arch}" >&2; exit 1; }
  echo "$types" | grep -qx 'https://cyclonedx.org/bom' \
    || { echo "ERROR: missing SBOM attestation for ${arch}" >&2; exit 1; }
}

echo "Checking attestations..."
check_attestations amd64 "$AMD64_DIGEST"
check_attestations arm64 "$ARM64_DIGEST"

echo "${TAG} verified: release.yml succeeded, and both SBOM and" \
     "build-provenance attestations exist for both architectures."
