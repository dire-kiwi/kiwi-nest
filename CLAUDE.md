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
- The ingress's end-to-end test (`crates/kiwi-nest/tests/ingress.rs`) also needs Caddy: `KIWI_NEST_TEST_CADDY` naming
  the binary, or `caddy` on the PATH. It's skipped otherwise. CI downloads a pinned Caddy release for it.
- `make release` builds static binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` into
  `dist/`, with `cargo zigbuild` (needs `zig` and `cargo-zigbuild`).
- `cargo run -- version` prints the version (from the workspace `Cargo.toml`) and the git commit.

To try it by hand, standalone (one node, its loopback API, no server):

```sh
# The volumes directory must be on btrfs. Prints: listening on http://127.0.0.1:7380
# and: gateway on http://127.0.0.1:7381
cargo run -- node --standalone --state-dir /tmp/kn --volumes-dir /mnt/kiwi-nest-test/kn &
cargo run -- deploy blog ./path/to/bundle
cargo run -- deploy blog-pr1 ./path/to/bundle --from blog
cargo run -- volume create corpus && cargo run -- volume load corpus ./dir
cargo run -- list
curl -H 'Host: blog.example.com' http://127.0.0.1:7381/  # through the gateway, which wakes the app if it's idle
```

Or as a cluster: `scripts/dev-cluster.sh` starts a server and two nodes (`a` and `b`, each with its own state,
gateway port, loop-mounted btrfs file and project prefix; `KIWI_NEST_DEV_BTRFS=DIR` uses directories on an existing
btrfs filesystem instead of loop mounts and sudo), and writes a CLI configuration with the cluster added. With
`caddy` on the PATH (or `KIWI_NEST_DEV_CADDY`), it also runs the ingress: Caddy with its internal CA, HTTPS on
:7443, the server's API at `https://kn.localhost:7443` (which the nodes and the CLI then use) and the nodes' gateways
listening for it on :7491 and :7492. Switching between the two needs `clean`, since nodes keep the server's URL.

```sh
scripts/dev-cluster.sh up
export KIWI_NEST_CONFIG_DIR=$PWD/.dev-cluster/config
cargo run -q -- node list
cargo run -q -- deploy blog ./path/to/bundle      # waits for the node; --node a picks a new nest's node
curl -H 'Host: blog.example.com' http://127.0.0.1:7481/
curl --cacert .dev-cluster/server/ingress-ca.crt https://blog.localhost:7443/   # through the ingress, with Caddy
cargo run -q -- nest list
scripts/dev-cluster.sh stop a                     # then `node forget a`; `start a` or `rejoin a` brings it back
scripts/dev-cluster.sh down                       # or `clean`, which also removes the cluster's state
```

The CLI goes to `--node-api` if given, else `--server` with `--token`, else the current cluster in
`~/.config/kiwi-nest` (`kiwi-nest cluster add|use|list`), else the standalone node on this machine.

## Layout

- `crates/kiwi-nest`: the binary; `clap` picks `server`, `node`, `version` or a CLI command. End-to-end tests in
  `tests/` (`cluster.rs` runs a server and nodes).
- `crates/kn-proto`: API types shared by the CLI, server and nodes (`api`), the agent link's (`agent`: polls,
  reports, desired state, commands; versioned by `PROTOCOL`), and the version.
- `crates/kn-bundle`: safe ZIP and directory reading, the Compose subset, `x-kiwi-nest`, the generated Compose file.
  No I/O beyond the bundle, so plain unit tests cover it.
- `crates/kn-btrfs`: the btrfs ioctls (create, snapshot, destroy), subvolume checks, paths opened without symlinks.
- `crates/kn-node`: `volumes` (the subvolume layout and the check before every start, with a fake backend for unit
  tests), `runtime` (the `Runtime` trait, and `Docker`, which runs `docker compose`), `apps` (one task per app, the
  state file, `--from`; `apps/shared.rs` for shared volumes; `apps/lifecycle.rs` starts, stops and adopts apps),
  `gateway` (the proxy, each app's `gate` of phases and leases, the starting page, the ingress listener's token
  check), `agent` (the link to the server: joining, the poll loop, the per-app applier and commands; `agent/link.rs`
  is its HTTP client; `agent/forgotten.rs` takes a forgotten node's apps down and sets its data aside), `api`
  (standalone API). `testing` is a fake runtime for unit tests.
- `crates/kn-server`: `store` (SQLite on its own thread, numbered migrations; `tokens`, `nodes` (cordons, labels,
  forgetting and tombstones), `apps` for apps, volumes, their nests, desired state and change status, `nests`,
  `commands`), `bundles` (the store by digest, checking uploads), `auth`, `scheduler` (placing new nests: a pure
  filter and score, with table tests), `hub` (in memory: online nodes, waking polls, callers waiting on commands),
  `api` (for the CLI), `agent` (for nodes) and `ingress` (Caddy's generated configuration, kept loaded through its
  admin API).
- `crates/kn-cli`: the CLI commands, and `config` (clusters in `~/.config/kiwi-nest/clusters.json`).

## Conventions

- Integration tests that need Docker or btrfs are opt-in through `KIWI_NEST_INTEGRATION=1` and
  `KIWI_NEST_TEST_BTRFS_DIR`, use a random Compose project prefix, and clean up only what they made. Their shared
  harness is `crates/kiwi-nest/tests/common`.
- Unit tests use the fake subvolume backend (`volumes::fake`) wherever btrfs itself isn't what's being tested, and the
  fake runtime (`testing`) for the node's lifecycle, gateway and agent. The agent's tests run a real server in the
  process, and each node on a tokio runtime of its own, so dropping the runtime kills the node mid-operation.
- The server's tests play the nodes themselves over HTTP (`crates/kn-server/src/tests.rs`).
- Tests never read the user's CLI configuration: the end-to-end harness sets `KIWI_NEST_CONFIG_DIR` to an empty
  directory.
- Every rule the bundle checker enforces has a unit test.
