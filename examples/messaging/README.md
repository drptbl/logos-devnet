# Messaging

Two `logosctl` sessions, Alice and Bob, send a message and find each other's service through the devnet's messaging hub. Nothing goes through the public fleet: every node listens on `127.0.0.1`.

`logos-devnet.toml` here gives the example a devnet of its own on a free port, the hub, and two sessions with `delivery_module` and `libp2p_module` installed.

## What you need

`logos-devnet`, Docker, `logosctl` (the Logos Core CLI) on your `PATH`, and `jq`.

## Run it

From this directory:

```bash
logos-devnet up
source .logos-devnet/env
```

That starts the devnet, the hub and both sessions. Each session gets a delivery node and a discovery node on the hub, already started and connected, so it joins the hub's private network instead of a public one. `call` runs a module's method in the session `LOGOSCTL_CONFIG_DIR` names, which `logos-devnet session env <name>` sets:

```bash
call() { logosctl --json call "$@" | tail -1 | jq -c .result; }
source <(logos-devnet session env alice)
call delivery_module getConnectionStatus   # "value":"PartiallyConnected"
```

Bob listens on a content topic, and writes what his node receives to a file:

```bash
source <(logos-devnet session env bob)
topic=/devnet-example/1/hello/proto
logosctl --json watch delivery_module > bob-events.log &
call delivery_module subscribe "str:$topic"
```

Alice sends Bob a message:

```bash
source <(logos-devnet session env alice)
call delivery_module send "str:$topic" str:hello-bob
sleep 5
jq -r 'select(.event == "messageReceived") | .data.arg2._bytes | @base64d' bob-events.log   # hello-bob
```

With the hub as their only peer, the nodes report `PartiallyConnected`, and that's enough. The message may arrive marked `history` rather than `live`: Bob's node picked it up from the hub's store.

For discovery, each session's `libp2p_module` node takes the hub as its only bootstrap node. Alice advertises a service, and Bob looks it up:

```bash
source <(logos-devnet session env alice)
call libp2p_module discoStartAdvertising str:devnet-example str:hello str:
source <(logos-devnet session env bob)
call libp2p_module discoRegisterInterest str:devnet-example
sleep 5
call libp2p_module discoLookup str:devnet-example str:hello | jq '.value[0].addrs'   # Alice's address
```

From a fresh devnet, all of it takes a few minutes, nearly all of it installing modules from the catalog.

The nodes live in the sessions' daemons, so `down` drops them, and `up` makes them again on the hub. An app outside a session creates its own from `LOGOS_DEVNET_DELIVERY_CONFIG` and `LOGOS_DEVNET_DISCOVERY_CONFIG`.

## Clean up

```bash
kill %1   # Bob's watch
logos-devnet session rm alice && logos-devnet session rm bob
logos-devnet down
```

`logos-devnet messaging rm` deletes the hub, and deleting `.logos-devnet` removes the whole devnet.
