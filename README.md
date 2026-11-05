# kiwi-nest

A multi-node hosting system for PR preview environments and general-purpose hosting.

Apps run as single instances on a small cluster of machines. Apps that share data, such as a production app and its
pull-request previews, live together in a *nest* on one node, so previews can start from instant btrfs snapshots of
production's data. Idle apps stop and wake on their next request.

kiwi-nest is being built: see [docs/design.md](docs/design.md) for the design and the build plan. So far a server
(`kiwi-nest server`) keeps the cluster's inventory and tokens, and nodes join it (`kiwi-nest node --server URL
--join-token TOKEN`) and long-poll it for what to run. Nodes run apps from bundles, with their data in btrfs
subvolumes and a gateway on loopback that starts stopped apps for their requests and stops idle ones. The CLI
(`kiwi-nest deploy|list|inspect|logs|suspend|resume|delete`, `deploy --from` for previews, `kiwi-nest volume …` for
shared volumes, `kiwi-nest token …`, `kiwi-nest cluster …`) goes through the server, or to a standalone node
(`kiwi-nest node --standalone`). The server also serves a web UI (`kiwi-nest user add NAME`, then log in at the
server's address) showing the nodes, nests, apps, volumes and backups, with suspend, resume, logs, back up now and
cordon. `scripts/dev-cluster.sh` runs a server and two nodes on one machine.

**Installing a cluster:** [docs/install.md](docs/install.md). In short, from a release's binary (checked against
its `SHA256SUMS`): `sudo kiwi-nest server init --api-domain kn.example.com --ingress acme --acme-email …` on the
server's host, `sudo kiwi-nest node join https://kn.example.com --token JOIN` on each node, and later
`sudo kiwi-nest upgrade` on the server's host, which then upgrades the nodes one at a time while their apps keep
running. The repository's GitHub Action (`action.yml`) deploys pull requests' previews `--from` production and
deletes them when they close.
