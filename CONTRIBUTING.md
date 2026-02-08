# Contributing to Strata

Thanks for your interest in contributing!

## Recommended Workflow

We strongly recommend using the Dev Container to get a clean, reproducible environment:

1. Install the Dev Containers extension for VS Code
2. Open the repo and choose "Reopen in Container"
3. Wait for the build to complete
4. Run `cargo build`

The dev container includes Rust, GStreamer, Meson, Clang, and all required tooling.

## Development Tips

- Run `cargo fmt` and `cargo clippy --workspace --all-targets` before opening a PR.
- Unit tests: `cargo test --workspace --lib`
- Integration tests require `NET_ADMIN` (use the dev container or `sudo`).

## Pull Requests

- Keep changes focused and scoped to one feature or fix.
- Include tests when possible.
- Update docs if behavior or configuration changes.

## License

By contributing, you agree that your contributions are licensed under the
LGPL-2.1-or-later license.
