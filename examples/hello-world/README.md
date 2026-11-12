# Hello world

LEZ's own program deployment tutorial on a devnet: you build its `hello_world` program, then deploy it and call it, paid by a funded test account. Once everything is built, a run takes about 15 s.

`logos-devnet.toml` here gives the example a devnet of its own on a free port, next to a shared one on 3040, with a group of two funded accounts. The first pays for the deploy and the call, and the second gets the greeting.

## What you need

- `logos-devnet` and Docker
- Rust, `jq`, and `cargo-risczero` 3.0, from RISC Zero's `rzup` or `cargo install --locked cargo-risczero --version 3.0.5`
- LEZ's tutorial, with its fix:

```bash
git clone https://github.com/drptbl/logos-execution-zone.git lez
git -C lez checkout 97d9a749ff48edf72c66ed2f835c67a8f5903a90
```

The tutorial in LEZ `v0.3.1` and `v0.3.2` can't run: its runners send calls without a fee payer, which the sequencer refuses. The fix is up for review as [logos-blockchain/logos-execution-zone#1102](https://github.com/logos-blockchain/logos-execution-zone/pull/1102). Use LEZ's own tag once one ships with it.

## Build the program

```bash
cd lez
RISC0_DOCKER_CONTAINER_TAG=r0.1.91.1 cargo risczero build \
  --manifest-path examples/program_deployment/methods/guest/Cargo.toml
```

LEZ builds its programs in RISC Zero's `r0.1.91.1` image. The default one is too old for its dependencies. The programs land in `lez/target/riscv32im-risc0-zkvm-elf/docker/`.

## Run it

From this directory, with `LEZ` set to your clone:

```bash
logos-devnet up
source .logos-devnet/env
export LEE_WALLET_HOME_DIR="$LOGOS_DEVNET_ACCOUNTS_TUTORIAL_WALLET"
payer=$(jq -r '.accounts[0].id' "$LOGOS_DEVNET_ACCOUNTS_TUTORIAL")
account=$(jq -r '.accounts[1].id' "$LOGOS_DEVNET_ACCOUNTS_TUTORIAL")
(cd "$LEZ/examples/program_deployment" && cargo run --bin run_hello_world \
  "$LEZ/target/riscv32im-risc0-zkvm-elf/docker/hello_world.bin" "$account" "$payer")
```

The runner signs with the group's wallet, which `LOGOS_DEVNET_ACCOUNTS_TUTORIAL_WALLET` points at. It deploys the program, sends the call and prints each transaction. It returns once the call is sent, so give it a block before reading the account:

```bash
sleep 2
curl -fsS "$LOGOS_DEVNET_SEQUENCER" -H 'content-type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getAccount\",\"params\":[\"$account\"]}" \
  | jq -r '.result.data.shards | to_entries[] | select(.key != "11111111111111111111111111111111") | .value | implode'
```

That prints `Hola mundo!`, which the program wrote into its shard of the account. The shard it skips, `1111…`, is the account's native token balance.

Each run deploys a new copy of the program. For a program your project deploys once, at an id that survives `reset`, name it under `[programs]` and read `LOGOS_PROGRAM_<NAME>`, as the main README describes.

`logos-devnet down` stops the devnet, and deleting `.logos-devnet` removes it.
