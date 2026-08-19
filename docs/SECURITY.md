# Security

If you find a significant vulnerability, or evidence of one,
please report it privately.

We prefer that you use the [GitHub mechanism for privately reporting a vulnerability](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability#privately-reporting-a-security-vulnerability). Under the
[repository’s security tab](https://github.com/alltheplaces/osm-diffs/security), click "Report a vulnerability" to open the advisory form.

For how we secure the release process itself — build provenance, SBOMs, signed attestations — see [`SUPPLY_CHAIN_SECURITY.md`](https://github.com/alltheplaces/osm-diffs/blob/main/docs/SUPPLY_CHAIN_SECURITY.md).

We also run [SAST](https://en.wikipedia.org/wiki/Static_application_security_testing) via [GitHub CodeQL](https://codeql.github.com/) on every change, enforced by branch protection on `main` — see [`TESTING.md`](https://github.com/alltheplaces/osm-diffs/blob/main/docs/TESTING.md) for details.

We also publish an [OSSF Scorecard](https://scorecard.dev/viewer/?uri=github.com/alltheplaces/osm-diffs) analysis on every push to `main` (badge in the [README](https://github.com/alltheplaces/osm-diffs#readme)). Its "Vulnerabilities" check reports known, already-public advisories in our dependencies (RUSTSEC/OSV) — the same ones [Dependabot](https://github.com/alltheplaces/osm-diffs/network/updates) already opens PRs for. A nonzero score there isn't a new finding and doesn't need private disclosure; check [open pull requests](https://github.com/alltheplaces/osm-diffs/pulls) for the fix already in progress.

On top of that, [`cargo-deny.yml`](https://github.com/alltheplaces/osm-diffs/blob/main/.github/workflows/cargo-deny.yml) actively gates PRs and runs weekly, via [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) against the rules in [`deny.toml`](https://github.com/alltheplaces/osm-diffs/blob/main/deny.toml). It also checks dependency licenses, banned/duplicated crates, and untrusted sources — not just advisories. Advisories that are already known, already tracked, and not fixable from this repo (e.g. pinned deep in a dependency we don't control) can be allow-listed there with a reason and a link to the upstream fix, rather than leaving CI red on something no PR here can resolve.
