# kiwi-nest

Hosting for single-instance apps on a small cluster: one Rust binary that is the server, the node agent and the CLI.

The design and the build plan are in [docs/design.md](docs/design.md). Read it before changing anything, and update
it when the design changes.

kiwi-nest is written from scratch; don't copy from or refer to other deployment tools.

## Build and test

- `make check` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` (what CI runs).
- `make integration` also runs the tests that need Docker (`KIWI_NEST_INTEGRATION=1`).
- Tests that need btrfs (every test that starts a node, since its volumes are subvolumes) run when
  `KIWI_NEST_TEST_BTRFS_DIR` names a directory on btrfs, and are skipped otherwise. `make test-btrfs`
  (`scripts/test-btrfs.sh`, needs sudo) loop-mounts one at `/mnt/kiwi-nest-test`, as CI does; any directory on a
  btrfs filesystem you own works too. Without root, tests destroy subvolumes by emptying them and using `rmdir`.
- `make release` builds static binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` into
  `dist/`, with `cargo zigbuild` (needs `zig` and `cargo-zigbuild`).
- `cargo run -- version` prints the version (from the workspace `Cargo.toml`) and the git commit.

To try it by hand:

```sh
# The volumes directory must be on btrfs. Prints: listening on http://127.0.0.1:7380
cargo run -- node --standalone --state-dir /tmp/kn --volumes-dir /mnt/kiwi-nest-test/kn &
cargo run -- deploy blog ./path/to/bundle
cargo run -- deploy blog-pr1 ./path/to/bundle --from blog
cargo run -- volume create corpus && cargo run -- volume load corpus ./dir
cargo run -- list
```

## Layout

- `crates/kiwi-nest`: the binary; `clap` picks `node`, `version` or a CLI command. End-to-end tests in `tests/`.
- `crates/kn-proto`: API types shared by the CLI, server and nodes, and the version.
- `crates/kn-bundle`: safe ZIP and directory reading, the Compose subset, `x-kiwi-nest`, the generated Compose file.
  No I/O beyond the bundle, so plain unit tests cover it.
- `crates/kn-btrfs`: the btrfs ioctls (create, snapshot, destroy), subvolume checks, paths opened without symlinks.
- `crates/kn-node`: `volumes` (the subvolume layout and the check before every start, with a fake backend for unit
  tests), `runtime` (`docker compose`), `apps` (one task per app, the state file, `--from`; `apps/shared.rs` for shared
  volumes), `api` (standalone API).
- `crates/kn-cli`: the CLI commands.

## Conventions

- Integration tests that need Docker or btrfs are opt-in through `KIWI_NEST_INTEGRATION=1` and
  `KIWI_NEST_TEST_BTRFS_DIR`, use a random Compose project prefix, and clean up only what they made.
- Unit tests use the fake subvolume backend (`volumes::fake`) wherever btrfs itself isn't what's being tested.
- Every rule the bundle checker enforces has a unit test.
