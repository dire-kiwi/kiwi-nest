#!/usr/bin/env bash
# The GitHub Action (action.yml): installs the released kiwi-nest CLI, checking it against the release's SHA256SUMS,
# then deploys the pull request's preview `--from` production, or deletes it once the pull request is closed.
#
# Its inputs come as KN_* variables (see action.yml). For tests: KN_RELEASES_URL replaces GitHub's release
# downloads (any URL curl takes, file:// included), and without GITHUB_* variables it needs KN_NUMBER and
# KN_COMMAND.
set -euo pipefail

die() { echo "::error::$*" >&2; exit 1; }
log() { echo "kiwi-nest preview: $*" >&2; }

[ -n "${KN_SERVER:-}" ] || die "the server input is needed"
[ -n "${KN_TOKEN:-}" ] || die "the token input is needed"
[ -n "${KN_APP:-}" ] || die "the app input is needed (the production app previews start from)"
echo "::add-mask::$KN_TOKEN"
server=${KN_SERVER%/}
tmp=${RUNNER_TEMP:-$(mktemp -d)}/kiwi-nest
mkdir -p "$tmp/bin"

# The pull request, and what to do.
event=${GITHUB_EVENT_PATH:-}
field() { [ -n "$event" ] && [ -f "$event" ] && jq -r "$1 // empty" "$event"; }
number=${KN_NUMBER:-$(field '.pull_request.number // .number' || true)}
[ -n "$number" ] || die "no pull request number: run this on pull_request events"
command=${KN_COMMAND:-}
if [ -z "$command" ]; then
  if [ "$(field '.action' || true)" = closed ]; then command=delete; else command=deploy; fi
fi
case $command in deploy|delete) ;; *) die "command must be deploy or delete, not $command" ;; esac
name=${KN_NAME:-$KN_APP-pr$number}

curl_ca=()
if [ -n "${KN_CA_CERT:-}" ]; then
  printf '%s\n' "$KN_CA_CERT" >"$tmp/ca.crt"
  export KIWI_NEST_CA_CERT=$tmp/ca.crt
  curl_ca=(--cacert "$tmp/ca.crt")
fi

# The CLI: the server's version unless told otherwise, as the CLI must be at least as new as the server.
version=${KN_VERSION:-}
version=${version#v}
if [ -z "$version" ]; then
  version=$(curl -fsS "${curl_ca[@]}" "$server/v1/version" | jq -r .version) ||
    die "couldn't ask $server for its version"
fi
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) die "there's no kiwi-nest release for $(uname -s) $(uname -m)" ;;
esac
asset=kiwi-nest-$version-$target
releases=${KN_RELEASES_URL:-https://github.com/${KN_REPOSITORY:-dire-kiwi/kiwi-nest}/releases/download}
log "installing $asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$releases/v$version/SHA256SUMS" || die "couldn't download release $version's SHA256SUMS"
curl -fsSL -o "$tmp/$asset" "$releases/v$version/$asset" || die "couldn't download $asset"
want=$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$want" ] || die "release $version's SHA256SUMS doesn't list $asset"
if command -v sha256sum >/dev/null; then
  got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else
  got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)
fi
[ "$got" = "$want" ] || die "$asset doesn't match its checksum: its SHA-256 is $got, and SHA256SUMS says $want"
install -m 0755 "$tmp/$asset" "$tmp/bin/kiwi-nest"
[ -n "${GITHUB_PATH:-}" ] && echo "$tmp/bin" >>"$GITHUB_PATH"
kn=$tmp/bin/kiwi-nest
"$kn" version >&2

export KIWI_NEST_SERVER=$server KIWI_NEST_TOKEN=$KN_TOKEN
# Never the runner's own configuration.
export KIWI_NEST_CONFIG_DIR=$tmp/config

output() { [ -n "${GITHUB_OUTPUT:-}" ] && echo "$1=$2" >>"$GITHUB_OUTPUT"; return 0; }
summary() { [ -n "${GITHUB_STEP_SUMMARY:-}" ] && echo "$1" >>"$GITHUB_STEP_SUMMARY"; return 0; }
output name "$name"

if [ "$command" = delete ]; then
  log "deleting $name"
  if ! out=$("$kn" delete "$name" 2>&1); then
    echo "$out" >&2
    case $out in
      *"app \`$name\` doesn't exist"*) log "$name was never deployed, or is gone already" ;;
      *) die "couldn't delete $name" ;;
    esac
  else
    echo "$out" >&2
  fi
  output url ""
  summary "Preview \`$name\` deleted."
  exit 0
fi

[ -n "${KN_DOMAIN:-}" ] || die "the domain input is needed to deploy, such as {name}.example.com"
domain=${KN_DOMAIN//\{name\}/$name}
domain=${domain//\{number\}/$number}
args=(deploy "$name" "${KN_BUNDLE:-.}" --from "$KN_APP" --domain "$domain")
[ -n "${KN_IDLE_TIMEOUT:-}" ] && args+=(--idle-timeout "$KN_IDLE_TIMEOUT")
case ${KN_BACKUPS:-false} in
  false) args+=(--no-backups) ;;
  true) ;;
  *) die "backups must be true or false, not $KN_BACKUPS" ;;
esac
log "deploying $name at $domain, from $KN_APP"
"$kn" "${args[@]}"
scheme=https
case $server in http://*) scheme=http ;; esac
output url "$scheme://$domain"
summary "Preview \`$name\` deployed at $scheme://$domain, from \`$KN_APP\`."
