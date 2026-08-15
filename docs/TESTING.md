# Testing

## Running Tests

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the commands to run before
opening a PR (formatting, linting, tests) — not repeated here to avoid
two copies going stale independently.

## Test Structure

- **Unit tests**: in source files, as `#[cfg(test)]` modules (standard
  Rust pattern).
- **Integration tests**: [`tests/`](../tests) — a full pipeline run
  against a real OSM extract (a Swiss shopping mall) plus minimal
  AllThePlaces data. Fast enough to run on every `cargo test`.

## CI

Every PR and push to `main` runs
[`test.yml`](../.github/workflows/test.yml) (formatting, linting,
tests, and a minimum coverage threshold) and
[`codeql.yml`](../.github/workflows/codeql.yml) (static analysis).
PRs that touch `Containerfile` or `scripts/sbom/` also run
[`test-container.yml`](../.github/workflows/test-container.yml).

Test coverage is measured with [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov)
(`cargo llvm-cov test`, using `rustc`'s built-in LLVM source-based
coverage instrumentation), exported as
[Cobertura XML](https://www.baeldung.com/cobertura) — originally a
Java coverage tool's report format, now emitted by plenty of non-Java
tools too, `cargo-llvm-cov` among them — and uploaded to
[Coveralls](https://coveralls.io/github/alltheplaces/osm-diffs?branch=main)
— that's the little coverage badge at the top of the main
[`README.md`](../README.md). CI fails if line coverage drops below the
threshold set in `test.yml`.

[SAST](https://en.wikipedia.org/wiki/Static_application_security_testing)
is handled by [GitHub CodeQL](https://codeql.github.com/)
(`codeql.yml`), which builds a semantic model of the code and queries
it for known vulnerability patterns — unsafe deserialization, command
injection, that kind of thing — rather than just linting for style.
On a PR, findings post directly on the review thread.
`codeql.yml` also runs on a weekly schedule against `main`, independent
of any PR or push; *those* findings don't have a PR to comment on, so
they show up instead as code scanning alerts on the repository's
[Security tab](https://github.com/alltheplaces/osm-diffs/security).

`main` is protected by a
[ruleset](https://github.com/alltheplaces/osm-diffs/rules/11597145)
requiring `test.yml`'s tests and a clean CodeQL scan before merging —
so a red check there isn't optional. `test-container.yml` isn't part
of that ruleset (it only triggers on the paths above, and GitHub can't
gate merges on a check that doesn't always run) — treat a failure
there as seriously as any other, it just isn't enforced the same way.

## For Large System Changes

Neither of these runs in CI — both are for ad hoc validation against
real hardware before a change lands, not automated gates:

- [`scripts/test-branch-on-macos/`](../scripts/test-branch-on-macos) —
  full pipeline on your dev machine. Free, fast to iterate with.
- [`scripts/test-branch-on-hetzner/`](../scripts/test-branch-on-hetzner)
  — full pipeline on real cloud hardware, built from your feature
  branch. Costs real money and needs Hetzner credentials — see its
  README before reaching for it.
