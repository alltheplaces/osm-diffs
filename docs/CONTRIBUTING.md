# Contributing

Thanks for taking a look at `osm-diffs`! 👋 Contributions, questions,
and bug reports are all welcome — this project is still early, so
there's no such thing as too small a
[Pull Request (PR)](https://docs.github.com/en/pull-requests/get-started/about-pull-requests).

Please be kind to each other and to maintainers; see our
[Code of Conduct](CODE_OF_CONDUCT.md).

## Before opening a Pull Request

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

These match what CI runs on every PR (see
[`.github/workflows/test.yml`](../.github/workflows/test.yml) for the
exact invocations, including a minimum test-coverage threshold `cargo
test` alone doesn't check). Running them locally first saves a
round-trip through CI.

## Where to go next

- [`docs/TESTING.md`](TESTING.md) — how this project's tests are
  organized, and how to try a change on real full-planet data before
  it lands.
- [`docs/RELEASING.md`](RELEASING.md) — how a release actually ships,
  for once your change has landed on `main`.

That's it — open a PR, and we'll take it from there. 🙂
