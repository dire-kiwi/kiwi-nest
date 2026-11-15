---
name: kiwi-nest
description: Deploy and manage web apps on the user's kiwi-nest clusters with the kiwi-nest CLI. Use this whenever the user wants to deploy, publish, host, preview, or put online a site, app, static page, or Docker container "on my server", on the home lab or the Proxmox VMs (201, 203), on dire.nz, *.dire.nz or *.code.dire.nz, or "with kiwi-nest"; whenever they ask to list, inspect, check logs of, suspend, resume, redeploy, update, or delete something deployed there; when packaging a Docker Compose app as a kiwi-nest bundle (compose.yaml with x-kiwi-nest); for pull-request previews (deploy --from, the kiwi-nest GitHub Action), deploy tokens, pushing images to registry.dire.nz, and CI workflows that deploy; and when moving an app from site-deploy to kiwi-nest. Use it, not site-deploy, for anything on dire.nz or VMs 201 and 203.
---

# kiwi-nest

kiwi-nest runs single-instance apps from **bundles** (a directory with a `compose.yaml` that has an `x-kiwi-nest`
block) on a small cluster: a **server** (API, web UI, scheduler, ingress Caddy, image registry) and **nodes** that
run the apps with Docker Compose, keep each app's data in btrfs subvolumes, stop idle apps and wake them on the next
request. Apps that share data (production and its previews, a shared dataset and its readers) live in one **nest**
on one node, so previews start from instant snapshots of production's data.

- **Source of truth:** the kiwi-nest repo, `~/personal/kiwi-nest` (`docs/design.md`, `docs/install.md`,
  `crates/kn-bundle` for what bundles may contain). `kiwi-nest COMMAND --help` for flags. Don't guess at behaviour;
  read those when this skill doesn't answer something.
- **The CLI** is `~/.local/bin/kiwi-nest`. It must be at least as new as the server:
  `curl -fsS https://kn.dire.nz/v1/version` against `kiwi-nest version`. To update it, install the release of the
  server's version from `https://github.com/dire-kiwi/kiwi-nest/releases` (check it against `SHA256SUMS`).

## The clusters

The CLI keeps its clusters in `~/.config/kiwi-nest/clusters.json` (mode 0600: server URL, token, which is current).
`kiwi-nest cluster list` shows them, with `*` on the current one. **Read them from there; never print, copy or
hard-code a token**, and never put one on a command line or in a committed file. **Always pass `--cluster NAME`**:
the current cluster can change under you.

| Cluster | Server | What it is |
|---|---|---|
| `dire` | `https://kn.dire.nz` (LAN only) | **Production.** Server, ingress, registry and node `vm201` on Proxmox VM 201; node `vm203` on VM 203. Real apps go here. |
| `home` | `https://nest.dire.nz` (LAN only) | **The test cluster**, on VM 207 (node `vm207`). For trying kiwi-nest releases and throwaway apps. Nothing real lives there. |

If `cluster list` doesn't show the one you need, stop and tell the user (adding one needs an admin token). The
workstation may not have `home`: it's in `ivan`'s CLI on VM 207 (`ssh ivan@192.168.50.207`).

**Nodes of `dire`**, chosen with labels (`x-kiwi-nest.node_labels`), not by name:

