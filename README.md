# kiwi-nest

A multi-node hosting system for PR preview environments and general-purpose hosting.

Apps run as single instances on a small cluster of machines. Apps that share data, such as a production app and its
pull-request previews, live together in a *nest* on one node, so previews can start from instant btrfs snapshots of
production's data. Idle apps stop and wake on their next request.

kiwi-nest is being built: see [docs/design.md](docs/design.md) for the design and the build plan. So far a single node
runs apps from bundles (`kiwi-nest node --standalone`), with their data in btrfs subvolumes, driven by the CLI
(`kiwi-nest deploy|list|inspect|logs|delete`, `deploy --from` for previews, and `kiwi-nest volume …` for shared
volumes).
