#!/usr/bin/env bash
# Tests the GitHub Action's script (scripts/action.sh) with a fake release of a fake CLI, which records what it's
# asked to do: installing and checking the CLI, deploying a preview from production, deleting it on close, and
# refusing a CLI that doesn't match its checksum.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

version=9.8.7
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
esac
release=$work/releases/v$version
mkdir -p "$release"
cat >"$release/kiwi-nest-$version-$target" <<FAKE
#!/bin/sh
echo "\$*" >>"$work/calls"
if [ "\$1" = delete ] && [ "\$2" = gone-pr1 ]; then echo "kiwi-nest: app \\\`gone-pr1\\\` doesn't exist" >&2; exit 1; fi
if [ "\$1" = version ]; then echo "kiwi-nest $version (commit fake)"; fi
echo "server=\$KIWI_NEST_SERVER token=\$KIWI_NEST_TOKEN" >>"$work/calls"
FAKE
(cd "$release" && { sha256sum "kiwi-nest-$version-$target" 2>/dev/null || shasum -a 256 "kiwi-nest-$version-$target"; } >SHA256SUMS)

run() {
  : >"$work/calls"
  rm -f "$work/out"
  env -u GITHUB_EVENT_PATH RUNNER_TEMP="$work/runner" GITHUB_OUTPUT="$work/out" KN_RELEASES_URL="file://$work/releases" \
    KN_SERVER=https://kn.example.com KN_TOKEN=knd_secret KN_APP=blog KN_VERSION=v$version KN_NUMBER=7 \
    KN_DOMAIN='{name}.example.com' KN_BUNDLE=./site "$@" "$here/action.sh"
}
expect() {
  grep -qF -- "$1" "$2" || { echo "FAIL: expected \`$1\` in $2:"; cat "$2"; exit 1; }
}

run env KN_COMMAND=deploy KN_IDLE_TIMEOUT=10m KN_BACKUPS=false
expect "deploy blog-pr7 ./site --from blog --domain blog-pr7.example.com --idle-timeout 10m --no-backups" "$work/calls"
expect "server=https://kn.example.com token=knd_secret" "$work/calls"
expect "url=https://blog-pr7.example.com" "$work/out"
expect "name=blog-pr7" "$work/out"

# A closed pull request deletes the preview; one that was never deployed is fine too.
mkdir -p "$work/event"
echo '{"action":"closed","number":7,"pull_request":{"number":7}}' >"$work/event/closed.json"
: >"$work/calls"
RUNNER_TEMP="$work/runner" GITHUB_EVENT_PATH="$work/event/closed.json" KN_RELEASES_URL="file://$work/releases" \
  KN_SERVER=https://kn.example.com KN_TOKEN=knd_secret KN_APP=blog KN_VERSION=$version "$here/action.sh"
expect "delete blog-pr7" "$work/calls"
run env KN_COMMAND=delete KN_APP=gone KN_NUMBER=1
expect "delete gone-pr1" "$work/calls"

# Deploying needs a domain; a CLI that doesn't match its checksum is refused before it runs.
if run env KN_COMMAND=deploy KN_DOMAIN= 2>/dev/null; then echo "FAIL: deployed without a domain"; exit 1; fi
echo "tampered" >>"$release/kiwi-nest-$version-$target"
if run env KN_COMMAND=deploy 2>"$work/err"; then echo "FAIL: a tampered CLI was installed"; exit 1; fi
expect "doesn't match its checksum" "$work/err"
[ ! -s "$work/calls" ] || { echo "FAIL: the tampered CLI ran"; exit 1; }
echo "action: ok"
