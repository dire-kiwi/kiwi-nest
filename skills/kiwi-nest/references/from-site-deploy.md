# Moving an app from site-deploy

The apps on Proxmox VMs 201 and 203 move to the `dire` cluster in PRO-9 (plan:
`/home/ivan/.local/share/huntaway-effect/documents/727103c4-9deb-4236-af66-46b6f755306c/plan.html`; each step is a
board task in the pen "site-deploy → kiwi-nest"). site-deploy keeps its other, non-Proxmox hosts.

## What changes for an app

| site-deploy | kiwi-nest | What to do |
|---|---|---|
| `x-site-deploy` | `x-kiwi-nest` (`domain`, `service`, `port`, `idle_timeout`) | Rename. **Set `idle_timeout` explicitly**: kiwi-nest's default is `0` (always on), site-deploy's was `5m` |
| `basic_auth` | `auth` (the same bcrypt hashes) | Rename |
| `start_timeout`, `suspend_mode` | none (the node's `--start-timeout`, 2m by default) | Drop them. Apps that need longer than 2 minutes to start need the node's flag changed |
| Preview data copied, or snapshots of site-deploy volumes | The app's own named volumes; previews `deploy --from APP` (btrfs snapshots) | No more copies, forced SSH commands or "start fresh" markers. Shared datasets become shared volumes (`x-kiwi-nest.shared`) |
| `ports`, `deploy.resources`, `security_opt`, `configs`/`secrets`, writable bundle binds, `$VAR` | Refused | Remove them; use `mem_limit`/`cpus`, a named volume for writes, files in the bundle, and `$$` for a literal `$` |
| The host it runs on (`--host 201`) | `x-kiwi-nest.node_labels: { vm: "201" }` (or `"203"`), applied when the app's nest is made | Add it |
| `docker-compose.yml`, `compose.yml` | `compose.yaml` only | Rename |
| `127.0.0.1:9082/NAME:TAG`, `site-deploy push` | `registry.dire.nz/NAME:TAG`, `kiwi-nest push` | Push the images again and change the references |
| `delete` keeps named volumes | **`delete` removes the volumes** unless `--keep-data` | Scripts never delete before a redeploy: `deploy` replaces in place. Preview teardown keeps the default (data goes) |
| A suspended app wakes on a request | Only `resume` starts a suspended app | Use `idle_timeout` for apps that sleep, not `suspend` |
| `site-deploy ssh -- docker exec …` | No exec command | `ssh ivan@HOST docker exec kn-APP-SERVICE-1 …` (the container's name is in `inspect`) |
| SSH tunnel and a restricted key for CI | HTTPS API with a deploy token scoped by name (`--pattern 'code-pr*' --from code`) | The CI SSH keys and forced commands on 201 and 203 go |
| One ZIP built by `bundle.sh` | A directory (or a ZIP) | `kiwi-nest deploy NAME ./dir`: no ZIP step |

Other differences worth knowing:

- **Status words:** `running`, `idle` (stopped by the idle timeout; a request starts it), `suspended`, `failed`,
  `deploying`.
- **`X-Forwarded-Proto: https`** reaches apps behind the tunnel (kiwi-nest's Caddy trusts cloudflared on loopback),
  so apps can set `Secure` cookies and origin checks pass. Workarounds for site-deploy's `http` can go.
- **Names and domains belong to the cluster**, not a host: a name or domain in use on any node is refused.

## Moving the data

kiwi-nest can't import Docker named volumes: an app's volume is a btrfs subvolume at `VOLUMES/apps/APP/VOL` on its
node (vm201: `/var/lib/kiwi-nest/volumes`). For each app with data:

1. **Stage:** `kiwi-nest --cluster dire deploy APP ./bundle` (its `node_labels` put it on the VM where the data
   is), then `kiwi-nest --cluster dire suspend APP`.
2. **Practice copy:** on that VM, `sudo rsync -aHAX --delete /var/lib/docker/volumes/SD_PROJECT_VOL/_data/
   /var/lib/kiwi-nest/volumes/apps/APP/VOL/`, then `resume APP` and check it on the LAN
   (`curl --resolve APP.dire.nz:443:192.168.50.201 https://APP.dire.nz/`).
3. **At the switch:** stop the site-deploy app, `suspend` the kiwi-nest one, run the final rsync, `resume`, and move
   the traffic (the tunnel's ingress for 201, DNS for 203).

Leave the source volumes untouched until site-deploy is retired, so going back is just pointing the traffic back.
