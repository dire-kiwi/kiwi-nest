# kiwi-nest bundles

A bundle is a directory with `compose.yaml` at its root, or a ZIP of one (the ZIP holds the directory's contents,
not the directory). `kiwi-nest deploy NAME PATH` takes either; with a directory, the CLI zips it itself. The
source of truth is `crates/kn-bundle` (`manifest.rs` for `x-kiwi-nest`, `compose.rs` for the Compose subset) and
`docs/design.md` ("Bundles") in the kiwi-nest repository.

The checker reports every problem at once, each with where it is (`services.web.ports: aren't allowed: …`), and a
refused deploy saves nothing. Fix them all and deploy again.

## The idea

- **The app's code is in the bundle; images hold only dependencies.** Every file in the bundle can be mounted into
  containers, always read-only (`./site:/usr/share/nginx/html:ro`). Build the app locally (a binary, a frontend
  build, `node_modules`) into the bundle directory, so a new version is just a new bundle.
- **Images come from somewhere nodes can pull:** a public registry (`nginx:alpine`, `debian:trixie-slim`), or the
  cluster's registry after `kiwi-nest push`. Nodes never build: `build:` is refused.
- **Data lives in named volumes**, which kiwi-nest makes as btrfs subvolumes of the app. They survive redeploys,
  snapshot instantly for previews (`--from`), and are backed up. Docker named volumes, anonymous volumes and
  writable bundle files are not available.

## `x-kiwi-nest`

Required, at the top level of `compose.yaml`. Unknown keys are refused, which catches typos.

| Key | Required | Meaning |
|---|---|---|
| `domain` | yes | The app's hostname: lowercase labels of letters, digits and `-` (`blog.dire.nz`). Unique in the cluster, and not the server's API domain. `deploy --domain` overrides it. |
| `service` | yes | The service that gets HTTP traffic. It can't have `network_mode`. |
| `port` | yes | The port that service listens on inside its container (1–65535). kiwi-nest publishes it on loopback itself. |
| `idle_timeout` | no | Stop the app after this long with no HTTP requests: `90s`, `10m`, `1h30m`, `2d`. **Default `0`, always on.** Quote it in YAML (`"0"`, `"10m"`). `deploy --idle-timeout` overrides it. |
| `auth` | no | Basic auth checked at the ingress, before a request reaches the node or wakes the app: a list of `{ user, hash }`. Hashes are bcrypt only (`$2a$`/`$2b$`/`$2y$`, cost 04–31, 60 characters); user names have no `:`, spaces, `{` or `}`, and none twice. Write the hash's `$` as they are (no `$$` here). |
| `shared` | no | Shared volumes to mount: `NAME: { target: /abs/path, mode: ro \| rw \| snapshot, services: [web] }`. `mode` defaults to `ro`; `services` limits which services get it (default all). Deploy tokens can't mount `rw`. |
| `nest` | no | Put the app in this nest (with the apps and volumes already there). Usually worked out: see "Nests" below. |
| `arch` | no | `amd64` or `arm64`, for placement. |
| `node_labels` | no | Labels the node must have, such as `{ vm: "201" }`. Applied when the app's nest is placed, and checked for every member joining it. Quote the values. |
| `backups` | no | `false` leaves the app out of scheduled backups (`backup app` still works). Default `true`. `deploy --no-backups` sets it. |
| `access` | no | `public` (the default) or `lan`. With `lan`, the ingress serves the app only to connections from the server's `--lan-cidr` (`192.168.50.0/24` on `dire` and `home`), matched on the TCP peer, never a header. Anyone else, including everything through the Cloudflare tunnel, gets the ingress's 404 for an unknown host, before basic auth, and the app isn't woken. A server without `--lan-cidr` refuses the deploy. `deploy --access lan\|public` overrides it. |
| `uses` | no | Apps this one calls, such as `[cms]`: each is reached from its containers at `http://cms.internal:7383`, through the node's gateway, which wakes it if it's idle. LAN-only apps included, and nothing goes through the ingress, so the callee's `auth` and `access` don't apply to these calls. The app joins the nest of each app it uses (so it runs on the same node); they must exist and be in one nest. A deploy token may only use apps it may manage or name as `--from` (`token create deploy --from cms`). Needs a node newer than 0.2.1. See example 7. |

