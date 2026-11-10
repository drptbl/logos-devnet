# logos-devnet

A local LEZ chain for Logos development. One command starts a sequencer on your machine with 1 second blocks and a funded faucet. Two more fund your accounts and deploy your programs. A transfer lands in about 2 seconds and a program deploy in about 10, and you never run out of testnet funds.

Keep the public testnet for final checks. Messaging (chat, discovery) isn't part of this: it stays on the Logos fleets.

## Install

```bash
cargo install --locked --git https://github.com/drptbl/logos-devnet
```

You need Docker (OrbStack, Docker Desktop or Colima). With `--native` you need git, a Rust toolchain and LEZ's build dependencies instead: clang, pkg-config and OpenSSL (`brew install pkg-config openssl` on macOS), plus `libpcsclite-dev` on Linux. The first native build takes a few minutes.

## Use it

```bash
logos-devnet up                                    # start, or resume
logos-devnet fund Public/<your account id>         # 10B lepta from the faucet
logos-devnet deploy my-program my_program.bin      # prints LOGOS_PROGRAM_MY_PROGRAM=<id>
logos-devnet wallet-config ~/my-app/devnet-wallet  # a wallet_config.json for lez_core
source ~/.logos-devnet/env                         # LOGOS_DEVNET_SEQUENCER, LOGOS_PROGRAM_*
```

| Command | What it does |
| --- | --- |
| `up` | Starts the devnet, or resumes it. Takes `--docker` or `--native`, `--lez <tag>`, `--port` and `--block-time`, and remembers them. `--rebuild` builds the image or binaries again. |
| `down` | Stops it. The chain is kept. |
| `reset` | Deletes the chain and starts a new one. Takes the same options as `up`. |
| `status` | What runs, the chain head, the faucet and deployed programs. |
| `fund <account> [amount]` | Sends lepta from the faucet to a public account, 10B by default. |
| `deploy <name> <elf>` | Deploys a program and adds `LOGOS_PROGRAM_<NAME>` and `LOGOS_PROGRAM_<NAME>_HEX` to the env file. Names are lower-case letters, digits and `-`. |
| `wallet-config <dir>` | Writes a `wallet_config.json` pointing at the devnet into a new wallet directory. |
| `env` | Prints the env file. |
| `logs [-f]` | Prints the sequencer's log. |

A bad option changes nothing: `up` and `reset` check it, and build what they need, before they stop the running chain.

## Point your project at it

The env file is the contract. A project reads `LOGOS_DEVNET_SEQUENCER` for the RPC URL, and `LOGOS_PROGRAM_<NAME>` (base58) or `LOGOS_PROGRAM_<NAME>_HEX` (what `lez_core` takes as `program_id_hex`) for its programs, falling back to the testnet values when they're not set.

Program ids stay the same. `deploy my-program` always deploys from the dev wallet's `program-my-program` header key, and a program id is that key's account id, so `reset` and redeploy give you the same `LOGOS_PROGRAM_MY_PROGRAM`. A second `deploy` of the same name on the same chain does nothing: `reset` first to deploy a new build.

For a wallet, give it its own directory. `wallet-config` refuses a directory that already holds a wallet, because repointing a testnet wallet at the devnet mixes up its sync state. Then create the wallet the usual way, e.g. `logosctl call lez_core create_new str:<dir>/wallet_config.json str:<dir>/storage.json str:`, and fund its public account.

## What runs

LEZ's own `sequencer_service` runs, built with two features. `standalone` replaces Bedrock, the L1, with an in-process mock that accepts every block. `prove` runs programs in-process, so you don't need `r0vm`. The default is `v0.3.1`, the release `lez_core` 0.5.1 is built on. `--lez` picks another tag, and a chain only runs one tag: `reset --lez <tag>` to switch.

| | devnet | testnet |
| --- | --- | --- |
| Block time | 1 s (`--block-time`) | 60 s, often several minutes |
| Funding | faucet, funded at genesis | testnet faucet |
| Base fee | 8 | 8 |
| Block timestamp | your clock | the sequencer's clock |
| L1 finality, indexer | none | yes |

Fees are the testnet's. Every call holds `gas_limit × base fee` up front, plus a little for its size, so about 1.6M lepta at a `gas_limit` of 200,000. The faucet makes that irrelevant.

Private calls still prove on your machine, so they take as long as on the testnet.

## Where things live

Everything is in `~/.logos-devnet` (or `LOGOS_DEVNET_HOME`):

| Path | |
| --- | --- |
| `settings.json` | LEZ tag, runtime, port, block time, faucet |
| `env` | the env file |
| `chain/` | the sequencer's config, database and keys. `reset` deletes it. |
| `wallet/` | the dev wallet: the faucet and each program's header key. Kept across resets. |
| `compose.yaml` | the Docker runtime's compose file |
| `native/` | the native runtime's LEZ checkout, build, log and pid file |

The RPC listens on `127.0.0.1` only. It has no authentication, so don't publish the port.

## The image

`ghcr.io/drptbl/logos-devnet-sequencer:<lez tag>` holds the sequencer and the LEZ `wallet` for `linux/amd64` and `linux/arm64`. `up` pulls it, and builds it locally from [`docker/Dockerfile`](docker/Dockerfile) when no image is published for your tag. To build it yourself:

```bash
docker build --build-arg LEZ_TAG=v0.3.1 -t ghcr.io/drptbl/logos-devnet-sequencer:v0.3.1 - < docker/Dockerfile
```

## License

MIT or Apache-2.0, at your option.
