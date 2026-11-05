# Build, test and release kiwi-nest.

TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)

.PHONY: all build check fmt clippy test integration test-btrfs release clean

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
