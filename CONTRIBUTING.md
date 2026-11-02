# Contributing to Search CLI

Thanks for your interest in contributing. Here's how to get started.

## Setup

```bash
git clone https://github.com/paperfoot/search-cli
cd search-cli
cargo build
cargo test
```

Requires a current stable Rust. On Linux, build with `--no-default-features` unless you have the BoringSSL build tools (`cmake clang libclang-dev perl pkg-config`) for the `stealth` feature.

## Making Changes

1. Fork the repo and create a branch from `master`
2. Make your changes
3. Run the checks CI runs: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`
4. Open a pull request with a clear description of what you changed and why

## Docs and the Claude plugins

- `SKILL.md` is embedded in the binary (`search skill install`) and copied byte for byte to `plugins/search-cli/skills/search/SKILL.md`. Change both: `cp SKILL.md plugins/search-cli/skills/search/SKILL.md`.
- `claude plugin validate .` checks the marketplace; `claude plugin validate plugins/<name>` checks a plugin.
- The mode and provider tables in `README.md` and `SKILL.md` follow `src/registry.rs`; `search agent-info` prints the same data.

## What We're Looking For

- New search provider integrations
- Bug fixes and performance improvements
- Better error messages and documentation
- Test coverage improvements

## Code Style

- Follow existing patterns in the codebase
- Keep functions focused and small
- Add tests for new functionality
- Use meaningful variable names

## Reporting Issues

Open a GitHub issue with:
- What you expected to happen
- What actually happened
- Steps to reproduce
- Your OS and Rust version

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
