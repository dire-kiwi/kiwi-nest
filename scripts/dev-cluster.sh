#!/usr/bin/env bash
# A development cluster on this machine: a kiwi-nest server and two nodes, `a` and `b`, sharing the Docker Engine.
#
#   scripts/dev-cluster.sh up       build, then start the server and the nodes in the background (the default)
#   scripts/dev-cluster.sh status   show what's running, and the nodes as the server sees them
#   scripts/dev-cluster.sh down     stop them, remove the nodes' Compose projects and unmount their filesystems
#   scripts/dev-cluster.sh clean    down, then remove the cluster's state (the server's database, the nodes' data)
#
# Each node gets its own state directory, gateway port, loop-mounted btrfs file (mounted with sudo, with
# user_subvol_rm_allowed, and owned by you) and Compose project prefix (kn-dev-a-, kn-dev-b-). The server listens on
# 127.0.0.1:7400, and the nodes' gateways on 127.0.0.1:7481 and :7482. Everything lives in .dev-cluster/ (or
# $KIWI_NEST_DEV_DIR), including a CLI configuration with the cluster added as `dev`:
#
#   export KIWI_NEST_CONFIG_DIR=$PWD/.dev-cluster/config
#   kiwi-nest node list
#   kiwi-nest deploy blog ./path/to/bundle
#
# With Caddy (`caddy` on the PATH, or KIWI_NEST_DEV_CADDY naming it; KIWI_NEST_DEV_CADDY=none skips it), the
# cluster has an ingress: Caddy with its admin API on 127.0.0.1:7490, HTTPS on :7443 and HTTP on :7480, configured
# by the server with Caddy's internal CA. The server's API is then https://kn.localhost:7443, which the nodes and
# the CLI use, trusting .dev-cluster/server/ingress-ca.crt, and the nodes' gateways also listen for the ingress on
# 127.0.0.1:7491 and :7492, where requests need each node's ingress token:
#
#   curl --cacert .dev-cluster/server/ingress-ca.crt https://blog.localhost:7443/
#
# To skip the loop mounts (and sudo), set KIWI_NEST_DEV_BTRFS to a directory on a btrfs filesystem you own; each
# node then keeps its volumes in a directory there. Logs are in .dev-cluster/*.log; KIWI_NEST_LOG sets the level.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dir=${KIWI_NEST_DEV_DIR:-$root/.dev-cluster}
mnt=${KIWI_NEST_DEV_MNT:-/mnt/kiwi-nest-dev}
size=${KIWI_NEST_DEV_SIZE:-2G}
server=127.0.0.1:7400
bin=$root/target/debug/kiwi-nest
caddy=${KIWI_NEST_DEV_CADDY:-$(command -v caddy || true)}
[ "$caddy" = none ] && caddy=
caddy_admin=127.0.0.1:7490
http_port=7480
https_port=7443
api_domain=kn.localhost
nodes=(a b)
export KIWI_NEST_LOG=${KIWI_NEST_LOG:-info}

gateway_port() { case $1 in a) echo 7481 ;; b) echo 7482 ;; esac; }
ingress_port() { case $1 in a) echo 7491 ;; b) echo 7492 ;; esac; }
app_ports() { case $1 in a) echo 31000-31999 ;; b) echo 32000-32999 ;; esac; }

# Where a node's volumes live: its loop mount, or a directory under KIWI_NEST_DEV_BTRFS.
volumes_dir() {
  if [ -n "${KIWI_NEST_DEV_BTRFS:-}" ]; then
    echo "$KIWI_NEST_DEV_BTRFS/kiwi-nest-dev-$1"
  else
    echo "$mnt/$1"
  fi
}

running() { [ -f "$dir/$1.pid" ] && kill -0 "$(cat "$dir/$1.pid")" 2>/dev/null; }

# Waits until a file exists, or gives up.
wait_for_file() {
  local file=$1 what=$2 log=$3
  for _ in $(seq 1 100); do
    [ -s "$file" ] && return 0
    sleep 0.1
  done
  echo "$what didn't appear; see $log" >&2
  tail -n 20 "$log" >&2 || true
  exit 1
}

start_caddy() {
  running caddy && return
  mkdir -p "$dir/caddy"
  echo "{\"admin\":{\"listen\":\"$caddy_admin\"}}" >"$dir/caddy/boot.json"
  : >"$dir/caddy.log"
  XDG_DATA_HOME=$dir/caddy/data XDG_CONFIG_HOME=$dir/caddy/config \
    "$caddy" run --config "$dir/caddy/boot.json" >>"$dir/caddy.log" 2>&1 &
  echo $! >"$dir/caddy.pid"
  for _ in $(seq 1 100); do
    if (exec 3<>"/dev/tcp/${caddy_admin%:*}/${caddy_admin#*:}") 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  echo "Caddy didn't start; see $dir/caddy.log" >&2
  exit 1
}

# Waits for a line matching a pattern in a log, or gives up.
wait_for() {
  local log=$1 pattern=$2 what=$3
  for _ in $(seq 1 100); do
    if grep -q "$pattern" "$log" 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  echo "$what didn't start; see $log" >&2
  tail -n 20 "$log" >&2 || true
  exit 1
}

