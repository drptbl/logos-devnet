# lez_core through logosctl

What `lez_core`, the LEZ module in a `logosctl` session, expects. A session's env (`source <(logos-devnet session env <name>)`) points `logosctl` at its daemon, where the wallet is open.

- Account ids are hex. A session's `LOGOS_DEVNET_ACCOUNT_HEX` is its account in that form, a group's `accounts.json` has each account's `hex`, and `logos-devnet account <id>` prints any id in both forms.
- Amounts are 16 little-endian bytes in hex: 1000 lepta is `e8030000000000000000000000000000`.
- Strings take a `str:` prefix. Booleans go bare, as `true` or `false`.
- `get_balance str:<hex> true` reads a public account and `... false` a private one, in lepta. An empty result means the call failed.
- A transfer returns its `tx_hash` in a JSON string: `| jq -r .result | jq -r .tx_hash`. `logos-devnet tx --wait <hash>` returns once a block holds it, instead of a `sleep`.
- A private balance needs the wallet synced first: `call lez_core sync_to_block <block>`, with the block from `logos-devnet tx --wait --json <hash> | jq .block`.
- `transfer_public str:<from> str:<to> str:<amount>` pays a public account. `transfer_shielded_owned` with the same arguments shields funds into one of the wallet's own private accounts, which `create_account_private` makes. In dev mode its proof takes about a second.

https://github.com/drptbl/logos-devnet/tree/main/examples/sessions runs these end to end.