Generate a hash with `htpasswd -nbB USER PASSWORD` (apache-tools; the part after `:`), or
`docker run --rm caddy:2 caddy hash-password --plaintext PASSWORD`. Don't put the password in the bundle or the repo.

## The Compose subset

**Top level:** `services`, `volumes`, `networks`, `x-kiwi-nest`, `name` and `version` (ignored), and any other
`x-` key (for anchors). `include`, `configs` and `secrets` are refused, as is anything else. Only `compose.yaml`
is read: not `docker-compose.yml`, `compose.yml` or override files, and no `.env`.

**Services** take a list of known keys; anything not listed is refused, so a new Compose feature stays refused
until someone reviews it.

- Needed: `image` (a reference nodes can pull).
- Accepted as they are: `command`, `entrypoint`, `environment`, `working_dir`, `user`, `group_add`, `hostname`,
  `domainname`, `healthcheck`, `stop_signal`, `stop_grace_period`, `init`, `read_only`, `tty`, `stdin_open`,
  `tmpfs`, `expose`, `ulimits`, `shm_size`, `mem_limit`, `mem_reservation`, `memswap_limit`, `mem_swappiness`,
  `cpus`, `cpu_shares`, `cpu_quota`, `cpu_period`, `cpuset`, `pids_limit`, `cap_drop`, `dns`, `dns_search`,
  `dns_opt`, `extra_hosts`, `platform`, and `x-` keys.
- Checked: `volumes` (see mounts), `env_file` (files in the bundle; `{ path, required: false }` may be missing),
  `restart` (`no`, `always`, `unless-stopped`, `on-failure[:N]`; `always` becomes `unless-stopped`, so Docker
  doesn't restart an app kiwi-nest stopped for being idle), `depends_on`, `links` and `volumes_from` (services in
  the bundle), `networks` (declared ones; only `aliases`), `labels` (not `com.docker.*` or `kiwi-nest.*`),
  `logging` (driver `json-file` or `local` only, so `kiwi-nest logs` works), `pull_policy`, `network_mode`, `pid`,
  `ipc`, `uts`, `userns_mode`, `cgroup` (only `service:NAME` or private settings such as `none`; never `host`).

Refused, and why:

| Refused | Why, and what to do instead |
|---|---|
| `build` | Nodes don't build. Build the image elsewhere, `kiwi-nest push` it, use `image`. |
| `ports` | kiwi-nest publishes `x-kiwi-nest.port` on loopback for its gateway. Other ports aren't reachable from outside; use `expose` between services. |
| `container_name`, `scale` | kiwi-nest names containers after the app, and runs one of each service. |
| `privileged`, `cap_add`, `devices`, `device_cgroup_rules`, `gpus`, `security_opt`, `sysctls` | No extra power over the host. (`cap_drop` is fine.) |
| Host namespaces (`network_mode: host`, `pid: host`, …) | Same. `service:NAME` within the app works. |
| `deploy` | Use `mem_limit` and `cpus`. `mem_limit` also feeds placement (a service without one counts as 256 MiB). |
| `extends`, `profiles`, `include` | Write the whole app in `compose.yaml`; every service runs. Anchors and merge keys (`<<`) do work. |
| `configs`, `secrets` | Put the files in the bundle and mount or `env_file` them. |
| Variable interpolation (`$VAR`, `${VAR}`) | Compose runs with an empty environment and no `.env`, so the bundle means the same on every node. Write a literal `$` as `$$`. Put settings in the bundle (an `env_file` written by the release script). |
| YAML tags (`!reset`, `!override`, …) | Not supported. |
| Log drivers other than `json-file` and `local` | `kiwi-nest logs` reads Docker's logs. |
| Volumes other than `{}` (external, `name:`, `driver:`) | Each volume is the app's own subvolume. |
| Networks with anything but `internal` | No external, custom-named or driver networks; no fixed addresses. |

`env_file` contents aren't checked, but Compose expands `$` in them like anything else, with nothing set: keep `$`
out of values, or refuse it in the script that writes the file.

### Mounts

A mount's source must be one of:

