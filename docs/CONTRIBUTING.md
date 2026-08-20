# Contributing

Thanks for taking a look at `osm-diffs`! 👋 Contributions, questions,
and bug reports are all welcome — this project is still early, so
there's no such thing as too small a
[Pull Request (PR)](https://docs.github.com/en/pull-requests/get-started/about-pull-requests).

Please be kind to each other and to maintainers; see our
[Code of Conduct](https://github.com/alltheplaces/osm-diffs/blob/main/docs/CODE_OF_CONDUCT.md).

## Before opening a Pull Request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

If your PR touches `Cargo.toml` or `Cargo.lock` (e.g. adding or bumping a
dependency), also run
[`cargo deny`](https://github.com/EmbarkStudios/cargo-deny) (`cargo install
cargo-deny` if you don't have it):

```sh
cargo deny check
```

This checks the new dependency graph against
[`deny.toml`](https://github.com/alltheplaces/osm-diffs/blob/main/deny.toml)
— disallowed licenses, banned/duplicated crates, untrusted sources, and
known RUSTSEC/OSV advisories.

If you changed a Python script, run `uv run pytest` from `scripts/`.

After you've sent a Pull Request, our Continuous Integration (CI) runs a
series of checks on it — the commands above are a subset of what CI runs
([`test.yml`](https://github.com/alltheplaces/osm-diffs/blob/main/.github/workflows/test.yml),
[`cargo-deny.yml`](https://github.com/alltheplaces/osm-diffs/blob/main/.github/workflows/cargo-deny.yml)).
Running them locally first saves a round-trip through CI — see
[`TESTING.md`](https://github.com/alltheplaces/osm-diffs/blob/main/docs/TESTING.md)
for what else CI checks.

## PR titles: Conventional Commits

PRs are squash-merged, and the PR title becomes the commit message on
`main` — so give it the shape of a
[Conventional Commit](https://www.conventionalcommits.org/):

```
<type>[optional scope][!]: <description>
```

`type` is one of `feat`, `fix`, `docs`, `style`, `refactor`, `perf`,
`test`, `build`, `ci`, or `chore`. A bot
([`pr-title-lint.yml`](https://github.com/alltheplaces/osm-diffs/blob/main/.github/workflows/pr-title-lint.yml))
checks this on every PR and applies an `enhancement`/`bug`/`breaking-change`
label from it, which feeds
[`.github/release.yml`](https://github.com/alltheplaces/osm-diffs/blob/main/.github/release.yml)'s
categorized release notes — so getting the type right saves the
maintainer a manual labeling step, it's not just a style nit.

**The `!` marker is special here.** Normally it means "breaks the public
API." `osm-diffs` doesn't have one client code links against — what it
has is an **output schema** (`conflated.parquet`), and that's what
matters to everyone downstream. So on this project, `!` means *this PR
breaks the output schema*, per the rules in
[`RELEASING.md`](https://github.com/alltheplaces/osm-diffs/blob/main/docs/RELEASING.md#choosing-the-version-number)
— not that some function signature changed. A `refactor!:` or `chore!:`
that happens to rename a Parquet column is exactly as `!` as a `feat!:`
that removes one; a `feat:` CLI flag that doesn't touch the schema isn't
`!` at all. `cut-release.sh` reads these markers to *suggest* a version
floor, but the actual version is still your call, per `RELEASING.md`.

## Where to go next

- [`docs/TESTING.md`](https://github.com/alltheplaces/osm-diffs/blob/main/docs/TESTING.md)
  — how this project's tests are organized, and how to try a change on
  real full-planet data before it lands.
- [`docs/RELEASING.md`](https://github.com/alltheplaces/osm-diffs/blob/main/docs/RELEASING.md)
  — how a release actually ships, for once your change has landed on
  `main`.

That's it — open a PR, and we'll take it from there. 🙂
