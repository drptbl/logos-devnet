# Examples

`hello-world`, `sessions`, `messaging` and `storage` run with [logos-devnet v0.9.4](https://github.com/drptbl/logos-devnet/releases/tag/v0.9.4) and LEZ v0.3 for now: sessions wait for a `lez_core` built for LEZ v0.4, and LEZ's tutorial for a v0.4 tag. They'll be updated when those ship.

| Example | |
| --- | --- |
| [`hello-world`](hello-world) | LEZ's tutorial on a devnet: build a program, deploy it and call it, paid by a funded test account |
| [`sessions`](sessions) | two `logosctl` sessions paying each other in dev mode, publicly and privately |
| [`messaging`](messaging) | two `logosctl` sessions sending a message and finding each other's service through the local messaging hub |
| [`storage`](storage) | two `logosctl` sessions sharing a file through a local Logos Storage network: one uploads, the other downloads by CID |
| [`github-actions`](github-actions) | a workflow that runs your tests against a devnet in CI |
