# Summary

<!-- One or two sentences. What does this PR change, and why? -->

## Type of change

- [ ] feat (new user-facing feature)
- [ ] fix (bug fix)
- [ ] perf (performance — include numbers below)
- [ ] refactor (no behaviour change)
- [ ] docs
- [ ] test
- [ ] chore (build / CI / tooling)

## Checklist

- [ ] Tests added or updated; `cargo test` passes locally.
- [ ] `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` are clean.
- [ ] User-facing changes reflected in `README.md`.
- [ ] Architectural changes reflected in `ARCHITECTURE.md`.
- [ ] Commit subjects follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `perf:`, …) — `release-please` parses these to bump the version automatically.

## Performance impact

<!--
For perf PRs (or anything that touches src/core/): paste the relevant
output of `bash scripts/bench_reddit.sh` before and after. Skip this
section for non-perf changes.
-->

## Notes for the reviewer

<!-- Anything non-obvious — design trade-offs, alternatives you rejected, follow-ups you're aware of. -->
