# Mock LLM parity harness

This milestone adds a deterministic Anthropic-compatible mock service plus a reproducible CLI harness for the Rust `openyak` binary.

## Artifacts

- `crates/mock-anthropic-service/` — mock `/v1/messages` service
- `crates/openyak-cli/tests/mock_parity_harness.rs` — end-to-end clean-environment harness
- `scripts/run_mock_parity_harness.sh` — convenience wrapper

## Scenarios

The harness runs these scripted scenarios against a fresh workspace and isolated environment variables:

1. `streaming_text`
2. `read_file_roundtrip`
3. `grep_chunk_assembly`
4. `write_file_allowed`
5. `write_file_denied`
6. `multi_tool_turn_roundtrip`
7. `plugin_tool_roundtrip`

## Run

The commands below assume your current working directory is `rust/`.

Cross-platform direct run:

```bash
cargo test -p openyak-cli --test mock_parity_harness -- --nocapture
```

Convenience wrapper (Bash only):

```bash
./scripts/run_mock_parity_harness.sh
```

The wrapper currently runs:

```bash
cargo test -p openyak-cli --test mock_parity_harness -- --nocapture
```

Recommended parity verification bundle:

```bash
cargo test -p mock-anthropic-service
cargo test -p openyak-cli --test mock_parity_harness -- --nocapture
python scripts/run_mock_parity_diff.py
```

Behavioral checklist / parity diff:

```bash
python scripts/run_mock_parity_diff.py
```

Scenario manifests and checklist mappings live in `mock_parity_scenarios.json`.

## Manual mock server

```bash
cargo run -p mock-anthropic-service -- --bind 127.0.0.1:0
```

The server prints `MOCK_ANTHROPIC_BASE_URL=...`; point `ANTHROPIC_BASE_URL` at that URL and use any non-empty `ANTHROPIC_API_KEY`. Use `Ctrl-C` to stop it cleanly after manual checks.
