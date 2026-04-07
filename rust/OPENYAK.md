# OPENYAK.md

This file provides guidance to openyak when working with code in this repository.

## Detected stack
- Languages: Rust.
- Frameworks: none detected from the supported starter markers.

## Verification
- Run Rust verification from the `rust/` workspace root.
- Baseline: `cargo fmt --all --check`, `cargo build --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`
- Command surface: `cargo test -p openyak-cli --test command_surface_cli_smoke`, `cargo test -p openyak-cli --test doctor_cli_smoke`, `cargo test -p openyak-cli --test onboard_cli_smoke`, `cargo test -p openyak-cli --test package_release_cli_smoke`, `cargo test -p openyak-cli --test server_cli_smoke`, `cargo test -p openyak-cli --test mock_parity_harness`
- Docs-only updates still need a link/reference self-check; if command semantics changed, also verify `cargo run --bin openyak -- --help`, `cargo run --bin openyak -- skills --help`, and `cargo run --bin openyak -- server --help`.

## Working agreement
- Prefer small, reviewable changes and keep generated bootstrap files aligned with actual repo workflows.
- Keep shared defaults in `.openyak.json`; reserve `.openyak/settings.local.json` for machine-local overrides.
- Do not overwrite existing `OPENYAK.md` content automatically; update it intentionally when repo workflows change.
