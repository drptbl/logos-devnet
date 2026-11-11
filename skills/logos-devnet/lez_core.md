# lez_core through logosctl

What `lez_core`, the LEZ module in a `logosctl` session, expects. A session's env (`source <(logos-devnet session env <name>)`) points `logosctl` at its daemon, where the wallet is open.

- Account ids are hex. `logosctl --json call lez_core account_id_from_base58 str:<base58>` converts the `Public/<id>` a session prints. A group's `accounts.json` already has each account's `hex`.
- Amounts are 16 little-endian bytes in hex: 1000 lepta is `e8030000000000000000000000000000`.
- Strings take a `str:` prefix. Booleans go bare, as `true` or `false`.
- `get_balance str:<hex> true` reads a public account and `... false` a private one, in lepta. An empty result means the call failed.
- A private balance needs the wallet synced first: `call lez_core sync_to_block <head>`, with the head from the RPC's `getLastBlockId`.
- `transfer_public str:<from> str:<to> str:<amount>` pays a public account. `transfer_shielded_owned` with the same arguments shields funds into one of the wallet's own private accounts, which `create_account_private` makes. In dev mode its proof takes about a second.

`examples/sessions` in the repo runs these end to end.
