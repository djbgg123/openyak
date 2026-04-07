# Contributing to openyak

Thanks for contributing to `openyak`.

## Start with the right surface

This repository has two different roles:

- `rust/` is the primary maintained product surface.
- root `src/` and `tests/` are comparison, audit, and migration-support layers.

If you are changing user-facing CLI/runtime behavior, start in `rust/`.

## Read before changing code

- Repository overview: [`README.md`](./README.md)
- Repository contract: [`OPENYAK.md`](./OPENYAK.md)
- Rust workspace guide: [`rust/README.md`](./rust/README.md)
- Rust contribution guide: [`rust/CONTRIBUTING.md`](./rust/CONTRIBUTING.md)
- Security reporting expectations: [`SECURITY.md`](./SECURITY.md)
- Conduct expectations: [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)

## Minimum contribution bar

- Keep claims grounded in current behavior and fresh verification.
- Prefer small, reviewable diffs over broad speculative rewrites.
- Update docs when behavior, prerequisites, or public boundaries change.
- Keep local-state files, caches, and build outputs out of commits.
- Mirror the GitHub Actions checks locally before opening a PR.

## Verification baseline

The public CI baseline lives in [`.github/workflows/ci.yml`](./.github/workflows/ci.yml). It mirrors the current supported local checks across:

- Rust workspace verification
- root Python comparison-layer tests
- Python SDK checks
- TypeScript SDK checks

For the exact commands and repo-specific caveats, use [`rust/CONTRIBUTING.md`](./rust/CONTRIBUTING.md) as the source of truth.