| Node | Machine | Label | For |
|---|---|---|---|
| `vm201` | VM 201, 192.168.50.201, amd64 | `vm=201` | Public apps (`*.dire.nz`): kiwicd, potfit, trackmania, docs. Trackmania must stay here (VM 202's XML-RPC accepts only 201). |
| `vm203` | VM 203, 192.168.50.203, amd64 | `vm=203` | LAN apps: `code` (the huntaway hub) and its `code-prN` previews. Allows the host path `/srv/huntaway-preview-claude`. |

`vm203` joins in PRO-9's step 2: check `kiwi-nest --cluster dire node list` before relying on it. Both nodes are
amd64: build images for `linux/amd64`.

**Who can reach which names** (decided by DNS, so pick the domain by who should reach the app):

| Names | Reached from | Through |
|---|---|---|
| `NAME.dire.nz` | **the internet** | Cloudflare (proxied record) → the tunnel on 201 → kiwi-nest's Caddy. Treat as public. |
| `NAME.code.dire.nz` | **the LAN only** | DNS-only records → 192.168.50.201 (→ .203 until 203's switch). The tunnel 404s them. |
| `kn.dire.nz`, `registry.dire.nz` | the LAN only | DNS-only → .201: the API, web UI and registry |
| `NAME.nest.dire.nz` | the LAN only | the `home` test cluster on 207 |

One subdomain level per wildcard: `a.dire.nz` or `a.code.dire.nz`, not `a.b.dire.nz`. A LAN-only app on a
`*.dire.nz` name needs its own DNS-only record; otherwise use `*.code.dire.nz`.

<!-- TODO(KN-19): once KN-19 lands and dire runs a release with it, document `x-kiwi-nest.access: lan` (Caddy
refuses requests whose TCP peer isn't on `--lan-cidr 192.168.50.0/24`, so tunnel traffic gets 403) and
`deploy --access lan`, and use it for code, its previews and any private app. Until then privacy is by name
only, and the bundle checker refuses an `access` key. -->

**The move from site-deploy is in progress** (PRO-9). Until its switches happen, the tunnel still sends
`*.dire.nz` to site-deploy, and `*.code.dire.nz` still resolves to VM 203's site-deploy, so an app on kiwi-nest
isn't reachable at its name yet. Check it on the LAN with
`curl --resolve NAME.dire.nz:443:192.168.50.201 https://NAME.dire.nz/`. `~/personal/proxmox/vms/201-dev.md` (and
`203-personal.md`) say whether the switch has happened, and list the apps.

**Access check**, first: `kiwi-nest --cluster dire node list`. It proves the server, the token and the nodes in one
call. A TLS error means the machine isn't on the LAN; 401 a bad token; "upgrade" the CLI is older than the server.

## Commands

All take `--cluster NAME` (omitted below). `kiwi-nest COMMAND --help` has every flag.

| Command | Notes |
|---|---|
| `list` | Apps: status, containers, generation, node, nest, domain. `--json` for everything |
| `inspect NAME` | Node, nest and why it's there, status, error, domain, routing, idle timeout, open requests, Compose project, volumes, and each container's state |
| `deploy NAME PATH` | PATH is the bundle directory (the CLI zips it) or a ZIP. Creates the app or **replaces it in place**, keeping its volumes. Waits until the node reports it (`--timeout`, 15m). `--create-only` refuses if NAME exists |
| `deploy NAME PATH --from APP` | A preview: on NAME's first deploy, each named volume it shares with APP starts as an instant snapshot of APP's. Later deploys keep NAME's data and say `--from` was ignored, so CI can pass it every time |
| `deploy … --domain D --idle-timeout 10m --no-backups` | Override the bundle's `domain`, `idle_timeout` and `backups`, such as for a preview made from production's bundle |
| `deploy … --node NODE` | Pick the node of a **new** nest (prefer `node_labels` in the bundle). A nest never moves |
| `deploy … --queue` | Save it even if the node is offline (refused otherwise); applied when the node is back |
| `logs NAME` | `-n/--tail N`, `-f/--follow`, `-t/--timestamps`, `-s/--service S` |
| `suspend NAME` | Stops the app and **keeps it stopped: requests get 503 and don't wake it**. Refused while it has open requests |
| `resume NAME` | Starts an app that's suspended, idle or failed, and waits until it's ready |
| `delete NAME` | **Removes its containers, bundle and volumes: the data is gone.** `--keep-data` keeps the volumes (and its snapshots of shared volumes) for a later deploy of NAME |
| `push LOCAL NAME:TAG` | Logs Docker in with the CLI's token, tags and pushes to the cluster's registry, and prints the reference for bundles (`registry.dire.nz/NAME:TAG`) |
| `registry host` / `registry login` | The registry's host; log Docker in for `docker push`/`pull` |
| `nest list` | Each nest's node, apps, shared volumes, memory, disk, and why it's where it is |
| `node list` | Nodes: online, cordoned, arch, memory and headroom, btrfs free, labels, version |
| `volume create NAME` / `load NAME DIR` / `list` / `inspect NAME` / `delete NAME` / `delete-snapshot NAME APP` | Shared volumes (see the bundle reference). Create, load and delete need an admin token |
| `backup app NAME` / `backup list` | Backups to the server's targets. **`dire` has no backup targets yet** (a follow-up of PRO-9), so don't count on them |
| `token create deploy --name N --pattern P [--from APP]` / `token list` / `token revoke ID` | Admin only. See "CI" below |
| `user add NAME` | Web UI users (admin only). The UI is at `https://kn.dire.nz` |

Names (apps, nests, volumes): lowercase letters, digits and `-`, at most 50, starting and ending with a letter or
digit. App statuses: `deploying`, `running`, `idle` (stopped for being idle, or its last start failed; a request
starts it), `suspended` (only `resume` starts it), `failed` (its last deploy failed).

## Deploying an app

1. **Pick the cluster, name and domain.** Real apps go to `dire`; ask if it's unclear whether the app should be
   public (`NAME.dire.nz`) or LAN-only (`NAME.code.dire.nz`). Run `kiwi-nest --cluster dire list` first: an
   existing name is a redeploy of that app, not a new one, and a domain in use is refused.
2. **Write `compose.yaml`** with `x-kiwi-nest`. Read `references/bundle-format.md` for the keys, the Compose subset
   (what's refused and why) and worked examples (static site, app with data, app + Postgres, basic auth,
   dependencies image, shared dataset). In short:
   - `domain`, `service`, `port` are required. Set `node_labels: { vm: "201" }` (public apps) or `{ vm: "203" }`.
   - **Set `idle_timeout` explicitly**: the default is `0`, always on. Use something like `"10m"` for apps that
     can sleep; keep `"0"` for anything with background work, schedules or long-lived connections.
   - **The app goes in the bundle, images hold dependencies.** Build the app locally into the bundle directory and
     mount it read-only. Use a stock image when one fits; otherwise build a dependencies-only image for
     `linux/amd64` and `kiwi-nest push` it. Never `build:`, and never copy images onto a node by hand.
   - **Data in named volumes** (`data: {}` under `volumes:`). Bundle files are always read-only.
   - **No `$VAR`**: write `$$` for a literal `$`. Settings go into a file in the bundle (an `env_file` the release
     script writes), never into the repo if they're secret.
3. **Protect it unless it's meant to be public.** `*.dire.nz` is the internet. For a private app, preview or anything
   showing data, check with the user, then add `auth` (bcrypt hashes, checked by Caddy before a request reaches the
   node or wakes the app), or use a `*.code.dire.nz` name.
4. **Build the bundle in a scratch directory** (`$TMPDIR`), not in the user's repo, with `compose.yaml` at its
   root. No ZIP step is needed: `deploy` takes the directory.
5. **Deploy:** `kiwi-nest --cluster dire deploy NAME ./bundle`. It prints the node and nest, then the result. A
   refused bundle lists every problem with where it is; fix them all and deploy again (nothing was saved).
6. **Verify:** `kiwi-nest --cluster dire inspect NAME`, then the URL: `curl -sS -o /dev/null -w '%{http_code}\n'
   https://NAME.dire.nz/` (or with `--resolve NAME.dire.nz:443:192.168.50.201` before the switch). 404 from Caddy:
   the domain isn't routed (wrong name, or DNS points elsewhere); 503: the app didn't start (check `logs` and
   `inspect`).
7. **Record it** in the VM's notes (`~/personal/proxmox/vms/201-dev.md` or `203-personal.md`, the Apps table), and
   remove the entry when the app is deleted.

A failed deploy of a new app leaves it `failed`: read `inspect` and `logs`, fix the bundle and deploy again. A failed
redeploy that couldn't stop the old generation leaves the old one running.

## Updating an app

`deploy` with the same name replaces the app in place: the node stops the old project and starts the new one (a few
seconds of downtime), and **its volumes stay**. So **never delete to redeploy**: delete removes the data. Keep the
volume names the same across versions, or the data won't be where the new version looks. There's no rollback
command; redeploy the previous bundle (keep it, or know how to rebuild it).

**Confirm with the user before `delete`** on anything but a throwaway or a preview whose pull request has closed,
and say that it removes the data. If they want the data kept, use `--keep-data` (the volumes come back with a later
deploy of the same name).

## Previews

`kiwi-nest deploy blog-pr12 ./bundle --from blog --domain blog-pr12.dire.nz --idle-timeout 10m --no-backups`:

- The preview joins blog's nest (so blog's node), and on its first deploy each named volume both bundles declare
  starts as a snapshot of blog's, taken while blog's containers are paused for milliseconds. The rest start empty.
  Snapshots share disk space until either side writes.
- **It's production's data in a preview**, running unreviewed code: accounts, password hashes, API tokens and the
  app's own secrets come along, and so does anything the app does by itself (schedules, emails, webhooks). Keep
  previews behind `auth` or on a LAN-only name, turn such jobs off for previews (an environment setting in the
  preview's bundle), and give the preview its own signing keys if the app keeps them in its data.
- To refresh a preview's data, delete it (which removes its data) and deploy again with `--from`. Deleting the
  preview never touches production's volumes.
- In CI, use the GitHub Action (below), which does exactly this and deletes the preview when the pull request
  closes.

## Behaviour worth knowing

- **Idle stops:** after `idle_timeout` with no HTTP requests through the gateway (open streams and WebSockets count
  as activity), the whole app gets `docker compose stop`, databases included. The next request starts it; a browser
  page load waiting more than about a second sees a "starting" page that reloads into the app. Background work and
  direct database connections don't count, so they need `idle_timeout: "0"`. A start must be healthy within the
  node's `--start-timeout` (2 minutes), and a failed start is retried only after 15 seconds.
- **Suspend is not idle:** a suspended app stays down until `resume`. For apps that should sleep, use
  `idle_timeout`, not `suspend`.
- **Healthchecks** gate readiness: a deploy or start waits for every container with one to be healthy, and the
  routed port to accept connections.
- **Images:** nodes remove images their apps stopped using after 24 hours; the registry's garbage collection deletes
  tags no app uses once they're more than 24 hours old. So a pushed image must be deployed within a day, and a
  redeploy may pull again: everything must be pullable.
- **Containers** are named `kn-APP-SERVICE-1` (project `kn-APP`, shown by `inspect`). There's no exec command: for
  a shell or a one-off command, `ssh ivan@192.168.50.201 docker exec -it kn-APP-SERVICE-1 sh` (203 for vm203).
- **Volumes on disk:** `VOLUMES/apps/APP/VOL` (vm201: `/var/lib/kiwi-nest/volumes`). Don't change them under a
  running app; suspend it first, and resume after.
- **Server and node logs:** on 201, `journalctl -u kiwi-nest-server`, `-u kiwi-nest-caddy`, `-u kiwi-nest-node`.

## CI

Read `references/ci.md`: deploy tokens scoped with `--pattern`/`--from`, installing the CLI in a workflow (as
`~/personal/docs-deploy` does), and the GitHub Action for previews. `kn.dire.nz` is LAN-only, so only self-hosted
runners on the LAN (the kiwicd agents and the trackmania runner on VM 204) can deploy.

## Moving an app from site-deploy

Read `references/from-site-deploy.md`: what changes in the bundle, the scripts and the workflows.

## Don't

- Don't deploy real apps to `home`, or try out a kiwi-nest release on `dire`: releases go to `home` first.
- Don't run `server init` again on VM 201 with 0.1.1 (what it runs now): that release puts Caddy's admin port back
  to 2019, which site-deploy's Caddy holds, and breaks the ingress. Releases after 0.1.1 keep the drop-in's
  `--caddy-admin http://127.0.0.1:2029` (or take `--caddy-admin`) (KN-21). Upgrades (`sudo kiwi-nest upgrade`) are
  fine.
- Don't print or commit tokens, and don't make admin tokens for CI: use deploy tokens.
- Don't change Caddy's configuration by hand: the server replaces it.
