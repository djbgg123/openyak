# Security Policy

`openyak` is a local-first coding-agent project. The current public repository includes a real CLI, a local `/v1/threads` server surface, plugin and skill infrastructure, and alpha SDKs. Security expectations should stay aligned with those current boundaries rather than assuming a hosted SaaS threat model.

## Supported scope

Please report security issues that affect the currently shipped repository surfaces, especially:

- the Rust CLI and runtime under `rust/`
- the local server surface exposed by `openyak server`
- plugin/skill loading and path-boundary enforcement
- credential handling, OAuth configuration, and local secret storage behavior
- Python or TypeScript SDK handling of the documented local protocol

Out-of-scope or lower-priority reports include:

- hypothetical hosted-control-plane issues for infrastructure this repository does not operate
- live-provider or third-party service behavior outside the repository's code
- feature requests framed as vulnerabilities without a demonstrated security impact

## Reporting

Use the most private GitHub reporting path available to you for this repository.

- Preferred: GitHub's private vulnerability reporting or security advisory flow, if GitHub shows that option for this repository.
- Fallback: if no private GitHub path is available, open a minimal public issue requesting a private follow-up path and do **not** include exploit details, secrets, tokens, or reproduction material that would expose users.

## What to include

Please include:

- the affected file, command, or surface
- the security impact
- clear reproduction steps
- whether the issue depends on a specific OS, shell, or credential setup
- any suggested fix or mitigation, if known

## Response expectations

This repository is maintained on a best-effort basis. The goal is to acknowledge and triage credible reports, but response times are not guaranteed.

## Safe disclosure expectations

- Do not publish secrets, private tokens, or exploit payloads in public issues.
- Give maintainers a reasonable chance to triage and patch the issue before broad public disclosure.
- If the issue is already public, still keep new reports focused and minimally disclosive.
