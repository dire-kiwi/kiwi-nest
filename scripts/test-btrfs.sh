#!/bin/sh
# Mounts a loop-mounted btrfs file for the tests that need btrfs, and prints the KIWI_NEST_TEST_BTRFS_DIR to set.
#
#   scripts/test-btrfs.sh [DIR [SIZE]]     defaults: /mnt/kiwi-nest-test, 2G
#
# Needs sudo (to mount), mkfs.btrfs (btrfs-progs, for the tests only: kiwi-nest itself doesn't use it) and a kernel
# with btrfs. It's mounted with user_subvol_rm_allowed and owned by you, so tests that don't run as root can destroy
# the subvolumes they make. Running it again with the filesystem mounted does nothing. In GitHub Actions
# (GITHUB_ENV set), the variable is also set for the following steps.
set -eu

dir=${1:-/mnt/kiwi-nest-test}
size=${2:-2G}
image=${KIWI_NEST_TEST_BTRFS_IMAGE:-${RUNNER_TEMP:-/var/tmp}/kiwi-nest-test-btrfs.img}

if ! mountpoint -q "$dir" 2>/dev/null; then
  truncate -s "$size" "$image"
  mkfs.btrfs -q -f "$image"
  sudo mkdir -p "$dir"
  sudo mount -o loop,user_subvol_rm_allowed "$image" "$dir"
  sudo chown "$(id -u):$(id -g)" "$dir"
fi

echo "KIWI_NEST_TEST_BTRFS_DIR=$dir"
if [ -n "${GITHUB_ENV:-}" ]; then
  echo "KIWI_NEST_TEST_BTRFS_DIR=$dir" >>"$GITHUB_ENV"
fi
