# Sessions

Two `logosctl` sessions, Alice and Bob, each with a funded wallet open in `lez_core`, the way an app or an agent talks to Logos. Alice pays Bob, then shields some of her funds into a private account. The devnet runs in dev mode, so the shielded transfer's proof takes a second instead of minutes.

`logos-devnet.toml` here gives the example a devnet of its own on port 3042, next to a shared one on 3040.

## What you need

`logos-devnet`, Docker, `logosctl` (the Logos Core CLI) on your `PATH`, and `jq`.

## Run it

From this directory:

```bash
logos-devnet up
```

That starts the devnet and both sessions. Each one gets its own `logosctl` daemon with `lez_core` installed from the catalog, and a public account the faucet funded with 10B lepta. The commands below talk to whichever session `LOGOSCTL_CONFIG_DIR` names, which `logos-devnet session env <name>` sets:

```bash
call() { logosctl --json call lez_core "$@" | tail -1 | jq -r .result; }
rpc() {
  curl -fsS "$LOGOS_DEVNET_SEQUENCER" -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" | jq -r .result
}
source <(logos-devnet session env bob)
bob=${LOGOS_DEVNET_ACCOUNT#Public/}
source <(logos-devnet session env alice)
alice=${LOGOS_DEVNET_ACCOUNT#Public/}
```

Alice sends Bob 1000 lepta. `lez_core` takes account ids as hex and amounts as 16 little-endian bytes:

```bash
amount=e8030000000000000000000000000000
call transfer_public "str:$(call account_id_from_base58 "str:$alice")" \
  "str:$(call account_id_from_base58 "str:$bob")" "str:$amount"
sleep 2
rpc getAccountBalance "[\"$bob\"]"   # 10000001000
```

Then she shields 1000 lepta into a new private account of her own. Her session proves the transfer, and on a devnet without dev mode that takes minutes. Her wallet reads the private account's state once it has synced the new blocks:

```bash
private=$(call create_account_private)
call transfer_shielded_owned "str:$(call account_id_from_base58 "str:$alice")" "str:$private" "str:$amount"
sleep 2
call sync_to_block "$(rpc getLastBlockId '[]')"
call get_account_private "str:$private"   # a shard holding e803…, 1000 lepta
```

From a fresh devnet, all of it takes about 30 s, most of it starting the sessions.

`logos-devnet session rm alice` and `session rm bob` stop and delete the sessions, `logos-devnet down` stops the devnet, and deleting `.logos-devnet` removes it.