mount_node() {
  local n=$1 vol
  vol=$(volumes_dir "$n")
  if [ -n "${KIWI_NEST_DEV_BTRFS:-}" ]; then
    mkdir -p "$vol"
    return
  fi
  if ! mountpoint -q "$vol" 2>/dev/null; then
    command -v mkfs.btrfs >/dev/null || { echo "mkfs.btrfs (btrfs-progs) is needed for the loop mounts" >&2; exit 1; }
    truncate -s "$size" "$dir/$n.img"
    mkfs.btrfs -q -f "$dir/$n.img"
    sudo mkdir -p "$vol"
    sudo mount -o loop,user_subvol_rm_allowed "$dir/$n.img" "$vol"
    sudo chown "$(id -u):$(id -g)" "$vol"
  fi
}

up() {
  (cd "$root" && cargo build -q -p kiwi-nest)
  mkdir -p "$dir"
  local url=http://$server ingress=() ca=() listen
  if [ -n "$caddy" ]; then
    start_caddy
    url=https://$api_domain:$https_port
    ingress=(--ingress internal --caddy-admin "http://$caddy_admin" --api-domain "$api_domain"
      --http-port "$http_port" --https-port "$https_port" --ingress-sync 2s)
    ca=(--ca-cert "$dir/server/ingress-ca.crt")
  fi
  if ! running server; then
    : >"$dir/server.log"
    "$bin" server --listen "$server" --state-dir "$dir/server" "${ingress[@]}" >>"$dir/server.log" 2>&1 &
    echo $! >"$dir/server.pid"
    wait_for "$dir/server.log" "^listening on" "the server"
    # Only the first start prints an admin token.
    local token
    token=$(sed -n 's/^admin token: //p' "$dir/server.log")
    if [ -n "$token" ]; then
      (umask 077 && echo "$token" >"$dir/admin-token")
    fi
  fi
  [ -f "$dir/admin-token" ] || { echo "no admin token in $dir/admin-token; try \`$0 clean\`" >&2; exit 1; }
  if [ -n "$caddy" ]; then
    # Caddy's internal CA, which the server saves once Caddy has its configuration.
    wait_for_file "$dir/server/ingress-ca.crt" "the ingress CA" "$dir/server.log"
  fi
  local admin
  admin=$(cat "$dir/admin-token")
  export KIWI_NEST_CONFIG_DIR=$dir/config
  if [ ! -f "$dir/config/clusters.json" ]; then
    "$bin" cluster add dev "$url" --token "$admin" "${ca[@]}" >/dev/null
  fi
  for n in "${nodes[@]}"; do
    running "$n" && continue
    mount_node "$n"
    local join=()
    if [ ! -f "$dir/$n/state/agent.json" ]; then
      join=(--join-token "$("$bin" token create join --name "dev node $n" 2>/dev/null)")
    fi
    listen=()
    [ -n "$caddy" ] && listen=(--ingress-listen "127.0.0.1:$(ingress_port "$n")")
    : >"$dir/$n.log"
    "$bin" node --server "$url" "${ca[@]}" "${join[@]}" --name "$n" \
      --state-dir "$dir/$n/state" --volumes-dir "$(volumes_dir "$n")/volumes" \
      --gateway-listen "127.0.0.1:$(gateway_port "$n")" "${listen[@]}" --ports "$(app_ports "$n")" \
      --project-prefix "kn-dev-$n-" >>"$dir/$n.log" 2>&1 &
    echo $! >"$dir/$n.pid"
    wait_for "$dir/$n.log" "^node $n polling" "node $n"
  done
  echo "server on http://$server; nodes a (gateway 127.0.0.1:7481) and b (gateway 127.0.0.1:7482)"
  if [ -n "$caddy" ]; then
    echo "ingress: https://DOMAIN:$https_port (CA $dir/server/ingress-ca.crt); the server's API at $url"
  else
    echo "no ingress: put caddy on the PATH (or set KIWI_NEST_DEV_CADDY) for one"
  fi
  echo "use it with: export KIWI_NEST_CONFIG_DIR=$dir/config"
}

status() {
  for p in caddy server "${nodes[@]}"; do
    if running "$p"; then echo "$p: running (pid $(cat "$dir/$p.pid"))"; else echo "$p: stopped"; fi
  done
  if running server; then
    KIWI_NEST_CONFIG_DIR=$dir/config "$bin" node list
  fi
}

down() {
  for p in "${nodes[@]}" server caddy; do
    if running "$p"; then
      kill "$(cat "$dir/$p.pid")"
      echo "stopped $p"
    fi
    rm -f "$dir/$p.pid"
  done
  # The nodes leave their apps running when they stop; remove them.
  if command -v docker >/dev/null; then
    { docker compose ls --all --format json 2>/dev/null || true; } |
      { grep -o '"Name":"kn-dev-[ab]-[^"]*"' || true; } | cut -d'"' -f4 |
      while read -r project; do
        echo "removing $project"
        docker compose -p "$project" down --remove-orphans >/dev/null 2>&1 || true
      done
  fi
  if [ -z "${KIWI_NEST_DEV_BTRFS:-}" ]; then
    for n in "${nodes[@]}"; do
      if mountpoint -q "$mnt/$n" 2>/dev/null; then
        sudo umount "$mnt/$n"
        echo "unmounted $mnt/$n"
      fi
    done
  fi
}

clean() {
  down
  rm -rf "$dir"
  if [ -n "${KIWI_NEST_DEV_BTRFS:-}" ]; then
    for n in "${nodes[@]}"; do rm -rf "$(volumes_dir "$n")"; done
  fi
  echo "removed $dir"
}

case ${1:-up} in
  up) up ;;
  status) status ;;
  down) down ;;
  clean) clean ;;
  *) echo "usage: $0 [up|status|down|clean]" >&2; exit 2 ;;
esac
