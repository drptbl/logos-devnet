# Messaging

Two `logosctl` sessions, Alice and Bob, send a message and find each other's service through the devnet's messaging hub. Nothing goes through the public fleet: every node listens on `127.0.0.1`.

`logos-devnet.toml` here gives the example a devnet of its own on port 3044, the hub, and two sessions with `delivery_module` and `libp2p_module` installed. If something else holds 3044, `logos-devnet up --port <another>` picks another one, and it's remembered.

## What you need

`logos-devnet`, Docker, `logosctl` (the Logos Core CLI) on your `PATH`, and `jq`.

## Run it

From this directory:

```bash
logos-devnet up
source .logos-devnet/env
```

That starts the devnet, the hub and both sessions, and the env file now points at the hub. `call` runs a module's method in the session `LOGOSCTL_CONFIG_DIR` names, which `logos-devnet session env <name>` sets. Each session gets a delivery node from `LOGOS_DEVNET_DELIVERY_CONFIG`, so it joins the hub's private cluster instead of a public one:

```bash
call() { logosctl --json call "$@" | tail -1 | jq -c .result; }
for who in alice bob; do
  source <(logos-devnet session env $who)
  call delivery_module createNode "str:$LOGOS_DEVNET_DELIVERY_CONFIG"
  call delivery_module start
done
```

Bob listens on a content topic, and writes what his node receives to a file:

```bash
topic=/devnet-example/1/hello/proto
logosctl --json watch delivery_module > bob-events.log &
call delivery_module subscribe "str:$topic"
```

Alice waits until her node reaches the hub, then sends Bob a message:

```bash
source <(logos-devnet session env alice)
until [ "$(call delivery_module getConnectionStatus | jq -r .value)" != Disconnected ]; do sleep 1; done
call delivery_module send "str:$topic" str:hello-bob
sleep 5
jq -r 'select(.event == "messageReceived") | .data.arg2._bytes | @base64d' bob-events.log   # hello-bob
```

With the hub as their only peer, the nodes report `PartiallyConnected`, and that's enough. The message may arrive marked `history` rather than `live`: Bob's node picked it up from the hub's store.

For discovery, each session's `libp2p_module` node takes the hub as its only bootstrap node. Alice advertises a service, and Bob looks it up:

```bash
boot=$LOGOS_DEVNET_DISCOVERY_BOOTSTRAP
node="{\"bootstrapNodes\":[{\"peerId\":\"${boot##*/p2p/}\",\"addrs\":[\"${boot%/p2p/*}\"]}],\"mountKad\":true,\"mountServiceDiscovery\":true}"
for who in alice bob; do
  source <(logos-devnet session env $who)
  call libp2p_module createNode "str:$node"
  call libp2p_module start
  call libp2p_module discoStart
done
source <(logos-devnet session env alice)
call libp2p_module discoStartAdvertising str:devnet-example str:hello str:
source <(logos-devnet session env bob)
call libp2p_module discoRegisterInterest str:devnet-example
sleep 5
call libp2p_module discoLookup str:devnet-example str:hello | jq '.value[0].addrs'   # Alice's address
```

From a fresh devnet, all of it takes about a minute and a half, most of it installing modules.

The nodes live in the sessions' daemons, so `down` and `up` drop them: create them again after. The hub keeps its addresses, so the same config works.

## Clean up

```bash
kill %1   # Bob's watch
logos-devnet session rm alice && logos-devnet session rm bob
logos-devnet down
```

`logos-devnet messaging rm` deletes the hub, and deleting `.logos-devnet` removes the whole devnet.
