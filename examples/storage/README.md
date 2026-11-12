# Storage

Two `logosctl` sessions, Alice and Bob, share a file through Logos Storage. Alice uploads it, and Bob downloads it by its CID. Nothing goes through the public storage network: every node listens on `127.0.0.1`.

`logos-devnet.toml` here gives the example a devnet of its own on port 3045, two sessions, and `[storage]`, which runs a storage node in the hub that both sessions' nodes bootstrap from. Each session's daemon starts its storage node on its own, so there's nothing to create. If something else holds 3045, `logos-devnet up --port <another>` picks another one, and it's remembered.

## What you need

`logos-devnet`, Docker, `logosctl` (the Logos Core CLI) on your `PATH`, and `jq`.

## Run it

From this directory:

```bash
logos-devnet up
logos-devnet status | grep storage   # storage    running, the sessions' bootstrap node
```

`call` runs a `storage_module` method in the session `LOGOSCTL_CONFIG_DIR` names, which `logos-devnet session env <name>` sets. Uploads and downloads finish in the background and say so in events, so each session writes its node's events to a file:

```bash
call() { logosctl --json call storage_module "$@" | tail -1 | jq -c .result; }
for who in alice bob; do
  source <(logos-devnet session env $who)
  logosctl --json watch storage_module > $who-events.log &
done
sleep 5
```

Alice uploads a file. The CID comes with the `storageUploadDone` event:

```bash
source <(logos-devnet session env alice)
echo "hello from alice" > hello.txt
call uploadUrl "str:$PWD/hello.txt" 65536 true
sleep 3
cid=$(grep storageUploadDone alice-events.log | tail -1 | jq -r '.data.arg0 | fromjson | .cid')
echo "$cid"   # zDvZ...
```

Bob doesn't have it yet, so his node fetches it from Alice's through the network the hub bootstraps:

```bash
source <(logos-devnet session env bob)
call exists "str:$cid"   # "value":false
call downloadToUrl "str:$cid" "str:$PWD/from-alice.txt" false 65536 false true
sleep 5
cat from-alice.txt   # hello from alice
```

`uploadUrl` takes a file's absolute path, a chunk size and whether to announce it. `downloadToUrl` takes the CID, where to write it, whether to read only what's stored locally, a chunk size, whether to go through Mix, and whether to announce it.

From a fresh devnet, all of it takes about a minute and a half, most of it starting the sessions. Without `[storage]`, each session still has a storage node, but in no network: Bob's download would find nothing.

## Clean up

```bash
kill %1 %2   # the event watches
logos-devnet session rm alice && logos-devnet session rm bob
logos-devnet down
```

`logos-devnet messaging rm` deletes the hub, and deleting `.logos-devnet` removes the whole devnet.
