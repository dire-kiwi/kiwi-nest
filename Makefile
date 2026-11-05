# Build, test and release kiwi-nest.

TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl
# The CLI alone (no server or node), for macOS.
MACOS_TARGETS := aarch64-apple-darwin x86_64-apple-darwin
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)

# The host's target, as Cargo spells it in CARGO_TARGET_<TRIPLE>_RUNNER.
HOST := $(shell rustc -vV | sed -n 's/^host: //p' | tr 'a-z-' 'A-Z_')

.PHONY: all build web web-check check fmt clippy test integration test-root test-btrfs test-s3 release release-macos sums clean

all: build

build: web
	cargo build

# The web UI's dependencies, installed from its lockfile when it changes.
WEB_DEPS := web/node_modules/.package-lock.json

$(WEB_DEPS): web/package-lock.json
	cd web && npm ci

## web: build the web UI into web/dist, which the server embeds (without it, the server serves a page saying so)
web: $(WEB_DEPS)
	cd web && npm run build

## web-check: typecheck the web UI
web-check: $(WEB_DEPS)
	cd web && npm run check

## check: formatting, lints and tests, and the web UI's typecheck (what CI runs)
check: fmt clippy test web-check

fmt:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings
	# The CLI-only build, as for macOS.
	cargo clippy -p kiwi-nest --no-default-features -- -D warnings

test:
	cargo test --workspace

## integration: also run the tests that need Docker (and btrfs: set KIWI_NEST_TEST_BTRFS_DIR, see test-btrfs)
integration:
	KIWI_NEST_INTEGRATION=1 cargo test --workspace

## test-root: the tests that need root (btrfs send), built as you and run as root through sudo; with Docker, the
## end-to-end backup tests too (set KIWI_NEST_TEST_BTRFS_DIR, and KIWI_NEST_TEST_S3 for S3). A root test that
## would skip itself fails instead (KIWI_NEST_REQUIRE_ROOT), so they can't quietly not run
test-root:
	CARGO_TARGET_$(HOST)_RUNNER="sudo -E" KIWI_NEST_REQUIRE_ROOT=1 cargo test -p kn-btrfs -p kn-backup -p kn-node
	CARGO_TARGET_$(HOST)_RUNNER="sudo -E" KIWI_NEST_REQUIRE_ROOT=1 KIWI_NEST_INTEGRATION=1 \
		cargo test -p kiwi-nest --test backup

## test-s3: start an S3-compatible server in Docker for the S3 tests; prints the KIWI_NEST_TEST_S3 to set
test-s3:
	scripts/test-s3.sh

## test-btrfs: mount a loop-mounted btrfs file for the tests (needs sudo); prints the KIWI_NEST_TEST_BTRFS_DIR to set
test-btrfs:
	scripts/test-btrfs.sh

## release: static binaries for both musl targets, in dist/ with SHA256SUMS (needs zig and cargo-zigbuild); a
## directory of them is what `kiwi-nest upgrade --from DIR` takes
release: web
	rustup target add $(TARGETS)
	cargo zigbuild --release --locked -p kiwi-nest $(foreach t,$(TARGETS),--target $(t))
	mkdir -p dist
	$(foreach t,$(TARGETS),cp target/$(t)/release/kiwi-nest dist/kiwi-nest-$(VERSION)-$(t);)
	$(MAKE) sums

## release-macos: the CLI for macOS, on a Mac, into dist/
release-macos:
	rustup target add $(MACOS_TARGETS)
	cargo build --release --locked -p kiwi-nest --no-default-features $(foreach t,$(MACOS_TARGETS),--target $(t))
	mkdir -p dist
	$(foreach t,$(MACOS_TARGETS),cp target/$(t)/release/kiwi-nest dist/kiwi-nest-$(VERSION)-$(t);)

## sums: dist/SHA256SUMS, over every release file in dist/
sums:
	cd dist && sha256sum kiwi-nest-$(VERSION)-* > SHA256SUMS

clean:
	cargo clean
	rm -rf dist web/dist
