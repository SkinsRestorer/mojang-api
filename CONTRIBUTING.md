# Contribute to mojang-api

Contributions can fix behavior, improve documentation, or add focused tests.
The [README](README.md) documents runtime configuration. A release build uses `cargo build --release --locked`.

## Before you start

Read [the support guide](SUPPORT.md) for questions and issue routing.
Search existing issues and pull requests. Discuss larger API, architecture, or dependency changes before implementation.

Work from `main` and target that branch in your pull request.
Keep each change focused. Avoid unrelated formatting and dependency updates.

## Prepare a checkout

Install Rust through Rustup. It installs the version pinned in `rust-toolchain.toml`. CI also checks that the code compiles with the minimum version in `Cargo.toml`. The lockfile pins dependencies.

Install [Lefthook](https://lefthook.dev/install/) 1.13.6 or newer, then run `lefthook install` once per checkout.
The pre-commit hook checks staged whitespace and Rust formatting. When Rust, Cargo, or toolchain files change, the pre-push hook runs Clippy and the tests.
Install cargo-deny 0.20.2 for the pre-push hook. It runs `cargo deny --locked check` when `Cargo.toml`, `Cargo.lock`, or `deny.toml` changes.
The pre-push hook compares `HEAD` with the branch's upstream. Keep the upstream set to the branch that you push.

## Verify your change

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
```

Cover cache expiry, batching, rate limits, and invalid names/UUIDs with focused tests. Use deterministic local fixtures for upstream behavior. Keep proxy credentials and webhook URLs out of reports. Check the health endpoint before live upstream requests.

Run the relevant checks before review. State the command and result in the pull request.
If a check cannot run, explain the missing dependency or service. Do not claim it passed.

## Style and documentation

Follow the existing code conventions and repository formatter. Keep commit hooks enabled.
Add focused tests for changed logic when practical. Avoid tests that only assert source strings.
Update documentation when commands, APIs, configuration, or expected behavior change.
Keep examples small and reproducible. Preserve exact identifiers, commands, and error messages.

## Open a pull request

Explain the problem and resulting behavior. Link related issues without a placeholder issue number.
Describe API, configuration, or Discord report changes. State whether a live upstream or Discord check was required.
Include commands and results. State any runtime checks that remain necessary.
Respond to review with a correction or concrete evidence.

Use Conventional Commits: `type(scope): description`, for example `docs(contributing): explain local validation`.
Use a meaningful scope, or omit it. Keep the subject concise and imperative.
Add a body when the reason or compatibility impact is not obvious.

For vulnerabilities, follow [the security reporting instructions](SECURITY.md).
Remove credentials and private data from examples, logs, and screenshots.
