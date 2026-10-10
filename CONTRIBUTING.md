# Contributing

Thanks for improving Terminal Time Machine. Bug reports, documentation improvements, tests, and focused pull requests are welcome.

## Before Opening A Pull Request

1. Open an issue first for substantial behavior changes.
2. Keep each pull request focused on one problem.
3. Preserve the restore safety guarantees and update the README for user-visible changes.
4. Add a regression test for each bug fix.

## Local Checks

Run the same checks as CI before submitting:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --locked --release
```

The project deliberately has no third-party crates. Discuss any new dependency in an issue before adding it.

## Commit Messages

Use an imperative, specific subject such as `Reject conflicting manifest paths` or `Document portable store setup`.

## Security Issues

Do not file public issues for data-loss, path-traversal, or unsafe-restore vulnerabilities. Follow [SECURITY.md](SECURITY.md) instead.