- **A named volume** declared under the top-level `volumes:` as `{}`: `data:/var/lib/app`. It becomes the app's
  subvolume `VOLUMES/apps/APP/data`. `:ro` works.
- **A path in the bundle**, starting with `.`: `./site:/srv:ro`, or `.:/app` for the whole bundle. Always
  read-only; asking for `rw` is refused. The path must exist in the bundle.
- **A `tmpfs`**: `tmpfs: [/tmp]`, or a long-form mount with `type: tmpfs` (`size`, `mode`).
- **An absolute host path** only under one of the node's `--allow-host-path` directories (vm203 allows
  `/srv/huntaway-preview-claude`); anything else is refused, as are `~` and `..`.

Refused: anonymous volumes (`/data` alone, or `type: volume` with no source), undeclared volumes, mount options
other than `ro`, `rw`, `z`, `Z`, and missing sources (nothing is created on the node).

### Files and limits

Only plain files and directories: no symlinks, devices, FIFOs or sockets, no absolute paths or `..`, no duplicates,
no encrypted ZIP entries. Files keep only their executable bit. Limits: `compose.yaml` 1 MiB; ZIP 256 MiB; 20,000
entries; 256 MiB per file; 1 GiB expanded; paths 1024 bytes and 32 levels deep. For bigger data, use a shared
volume (`kiwi-nest volume create` and `volume load`) or the app's own volume.

## Nests

A nest is a group of apps and shared volumes that live on one node, because btrfs snapshots don't cross
filesystems. An app's nest comes from (in effect, all must agree): the nest it's already in (a redeploy), its
`--from` app's nest, the nests of the shared volumes it mounts, the nests of the apps it `uses`, and
`x-kiwi-nest.nest`. With none, it's a new nest
named after the app. Two that disagree refuse the deploy. A new nest is placed on a node that is online, not
cordoned, has the `arch` and every `node_labels`, and allows the host paths; then by memory headroom, unless
`deploy --node NODE` picks it. **Once placed, a nest never moves**, so `node_labels` on a redeploy only checks,
it doesn't move the app.

## Examples

### 1. Static site

```text
site/
  compose.yaml
  public/index.html ...
```

```yaml
x-kiwi-nest:
  domain: hello.dire.nz
  service: web
  port: 80
  idle_timeout: "10m"
  node_labels: { vm: "201" }

services:
  web:
    image: nginx:alpine
    volumes:
      - ./public:/usr/share/nginx/html:ro
    mem_limit: 64m
```

`kiwi-nest deploy hello ./site`.

### 2. An app with data, always on

```yaml
x-kiwi-nest:
  domain: notes.dire.nz
  service: app
  port: 3000
  idle_timeout: "0"
  node_labels: { vm: "201" }

services:
  app:
    image: node:22-alpine
    working_dir: /app
    command: [node, server.js]
    env_file: [app.env]          # written by the release script; no `$` in it
    volumes:
      - ./app:/app:ro            # the built app, node_modules included
      - data:/data
    read_only: true
    tmpfs: [/tmp]
    mem_limit: 256m
    healthcheck:
      test: [CMD, wget, -qO-, http://127.0.0.1:3000/health]
      interval: 5s
      retries: 10

volumes:
  data: {}
```

### 3. App and Postgres

```yaml
x-kiwi-nest:
  domain: shop.code.dire.nz
  service: web
  port: 8080
  idle_timeout: "30m"            # both services stop together when idle
  node_labels: { vm: "203" }

services:
  web:
    image: registry.dire.nz/shop-deps:3
    command: [/app/shop]
    environment:
      DATABASE_URL: postgres://shop:shop@db/shop
    volumes: [ "./bin:/app:ro" ]
    depends_on:
      db: { condition: service_healthy }
    mem_limit: 256m
  db:
    image: postgres:17-alpine
    environment:
      POSTGRES_USER: shop
      POSTGRES_PASSWORD: shop
      POSTGRES_DB: shop
    volumes: [ "pg:/var/lib/postgresql/data" ]
    healthcheck:
      test: [CMD-SHELL, "pg_isready -U shop"]
      interval: 2s
      retries: 30
    mem_limit: 256m

volumes:
  pg: {}
```

