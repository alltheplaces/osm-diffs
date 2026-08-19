#!/usr/bin/env sh
#
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Cut a new release, end to end:
#   1. Bump Cargo.toml's version (and Cargo.lock's self-entry) on a new
#      branch, open a PR, enable auto-merge, and wait for it to clear
#      required checks and the merge queue.
#   2. Once that's landed on main, create the GitHub Release there (tag +
#      auto-generated notes, from the categories configured in
#      .github/release.yml).
#   3. Wait for the resulting .github/workflows/release.yml run (build,
#      SBOM, attest) and verify it actually came out right, via
#      verify-release.sh.
#
# Step 3 alone can take ~20-25 minutes; it's safe to Ctrl-C this script
# once the release has been created (end of step 2) and, if you do,
# finish verifying it later with:
#   ./scripts/verify-release.sh vX.Y.Z
#
# Usage:
#   ./scripts/cut-release.sh vX.Y.Z
#
# Version bump rules (SemVer, driven by the pipeline's OUTPUT SCHEMA, not
# just code changes):
#   major - output schema changed in a client-breaking way
#   minor - output schema evolved in a backward-compatible way
#   patch - bugfixes only, no schema changes
# Before asking for confirmation, this script scans merged PR titles since
# the last tag for a Conventional Commits "!" and suggests a version floor
# from that -- see docs/CONTRIBUTING.md and docs/RELEASING.md. It's a hint,
# not authoritative: the version is still your call.
#
# Requirements: git, cargo, gh (authenticated -- run `gh auth login` first)

set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

REQUIRED_CHECK="Execute unit and integration tests"
MERGE_WAIT_TIMEOUT=1500  # 25 min: ~5 min test run + merge-queue wait + slack
MERGE_POLL_INTERVAL=15

# ── argument handling ────────────────────────────────────────────────────────
TAG="${1:?usage: cut-release.sh vX.Y.Z}"
case "$TAG" in
  v*.*.*) VERSION="${TAG#v}" ;;
  *) echo "ERROR: tag must look like vX.Y.Z (got: $TAG)" >&2; exit 1 ;;
esac
case "$VERSION" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "ERROR: '$VERSION' doesn't look like X.Y.Z" >&2; exit 1 ;;
esac

# ── pre-flight checks ────────────────────────────────────────────────────────
for cmd in git cargo gh; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "ERROR: $cmd is not installed" >&2; exit 1; }
done
gh auth status >/dev/null 2>&1 || { echo "ERROR: gh is not authenticated -- run 'gh auth login'" >&2; exit 1; }

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" != "main" ]; then
  echo "ERROR: must be run from main (currently on '$BRANCH')" >&2
  exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "ERROR: working tree is not clean" >&2
  exit 1
fi

echo "Fetching origin..."
git fetch origin main --tags -q
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
  echo "ERROR: local main is not up to date with origin/main (run 'git pull')" >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  echo "ERROR: tag '${TAG}' already exists" >&2
  exit 1
fi

CURRENT_VERSION="$(awk '
  /^\[package\]/ { in_package=1; next }
  /^\[/           { in_package=0 }
  in_package && /^version[[:space:]]*=/ {
    match($0, /"[^"]*"/)
    print substr($0, RSTART+1, RLENGTH-2)
    exit
  }
' Cargo.toml)"
HIGHEST="$(printf '%s\n%s\n' "$CURRENT_VERSION" "$VERSION" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
if [ "$HIGHEST" != "$VERSION" ] || [ "$VERSION" = "$CURRENT_VERSION" ]; then
  echo "ERROR: ${VERSION} is not newer than Cargo.toml's current version (${CURRENT_VERSION})" >&2
  exit 1
fi

# ── scan for Conventional-Commits breaking markers since the last release ───
# This is a hint, not a rule: choosing the version is still your call (see
# "Choosing the version number" in docs/RELEASING.md). Reminder, since it's
# easy to reach for the usual meaning by reflex: on osm-diffs, "!" flags a
# commit that breaks the OUTPUT SCHEMA (conflated.parquet), not one that
# breaks some public API -- see docs/CONTRIBUTING.md.
LAST_TAG="$(git describe --tags --abbrev=0 2>/dev/null || true)"
if [ -n "$LAST_TAG" ]; then
  BREAKING_COMMITS="$(git log "${LAST_TAG}..HEAD" --format='%s' \
    | grep -E '^[a-z]+(\([a-zA-Z0-9_./-]+\))?!:' || true)"
  if [ -n "$BREAKING_COMMITS" ]; then
    echo "Breaking-change ('!') commits since ${LAST_TAG} -- remember, here"
    echo "'!' means OUTPUT-SCHEMA-breaking, not API-breaking (see"
    echo "docs/CONTRIBUTING.md):"
    echo "$BREAKING_COMMITS" | sed 's/^/  - /'
    CUR_MAJOR="${CURRENT_VERSION%%.*}"
    NEW_MAJOR="${VERSION%%.*}"
    if [ "$NEW_MAJOR" -le "$CUR_MAJOR" ]; then
      echo
      echo "WARNING: the above suggests a major bump, but ${TAG} isn't one."
      echo "  This is only a suggestion from PR titles, not a schema diff --"
      echo "  double-check against 'Choosing the version number' in"
      echo "  docs/RELEASING.md before continuing."
      printf "Continue with %s anyway? [y/N] " "$VERSION"
      read -r REPLY || REPLY=""
      case "$REPLY" in
        [yY]*) ;;
        *) echo "Aborted." >&2; exit 1 ;;
      esac
    fi
  fi
