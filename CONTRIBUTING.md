# Contributing to MerkurDB

Thanks for your interest in contributing!

## Getting Started

```bash
git clone git@github.com:TtTRz/MerkurDB.git
cd MerkurDB
cargo build --workspace
cargo test --workspace
```

## Development Workflow

1. Fork the repo and create a feature branch
2. Make your changes
3. Ensure all checks pass (these mirror the CI gates — run them on the latest
   stable toolchain, since CI uses `rust-toolchain@stable`):
   ```bash
   cargo fmt --check
   cargo build --workspace
   python3 scripts/test_mcp_e2e.py
   cargo test --workspace
   cargo build --workspace --all-features
   cargo test --workspace --all-features
   cargo clippy --workspace --all-features -- -D warnings
   cargo build --workspace --no-default-features
   cargo bench --workspace --no-run
   ```
4. Add tests for new functionality
5. Update docs if the API or config changes
6. Submit a PR

All-feature builds require `protoc` and a C/C++ toolchain. The real-server MCP
test also requires Python 3 and permission to bind loopback ports. It launches
the built server and MCP binaries with a temporary SQLite database and Noop
providers; it makes no external model calls. SDK and MCP HTTP tests also bind
loopback ports, so they must run outside a sandbox that forbids local listeners.

## Code Style

- Follow `rustfmt` defaults (`cargo fmt` before committing — CI enforces it)
- Run `cargo clippy --workspace --all-features -- -D warnings` — PRs with clippy warnings will not be merged
- Use `tracing` for logging, not `println!`
- Errors use `MerkurError` / `MerkurResult` from `merkur-core`
- New endpoints: add to router, handlers, OpenAPI spec, README API table, and tests

## Plugin Development

To add a new plugin (e.g., a new Embedder):

1. Implement the corresponding trait (`Embedder`, `Storage`, `Consolidator`, or `Forgetter`)
2. Add it to the relevant crate with appropriate feature gating
3. Wire it up in `main.rs` under the plugin match arm
4. Add config struct in `config.rs`
5. Document in `config.example.yaml`

## Commit Messages

- Use present tense, imperative mood ("Add feature" not "Added feature")
- Keep first line under 72 characters
- Reference issue numbers when applicable

## Questions?

Open a GitHub issue or discussion.
