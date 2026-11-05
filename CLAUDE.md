# kiwi-nest

Hosting for single-instance apps on a small cluster: one Rust binary that is the server, the node agent and the CLI.

The design and the build plan are in [docs/design.md](docs/design.md). Read it before changing anything, and update
it when the design changes.

kiwi-nest is written from scratch; don't copy from or refer to other deployment tools.

## Build and test

- `make check` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` (what CI runs).
- `make integration` also runs the tests that need Docker (`KIWI_NEST_INTEGRATION=1`).
- `make release` builds static binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` into
  `dist/`, with `cargo zigbuild` (needs `zig` and `cargo-zigbuild`).
- `cargo run -- version` prints the version (from the workspace `Cargo.toml`) and the git commit.

To try it by hand:

```sh
cargo run -- node --standalone --state-dir /tmp/kn &   # prints: listening on http://127.0.0.1:7380
cargo run -- deploy blog ./path/to/bundle
cargo run -- list
```

## Layout

- `crates/kiwi-nest`: the binary; `clap` picks `node`, `version` or a CLI command. End-to-end tests in `tests/`.
- `crates/kn-proto`: API types shared by the CLI, server and nodes, and the version.
- `crates/kn-bundle`: safe ZIP and directory reading, the Compose subset, `x-kiwi-nest`, the generated Compose file.
  No I/O beyond the bundle, so plain unit tests cover it.
- `crates/kn-node`: `runtime` (`docker compose`), `apps` (one task per app, the state file), `api` (standalone API).
- `crates/kn-cli`: the CLI commands.

## Conventions

- Integration tests that need Docker (or, later, btrfs) are opt-in through `KIWI_NEST_INTEGRATION=1`, use a random
  Compose project prefix, and clean up only what they made.
- Every rule the bundle checker enforces has a unit test.
