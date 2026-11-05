# kiwi-nest

A multi-node hosting system for PR preview environments and general-purpose hosting.

Apps run as single instances on a small cluster of machines. Apps that share data, such as a production app and its
pull-request previews, live together in a *nest* on one node, so previews can start from instant btrfs snapshots of
production's data. Idle apps stop and wake on their next request.

kiwi-nest is at the design stage: see [docs/design.md](docs/design.md) for the design and the build plan.
