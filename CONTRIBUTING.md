# Contributing to Strata

Thanks for your interest in contributing!

## Quick Start

### Recommended Workflow

We strongly recommend using the Dev Container to get a clean, reproducible environment:

1. Install the Dev Containers extension for VS Code
2. Open the repo and choose "Reopen in Container"
3. Wait for the build to complete
4. Run `make install-hooks` to set up git hooks
5. Run `make check` to verify everything works

The dev container includes Rust, GStreamer, Meson, Clang, and all required tooling.

### Git Hooks (Important!)

**The git hooks will automatically catch most issues before you push.** They run:

- **Pre-commit**: Format, compilation check, clippy
- **Pre-push**: All of the above + unit tests

To install:
```bash
make install-hooks
```

The hooks are already configured, but if you're seeing issues, re-run the install command.

## Development Commands

We provide a Makefile with common tasks:

```bash
make check          # Fast compilation check
make fmt            # Format code
make lint           # Run clippy
make test           # Run unit tests
make pre-push       # Run all pre-push checks locally
make release-check  # Full release verification
make help           # Show all commands
```

**Before opening a PR, run:**
```bash
make pre-push
```

This catches the same issues that CI will catch, saving you time on failed builds.

## Development Tips

- Run `make fmt` and `make lint` frequently during development
- Run `make test` before pushing to catch test failures early
- If you need to skip hooks temporarily: `git push --no-verify` (but please don't!)
- Unit tests: `cargo test --workspace --lib`
- Integration tests require `NET_ADMIN` (use the dev container or `sudo`)

## Pull Requests

- Keep changes focused and scoped to one feature or fix
- Include tests when possible
- Update docs if behavior or configuration changes
- The CI will run automatically, but prefer catching issues locally with `make pre-push`

## Version Management

- Core platform crates (common, control, agent, dashboard, portal, sim) typically share a version
- strata-gst (GStreamer plugin) has its own version for releases
- strata-transport (networking layer) has its own version
- Run `make version-check` to see current versions

## License

By contributing, you agree that your contributions are licensed under the
LGPL-2.1-or-later license.
