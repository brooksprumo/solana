# Agave Agent Notes

Start with `./.repo-context/workspace.md` if it exists. If it is missing or stale, run `scripts/build-repo-context.sh` from the repo root to regenerate the local cache.

If you are working in `accounts-db`, start with `./.repo-context/focus/accounts-db.md` and `./.repo-context/flows/accounts-db.md` before reading source files.

High-signal crates:
- `validator`: binary entrypoint, CLI parsing, validator lifecycle wiring
- `core`: validator services, stage orchestration, replay/banking coordination
- `runtime`: bank, account state, execution coordination
- `ledger`: blockstore, shred handling, replay inputs
- `rpc`: JSON-RPC surface and request plumbing
- `gossip` and `turbine`: cluster networking and data plane
- `svm`: execution boundary
- `accounts-db` and `snapshots`: persisted state and snapshot handling

Recommended workflow:
- Use `cargo metadata --format-version 1 --no-deps` for authoritative workspace structure.
- Prefer crate-scoped commands such as `cargo test -p solana-runtime`.
- Never run `cargo fmt` in this repo.
- Treat `./.repo-context/` as a local cache. Refresh it after manifest changes or major refactors.