The database is reachable only inside the app's network (no `ports`). A preview made with `--from shop` gets an
instant snapshot of `pg`, taken while shop's containers are paused for a moment.

### 4. Behind basic auth

```yaml
x-kiwi-nest:
  domain: private.dire.nz
  service: web
  port: 80
  auth:
    - { user: ivan, hash: "$2y$10$3w0f8bQm1Yy3Kq0jv7m8Ue8h1U6m3VqH2eVbq2R0Zk6g4wq9b1n9W" }

services:
  web:
    image: nginx:alpine
    volumes: [ "./public:/usr/share/nginx/html:ro" ]
```

(The hash is an example; generate a real one.)

### 4b. LAN only

```yaml
x-kiwi-nest:
  domain: notes.code.dire.nz      # a LAN-only name (DNS-only record)
  service: web
  port: 80
  access: lan                     # and the ingress refuses anyone off 192.168.50.0/24
  node_labels: { vm: "203" }

services:
  web:
    image: nginx:alpine
    volumes: [ "./public:/usr/share/nginx/html:ro" ]
```

From outside the LAN (the tunnel included) it answers as if nothing were served there. Add `auth` too if not
everyone on the LAN should see it.

### 5. A dependencies image from the cluster's registry

```sh
docker buildx build --platform linux/amd64 --load -t report-deps:dev deps/   # no app code in it
kiwi-nest push report-deps:dev report-deps:4    # prints registry.dire.nz/report-deps:4
```

```yaml
services:
  app:
    image: registry.dire.nz/report-deps:4
    command: [/app/report]
    volumes: [ "./bin:/app:ro" ]
```

Give the image a new tag only when the dependencies change. Don't copy images onto nodes by hand
(`docker save | ssh … docker load`): nodes prune images their apps stopped using and must be able to pull them
again, and the registry's garbage collection keeps only the tags apps use (or pushed in the last 24 hours).

### 6. A shared dataset

```sh
kiwi-nest volume create corpus --node vm201     # admin token; a nest named after the volume
kiwi-nest volume load corpus ./corpus-dir       # replaces its contents (refused while mounted ro/rw)
```

```yaml
x-kiwi-nest:
  domain: search.dire.nz
  service: web
  port: 8080
  shared:
    corpus: { target: /corpus, mode: snapshot }   # its own writable copy, made on its first deploy

services:
  web:
    image: registry.dire.nz/search-deps:1
    volumes: [ "./bin:/app:ro" ]
```

Mounting `corpus` puts the app in corpus's nest. `ro` mounts the volume itself read-only, `rw` shares writes with
every app mounting it (admin tokens only), and `snapshot` gives the app its own writable copy, kept across
redeploys and deleted with the app unless `delete --keep-data`. `volume load` is refused while any app mounts the
volume `ro` or `rw`; snapshots keep what they had.

### 7. An app calling another (a public site and its private CMS)

The CMS is LAN-only; the site renders pages from it on each request. Deploy `cms` first, then `blog`, which goes
into cms's nest and so onto its node:

```yaml
# cms/compose.yaml
x-kiwi-nest:
  domain: cms.code.dire.nz        # LAN-only, for editing
  service: web
  port: 3000
  access: lan
  idle_timeout: "30m"             # woken by the blog's calls too
  node_labels: { vm: "201" }
...
```

```yaml
# blog/compose.yaml
x-kiwi-nest:
  domain: blog.dire.nz
  service: web
  port: 8080
  idle_timeout: "0"
  uses: [cms]                     # http://cms.internal:7383 from blog's containers
services:
  web:
    image: registry.dire.nz/blog-deps:1
    env_file: [ ./blog.env ]      # CMS_URL=http://cms.internal:7383
```

The call arrives at cms with `Host: cms.internal:7383` and `X-Forwarded-For` the caller's container address, not
through the ingress. Another app's containers can't reach it this way: the node's gateway only serves `NAME.internal`
to the apps that list `NAME` in `uses`, recognised by their Docker network's addresses, so no header lets anyone
else in. A redeploy of `blog` that was already deployed in a nest of its own is refused (a nest never moves):
delete it and deploy it again, or give `cms` `nest: blog` when it's first deployed instead.
