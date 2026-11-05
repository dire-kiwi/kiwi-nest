# Build, test and release kiwi-nest.

TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)

# The host's target, as Cargo spells it in CARGO_TARGET_<TRIPLE>_RUNNER.
HOST := $(shell rustc -vV | sed -n 's/^host: //p' | tr 'a-z-' 'A-Z_')

.PHONY: all build check fmt clippy test integration test-root test-btrfs test-s3 release clean

all: build

build:
	cargo build

## check: formatting, lints and tests (what CI runs)
check: fmt clippy test

fmt:
	cargo fmt --all --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

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

## release: static binaries for both musl targets, in dist/ (needs zig and cargo-zigbuild)
release:
	rustup target add $(TARGETS)
	cargo zigbuild --release --locked -p kiwi-nest $(foreach t,$(TARGETS),--target $(t))
	mkdir -p dist
	$(foreach t,$(TARGETS),cp target/$(t)/release/kiwi-nest dist/kiwi-nest-$(VERSION)-$(t);)
	cd dist && sha256sum kiwi-nest-$(VERSION)-* > SHA256SUMS-$(VERSION)

clean:
	cargo clean
	rm -rf dist
