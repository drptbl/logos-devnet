# Messaging, discovery and storage through logosctl

What `delivery_module`, `libp2p_module` and `storage_module` expect when a session calls them. `source <(logos-devnet session env <name>)` points `logosctl` at a session's daemon; `logos-devnet exec --session <name> -- <command>` does the same for one command.

- Calls answer `{"success": ..., "value": ..., "error": ...}` in the reply's `result`: `logosctl --json call <module> <method> <args> | tail -1 | jq .result`. A call can succeed at the logosctl level and still carry `success: false`.
- Strings take a `str:` prefix. Numbers and booleans go bare.
- Uploads, downloads and received messages finish in the background and arrive as events: `logosctl --json watch <module> > events.log &` before the call, one line of JSON per event.

## Delivery (`[messaging]`)

With `[messaging]`, `up` gives a session that loads `delivery_module` (directly, or through `chat_module`) a node on the hub, started and connected. Don't create another.

- `getConnectionStatus` answers `PartiallyConnected` with the hub as the only peer: that's enough to send.
- `subscribe str:<topic>` listens on a content topic, e.g. `/my-app/1/chat/proto`.
- `send str:<topic> str:<payload>` sends.
- A received message is a `messageReceived` event: `.data.arg1` is the topic, `.data.arg2._bytes` the payload in base64 (`jq -r '.data.arg2._bytes | @base64d'`), and `.data.arg3` is `live`, or `history` when the node caught up from the hub's store.

## Discovery (`[messaging]`)

The session's `libp2p_module` node takes the hub as its only bootstrap node, also from `up`.

- `discoStartAdvertising str:<service> str:<data> str:` advertises a service. libp2p_module 1.1.0 advertises one service per load: `logos-devnet session restart <name>` starts it over.
- `discoRegisterInterest str:<service>`, then `discoLookup str:<service> str:<data>` a few seconds later: `.value` is a list of `{peerId, addrs, services}`.

## Storage (`[storage]`)

Every session's daemon starts its storage node on its own; with `[storage]` the sessions share a network through the hub.

- `uploadUrl str:<absolute path> 65536 true` uploads a file (chunk size, then whether to announce it). The CID comes in the `storageUploadDone` event: `jq -r 'select(.event == "storageUploadDone") | .data.arg0 | fromjson | .cid'`.
- `exists str:<cid>` says whether this node holds it.
- `downloadToUrl str:<cid> str:<absolute path> false 65536 false true` fetches it from the network (local only, chunk size, through Mix, announce). `storageDownloadDone` says when it's written.

https://github.com/drptbl/logos-devnet/tree/main/examples/messaging and https://github.com/drptbl/logos-devnet/tree/main/examples/storage run these end to end.
