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
(`kiwi-nest node --standalone`). `scripts/dev-cluster.sh` runs a server and two nodes on one machine.
