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
3. Ensure all checks pass:
   ```bash
   cargo build --workspace
   cargo test --workspace
   cargo fmt --check
   cargo clippy --workspace --all-features -- -D warnings
   ```
4. Add tests for new functionality
5. Update docs if the API or config changes
6. Submit a PR

## Code Style

- Follow `rustfmt` defaults
- Run `cargo clippy -- -D warnings` — PRs with clippy warnings will not be merged
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
