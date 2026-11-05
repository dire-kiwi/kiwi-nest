# Installing kiwi-nest

This sets up a cluster from scratch: one machine as the server and the ingress, and one or more nodes that run the
apps. It's the same for two VMs on a home LAN as for a few machines in a VPC. The design is in
[design.md](design.md); this is the how-to.

- [What you need](#what-you-need)
- [Getting kiwi-nest](#getting-kiwi-nest)
- [The server](#the-server)
- [Nodes](#nodes)
- [Your workstation](#your-workstation)
- [Previews from pull requests](#previews-from-pull-requests)
- [Upgrades and rollbacks](#upgrades-and-rollbacks)
- [Each host's own settings](#each-hosts-own-settings)
- [Where things are](#where-things-are)

## What you need

- **Machines**: Linux, amd64 or arm64, with systemd. One is the server; it runs the ingress (Caddy) and, if you
  want one, the image registry. It can be a node too. Every node needs **Docker Engine with the Compose plugin**,
  and a **btrfs filesystem** for app data: a disk of its own, or a loop-mounted file (below). The server's host needs
  Docker only for the registry.
- **A private network** between them: a LAN, a VPC, WireGuard or Tailscale. Nodes connect out to the server's API
  over HTTPS; the ingress reaches each node's gateway on the node's private address (port 7382), which should be
  firewalled so only the server's host reaches it.
- **DNS**: a name for the server's API (`kn.example.com`), the registry's if you run one (`registry.example.com`)
  and the apps' (a wildcard, `*.example.com`), all pointing at the server's host.
- **A TLS mode** for the ingress:
  - `acme`: certificates from Let's Encrypt (or ZeroSSL), for names the internet can reach on ports 80 and 443.
  - `internal`: Caddy's own CA, for a cluster reached only on the LAN or over a VPN. Nodes, the CLI and Docker then
    trust that CA's root, which the server writes to `/var/lib/kiwi-nest-server/ingress-ca.crt`.
  - `certs`: certificates you get some other way (a DNS-01 wildcard, say), as `.pem` files in a directory.
  - `http`: plain HTTP, behind a proxy or tunnel of your own that does TLS.

## Getting kiwi-nest

Every release on [GitHub](https://github.com/dire-kiwi/kiwi-nest/releases) has one file per target and a
`SHA256SUMS` listing them: `kiwi-nest-VERSION-x86_64-unknown-linux-musl` and `…-aarch64-unknown-linux-musl` (static
binaries: server, node and CLI in one), and the CLI for macOS, `…-aarch64-apple-darwin` and `…-x86_64-apple-darwin`.
Download yours and check it:

```sh
V=0.2.0   # the release
T=x86_64-unknown-linux-musl
curl -fLO https://github.com/dire-kiwi/kiwi-nest/releases/download/v$V/kiwi-nest-$V-$T
curl -fLO https://github.com/dire-kiwi/kiwi-nest/releases/download/v$V/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS     # macOS: shasum -a 256 --check --ignore-missing SHA256SUMS
chmod +x kiwi-nest-$V-$T
```

`server init` and `node join` install the binary they're run from, so run them from that file; afterwards
`kiwi-nest` is on the PATH (`/usr/local/bin/kiwi-nest`). A build of your own (`make release`, which writes the same
files into `dist/`) works the same way.

## The server

On the server's host, as root:

```sh
sudo ./kiwi-nest-$V-$T server init \
  --api-domain kn.example.com \
  --ingress acme --acme-email you@example.com \
  --registry-domain registry.example.com        # optional; needs Docker on this host
```

Other modes: `--ingress internal`, `--ingress certs --certs-dir /etc/kiwi-nest/certs`, `--ingress http`. Add
`--backup-targets /etc/kiwi-nest/backup-targets.yaml` for backups (see the design's Backups; the file, and the
files it names, must be readable by the `kiwi-nest` user, say `root:kiwi-nest` mode 0640).

It checks the machine (ports 80 and 443 free, Docker for the registry), then:

- installs itself as a release under `/usr/local/lib/kiwi-nest`, linked from `/usr/local/bin/kiwi-nest`;
- makes the users `kiwi-nest` (the server) and `kiwi-nest-caddy` (the ingress), and the server's state directory,
  `/var/lib/kiwi-nest-server`;
- installs Caddy (a pinned release, downloaded and checked against the SHA-512 kiwi-nest records; `--caddy FILE`
  uses one you have), with nothing but its admin API on loopback: the server configures the rest;
- writes the units `kiwi-nest-server.service` and `kiwi-nest-caddy.service`, and the server's flags in
  `kiwi-nest-server.service.d/host.conf`, then enables and starts them;
- makes the first admin token and prints it, with the commands to use it. It's shown only then.

Running it again rewrites the units and the drop-in from its flags, and makes no new token.

## Nodes

**btrfs.** A spare disk is simplest:

```sh
mkfs.btrfs /dev/sdb
mkdir -p /var/lib/kiwi-nest/volumes
echo '/dev/sdb /var/lib/kiwi-nest/volumes btrfs defaults 0 0' >> /etc/fstab
mount /var/lib/kiwi-nest/volumes
```

Or a file on the root filesystem, loop-mounted:

```sh
truncate -s 50G /var/lib/kiwi-nest-volumes.btrfs
mkfs.btrfs /var/lib/kiwi-nest-volumes.btrfs
mkdir -p /var/lib/kiwi-nest/volumes
echo '/var/lib/kiwi-nest-volumes.btrfs /var/lib/kiwi-nest/volumes btrfs loop 0 0' >> /etc/fstab
mount /var/lib/kiwi-nest/volumes
```

**Join.** Make a join token (on your workstation, once the cluster is added there, or on the server's host with
`--server http://127.0.0.1:7400 --token ADMIN`): `kiwi-nest token create join`. Then on the node, as root:

```sh
sudo ./kiwi-nest-$V-$T node join https://kn.example.com --token knj_…
# with --ingress internal, add: --ca-cert ingress-ca.crt   (copied from the server)
```

It checks everything first and lists every problem at once: Docker and the Compose plugin, the volumes directory
(`--volumes-dir`, `/var/lib/kiwi-nest/volumes` by default) on btrfs and in `/etc/fstab`, the node's private address
(`--address`; by default the address it reaches the server from, which must be private unless
`--allow-public-address`), and that the server answers. Then it joins (so a bad token fails here), installs itself,
writes `kiwi-nest-node.service` and its `host.conf` (which also makes the unit wait for the volumes' mount), and
starts it. With `--ca-cert` and a registry, Docker is told to trust the CA for the registry too
(`/etc/docker/certs.d/REGISTRY/ca.crt`). `--name`, `--label disk=ssd` and `--allow-host-path DIR` are as for
`kiwi-nest node`.

**Firewall** the gateway's port to the server's host; `node join` prints the address. With ufw:

```sh
ufw allow from 192.168.1.10 to any port 7382 proto tcp
ufw deny 7382/tcp
```

## Your workstation

Install the CLI for your machine as above, somewhere on your PATH, then:

```sh
kiwi-nest cluster add home https://kn.example.com --token kna_…     # --ca-cert ingress-ca.crt with internal
kiwi-nest node list
kiwi-nest deploy blog ./blog                                       # see the design's Bundles
kiwi-nest user add you                                             # for the web UI, at https://kn.example.com
```

The CLI must be at least as new as the server: an older one is refused with a message saying which version to get.

## Previews from pull requests

The repository's own GitHub Action (`action.yml` at the root of this repository) deploys each pull request's
preview `--from` production, so it lands in production's nest and starts from a snapshot of production's data, and
deletes it when the pull request closes. Make a deploy token that can only manage the previews, and may use
production as `--from`:

```sh
kiwi-nest token create deploy --name blog-previews --pattern 'blog-pr*' --from blog
```

Put it in the repository's secrets as `KIWI_NEST_PREVIEW_TOKEN`, and add a workflow:

```yaml
name: Preview
on:
  pull_request:
    types: [opened, synchronize, reopened, closed]
concurrency: preview-${{ github.event.number }}
jobs:
  preview:
    runs-on: ubuntu-latest      # it must reach the server's API: a self-hosted runner for a LAN-only cluster
    steps:
      - uses: actions/checkout@v4
      - uses: dire-kiwi/kiwi-nest@v0.2.0
        id: preview
        with:
          server: https://kn.example.com
          token: ${{ secrets.KIWI_NEST_PREVIEW_TOKEN }}
          app: blog
          domain: "{name}.example.com"     # blog-pr12.example.com
          bundle: .
      - run: echo "${{ steps.preview.outputs.url }}"
```

The Action installs the CLI of the server's version (or `version:`), checked against the release's `SHA256SUMS`,
and deploys `blog-prNUMBER` with the domain given, `idle_timeout: 10m` and no scheduled backups (`idle-timeout:` and
`backups:` change that; they're applied with `kiwi-nest deploy --domain --idle-timeout --no-backups`, which rewrite
the bundle's `x-kiwi-nest` as it's uploaded). The first deploy snapshots production's volumes; later pushes keep
the preview's data. `ca-cert:` takes the internal CA's PEM, from a secret.

## Upgrades and rollbacks

On the server's host, as root:

```sh
sudo kiwi-nest upgrade                 # the latest release; --version 0.3.0 for another
sudo kiwi-nest upgrade --from ./dist   # or a directory of release files with their SHA256SUMS
```

It downloads the release for every Linux target and checks each against `SHA256SUMS`, checks this machine's runs,
copies the server's database (to `/var/lib/kiwi-nest-server/upgrades/`), makes the release current, rewrites the
units (never the drop-ins) and restarts the server. The new server then **upgrades the nodes itself, one at a
time**: each downloads its architecture's release from the server, checks its size and checksum and that it runs,
finishes what it's applying, swaps the release in and restarts into it. Apps keep running; each node's gateway is
down for the moment its agent restarts. Once a node polls again running the new release, the next one's turn comes.

With an admin token in reach (a cluster added for root, or `--server` and `--token`), `upgrade` follows the
rollout; otherwise `kiwi-nest upgrade status` from your workstation shows it. A node that refuses the release, or
doesn't come back running it within three minutes, **halts the rollout**, so a bad release stops at one node:
`upgrade status` says why, and `kiwi-nest upgrade nodes` carries on (trying that node again). The server keeps
serving nodes one agent-link version behind it, so a cluster mid-rollout works as usual. Nodes from before upgrades
existed, or not installed by `node join`, are listed as `manual`: run `sudo kiwi-nest upgrade` on them.

Old releases stay installed (the three newest, with `--keep`), so:

```sh
sudo kiwi-nest rollback            # the previous release; or rollback 0.2.0
```

makes it current again and restarts what's installed on the machine. If the newer release changed the server's
database, the older server refuses it ("from a newer kiwi-nest"): stop the server, put back the copy from
`/var/lib/kiwi-nest-server/upgrades/` as `server.sqlite` (removing `server.sqlite-wal` and `-shm`), and start it;
what changed since is lost. Nodes newer than a rolled-back server keep their release; roll them back too, on each,
if the server can't talk to them.

## Each host's own settings

The units are the same everywhere, and upgrades rewrite them. Each host's own flags are in a drop-in that upgrades
never touch:

```ini
# /etc/systemd/system/kiwi-nest-node.service.d/host.conf
[Unit]
RequiresMountsFor=/var/lib/kiwi-nest/volumes
[Service]
Environment="KIWI_NEST_ARGS=--server https://kn.example.com --name node-a --ingress-listen 192.168.1.21:7382 --volumes-dir /var/lib/kiwi-nest/volumes"
```

Change the flags there (any of `kiwi-nest node --help`'s, or `server --help`'s for the server), or add drop-ins of
your own beside it (`Environment=KIWI_NEST_LOG=debug` in `debug.conf`, say), then `systemctl daemon-reload` and
restart the unit. Flags can't hold spaces or quotes.

## Where things are

```text
/usr/local/lib/kiwi-nest/releases/VERSION/kiwi-nest   each release; dist/ beside it on the server's host
/usr/local/lib/kiwi-nest/current -> releases/VERSION  the one in use
/usr/local/bin/kiwi-nest                              -> current/kiwi-nest
/usr/local/lib/kiwi-nest/caddy/caddy                  the ingress's Caddy
/etc/systemd/system/kiwi-nest-{server,caddy,node}.service, and *.service.d/host.conf
/etc/kiwi-nest/caddy.json                             Caddy's boot configuration: its admin API on loopback
/etc/kiwi-nest/server-ca.crt                          on nodes: the CA given to `node join --ca-cert`
/var/lib/kiwi-nest-server/                            the server's database, bundles, registry, ingress CA
/var/lib/kiwi-nest-caddy/                             Caddy's certificates and ACME account
/var/lib/kiwi-nest/                                   a node's state; its volumes in volumes/ (btrfs)
```

Logs: `journalctl -u kiwi-nest-server`, `-u kiwi-nest-caddy`, `-u kiwi-nest-node`.