fi

echo "Checking latest CI status on main..."
LATEST_RUN="$(gh run list --branch main --workflow test.yml --limit 1 \
  --json headSha,status,conclusion -q '.[0]')"
RUN_SHA="$(echo "$LATEST_RUN" | jq -r .headSha)"
RUN_STATUS="$(echo "$LATEST_RUN" | jq -r .status)"
RUN_CONCLUSION="$(echo "$LATEST_RUN" | jq -r .conclusion)"
if [ "$RUN_SHA" != "$(git rev-parse HEAD)" ] || [ "$RUN_STATUS" != "completed" ] || [ "$RUN_CONCLUSION" != "success" ]; then
  echo "ERROR: latest '${REQUIRED_CHECK}' run on main is not a successful," >&2
  echo "  completed run for the current main HEAD (sha=${RUN_SHA}" >&2
  echo "  status=${RUN_STATUS} conclusion=${RUN_CONCLUSION}). Wait for CI" >&2
  echo "  to finish, or fix main, before cutting a release." >&2
  exit 1
fi

# ── bump the version via a PR (main is protected: no direct pushes) ─────────
BUMP_BRANCH="release/${TAG}"
echo "Creating branch ${BUMP_BRANCH}..."
git checkout -b "$BUMP_BRANCH" -q

awk -v new_version="$VERSION" '
  /^\[package\]/ { print; in_package=1; next }
  /^\[/           { in_package=0 }
  in_package && /^version[[:space:]]*=/ {
    print "version = \"" new_version "\""
    next
  }
  { print }
' Cargo.toml > Cargo.toml.new
mv Cargo.toml.new Cargo.toml
cargo check --quiet

git add Cargo.toml Cargo.lock
git commit -q -m "Bump version to ${VERSION}"
git push -q -u origin "$BUMP_BRANCH"

PR_URL="$(gh pr create \
  --title "Bump version to ${VERSION}" \
  --body "Version bump ahead of releasing ${TAG}. Opened by scripts/cut-release.sh." \
  --base main)"
echo "Opened ${PR_URL}"

gh pr merge "$PR_URL" --auto --squash
echo "Auto-merge enabled; waiting for '${REQUIRED_CHECK}' and the merge queue..."

ELAPSED=0
while :; do
  STATE="$(gh pr view "$PR_URL" --json state -q .state)"
  case "$STATE" in
    MERGED)
      echo "Merged."
      break
      ;;
    CLOSED)
      echo "ERROR: PR was closed without merging: ${PR_URL}" >&2
      exit 1
      ;;
  esac
  if [ "$ELAPSED" -ge "$MERGE_WAIT_TIMEOUT" ]; then
    echo "ERROR: timed out after ${MERGE_WAIT_TIMEOUT}s waiting for ${PR_URL} to merge." >&2
    echo "  Check its status manually; it may still merge on its own." >&2
    exit 1
  fi
  sleep "$MERGE_POLL_INTERVAL"
  ELAPSED=$((ELAPSED + MERGE_POLL_INTERVAL))
done

git checkout main -q
git branch -D "$BUMP_BRANCH" -q

# ── create the release (tag + auto-generated notes) at the new main HEAD ────
echo "Creating release ${TAG}..."
gh release create "$TAG" --target main --generate-notes

# ── wait for release.yml and verify the result ───────────────────────────────
echo "${TAG} is tagged and released; waiting for .github/workflows/release.yml" \
     "to build, SBOM, and attest the container, then verifying the result." \
     "This can take ~20-25 minutes; safe to Ctrl-C and finish later with" \
     "'./scripts/verify-release.sh ${TAG}'."
"${SCRIPT_DIR}/verify-release.sh" "$TAG"

echo "Done."
