# logos-devnet

A local LEZ chain for Logos development. One command starts a sequencer on your machine with 1 second blocks and a funded faucet, and a project file in your repo says what else it needs: programs, a hundred funded test accounts, `logosctl` sessions. A transfer lands in about 2 seconds and a program deploy in about 10, and you never run out of testnet funds.

Keep the public testnet for final checks. Messages and service discovery can stay local too: a project that asks for it gets a messaging hub on `127.0.0.1` instead of the Logos fleets.

## Install

```bash
cargo install --locked logos-devnet
```

Or take a binary from the [latest release](https://github.com/drptbl/logos-devnet/releases/latest), built for macOS (`aarch64-apple-darwin`, `x86_64-apple-darwin`) and Linux (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, static, so any distribution), and check it against its `.sha256`:

```bash
v=v0.7.0 t=aarch64-apple-darwin
curl -fsSL --remote-name-all https://github.com/drptbl/logos-devnet/releases/download/$v/logos-devnet-$v-$t.tar.gz{,.sha256}
shasum -a 256 -c logos-devnet-$v-$t.tar.gz.sha256 \
  && tar -xzf logos-devnet-$v-$t.tar.gz \
  && mkdir -p ~/.local/bin && install logos-devnet-$v-$t/logos-devnet ~/.local/bin/
```

Put `~/.local/bin` on your `PATH` if it isn't. `cargo install --locked --git https://github.com/drptbl/logos-devnet` builds the latest `main`.

It runs on macOS and Linux, and needs Docker (OrbStack, Docker Desktop or Colima). There's no Windows build. With `--native` you need git, a Rust toolchain and LEZ's build dependencies instead: clang, pkg-config and OpenSSL (`brew install pkg-config openssl` on macOS), plus `libpcsclite-dev` on Linux. The first native build takes a few minutes.

## Use it

```bash
logos-devnet doctor                                # what's missing, and how to fix it
logos-devnet up                                    # start, or resume
logos-devnet fund Public/<your account id>         # 10B lepta from the faucet
logos-devnet deploy my-program my_program.bin      # prints LOGOS_PROGRAM_MY_PROGRAM=<id>
logos-devnet wallet-config ~/my-app/devnet-wallet  # a wallet_config.json for lez_core
source ~/.logos-devnet/env                         # LOGOS_DEVNET_SEQUENCER, LOGOS_PROGRAM_*
```

| Command | What it does |
| --- | --- |
| `up` | Starts the devnet, or resumes it, and makes it match your `logos-devnet.toml`. Takes `--docker` or `--native`, `--lez <tag>` (a v0.3 tag), `--port`, `--block-time`, `--max-txs-per-block`, `--dev-mode` and `--log-filter`, and remembers them. `--rebuild` builds the image or binaries again. |
| `down` | Stops it. The chain is kept. |
| `reset` | Deletes the chain and starts a new one, every account group funded at genesis. Takes the same options as `up`. |
| `status` | What runs, the chain head, the faucet, programs, account groups, sessions, the messaging hub and local storage. |
| `doctor` | Checks the project file, Docker and the image (or git and cargo with `--native`), whether the chain takes the project's settings, the port, the faucet and, when sessions or the messaging hub need it, `logosctl`, and says how to fix what fails. |
| `fund <account> [amount]` | Sends lepta from the faucet to a public account, 10B by default. |
| `deploy <name> <bin> [--mutable]` | Deploys a program, from its `.bin` (what `cargo risczero build` makes, not the bare ELF), and adds `LOGOS_PROGRAM_<NAME>` and `LOGOS_PROGRAM_<NAME>_HEX` to the env file. Names are lower-case letters, digits and `-`. With `--mutable`, a new build updates it in place. |
| `wallet-config <dir>` | Writes a `wallet_config.json` pointing at the devnet into a new wallet directory. |
| `env` | Prints the env file. |
| `exec [--session <name>] -- <cmd>` | Runs a command with the devnet's env, and a session's with `--session`, and exits with its code: no `source` in Makefiles and scripts. |
| `tx <hash> [--wait] [--timeout <s>]` | Shows which block holds a transaction, by its 64-hex hash. `--wait` waits until one does, instead of a `sleep`, and fails after 60 s. |
| `account <id>` | Shows an account's balance, nonce and data, and its id in base58 and hex. Takes `Public/<id>`, base58, hex as `lez_core` prints it, or a deployed program's name. |
| `logs [-f] [--session <name> \| --hub]` | Prints the sequencer's log, or a session's or the hub's daemon log, with its modules' output. |
| `session new <name> [--module <m>]` | Makes a `logosctl` session with a funded devnet wallet, see below. |
| `session ls`, `session env <name>`, `session rm <name>` | Lists sessions, prints one's env to `source`, deletes one. |
| `session restart <name>` | Restarts a session's daemon and loads its modules again, keeping its packages, keys and funded wallet: a fresh start for a module that keeps state while loaded, safer than `logosctl module reload`. Calls in flight fail, and modules an app loaded itself need loading again. |
| `accounts new <name> --count <n> [--amount <a>]` | Makes a group of funded accounts, see below. |
| `accounts ls` | Lists account groups. |
| `messaging rm` | Stops the messaging hub and deletes it, see below. |

Every command takes `--json` for scripts and agents, `--config <file>` for a project file elsewhere, and `--home <dir>`. A bad option changes nothing: `up` and `reset` check it, and build what they need, before they stop the running chain.

[`examples/`](examples) has LEZ's hello-world tutorial running on a devnet, two `logosctl` sessions paying each other, two sessions messaging and discovering each other through the hub, two sessions sharing a file through local storage, and a GitHub Actions workflow to copy. For a whole project on it, [logos-agents](https://github.com/drptbl/logos-agents) deploys a released program and runs two sessions and the messaging hub from its [`logos-devnet.toml`](https://github.com/drptbl/logos-agents/blob/main/logos-devnet.toml).

## For AI agents

[`skills/logos-devnet`](skills/logos-devnet) is an agent skill: the loop to run, the choices that matter, and what to do when `logos-devnet` refuses. For Claude Code, copy it into `~/.claude/skills/` for every project, or into a project's `.claude/skills/`:

```bash
git clone --depth 1 https://github.com/drptbl/logos-devnet.git /tmp/logos-devnet
mkdir -p ~/.claude/skills && cp -R /tmp/logos-devnet/skills/logos-devnet ~/.claude/skills/
```

The files are plain Markdown, so other agents can read `SKILL.md` from their own instructions.

## Set up a project

Commit a `logos-devnet.toml` at your project's root, and `logos-devnet up` anywhere below it makes the devnet match:

```toml
[devnet]                 # optional, the same settings as up's flags
block_time = "1s"
max_txs_per_block = 100
dev_mode = true          # fake proofs: private calls take a second
log_filter = "info,sequencer_core=debug"   # the sequencer's RUST_LOG, for `logs`

[programs]               # name = guest binary, relative to this file
counter = "target/riscv32im-risc0-zkvm-elf/docker/counter.bin"
shop = { path = "target/riscv32im-risc0-zkvm-elf/docker/shop.bin", mutable = true }
token = { url = "https://github.com/<owner>/<repo>/releases/download/v1.0.0/token.bin", sha256 = "<its 64 hex digits>" }

[accounts.users]         # a group of funded accounts
count = 100
amount = 5_000_000_000   # lepta each, 10B when left out

[sessions.alice]         # a logosctl session, as session new
modules = ["chat_module"]

[messaging]              # a local messaging hub, see Messaging
```

A program can come from a release instead of a path: `up` downloads its `url` once, with `curl`, into the home, and deploys it only if its SHA-256 is the `sha256` given (as `shasum -a 256` prints it).

`up` deploys missing programs, makes and funds missing accounts and sessions, and changes nothing the second time (0.6 s). A `mutable` program whose binary changed is updated in place, with the same id. An immutable one keeps its old build, and `up` and `deploy` fail saying so, so tests don't run against it: `reset` deploys the new build. A module added to a session's `modules` is installed into it. `up` checks the whole file before it touches the devnet.

Projects share one devnet, `~/.logos-devnet` on port 3040, unless the file sets `home`, e.g. `home = ".logos-devnet"`, for a devnet of the project's own. Give it `[devnet] port = 0` too: it takes a free port on the first `up` and keeps it, so it runs next to other devnets, and `logos-devnet env` shows it. With a fixed `port` in the file, `--port` applies for that run, and the next `up` goes back to the file's port. With `port = 0`, `--port` is remembered. On a shared devnet, a `[devnet]` section that disagrees with the running chain is refused rather than restarting the chain under the other projects: `down` and `up` apply it, or `reset` for `lez` and `dev_mode`, which only a new chain takes. A `reset` there funds every project's account groups again, and each project's next `up` deploys its programs on the new chain.

## Many accounts

An account group is any number of funded accounts for tests with many users: a hundred parties calling one program, say. Declare it in the project file or make one by hand:

```bash
logos-devnet accounts new users --count 100 --amount 5000000000
```

Group names follow the rules for program names, and can't end in `-wallet`. A group's keys live in one LEZ wallet, `~/.logos-devnet/accounts/users/wallet`, which `lez_core` and the LEZ CLI wallet can both use: `LOGOS_DEVNET_ACCOUNTS_USERS_WALLET` in the env file points at it, for `LEE_WALLET_HOME_DIR`. A new chain, from `reset` or a first `up`, makes it forget the old one, as it does for sessions: stop an app that has it open first, or it writes the old chain back. Its `accounts.json` lists every account, and `LOGOS_DEVNET_ACCOUNTS_USERS` points at it:

```json
{
  "amount": 5000000000,
  "accounts": [
    { "index": 0, "id": "CfcghzvosNM114Akg71vuQyDApHPYpPVC9p4uPFQygye", "hex": "ad54f9c3…" }
  ]
}
```

A group that exists when a chain starts is funded at genesis, so a hundred accounts, or a thousand, are ready at once. One added to a running chain is funded one transfer at a time, about 2.4 s each (100 accounts took 237 s), so declare groups before the first `up`, or `reset`. A group never shrinks: a lower `count` keeps the accounts it has. A new `amount` applies to new accounts, and to every account after `reset`. Making the keys takes longer as a group grows, since each one is written to the group's wallet: 500 took 3 s, and growing a group from 2,000 to 6,000 took about 80 s. It happens once, as keys outlive `reset`. Each `up` checks every account is funded, which took about 1 s with 6,000. Accounts that send need more than they move: every call holds about 1.6M lepta of fees up front, so give a sending group at least a few million each (the default is 10B).

How many transactions fit is set by `max_txs_per_block`. Upstream allows 20 per block, so at 1 s blocks about 20 per second. Measured with every account of a group sending one transfer at the same moment:

| `max_txs_per_block` | Burst | Landed |
| --- | --- | --- |
| 20 | 100 transfers | 5 blocks of 20, 5.5 s |
| 100 | 100 transfers | 1 block, 1.6 s |
| 200 | 200 transfers | 197 in 1 block. 3 were refused by the RPC, which takes 100 connections at a time |

Up to 1000 is accepted. Above 100 clients at the same instant, the RPC answers 429 to the rest, so retry or spread them.

## Point your project at it

The env file is the contract. A project reads `LOGOS_DEVNET_SEQUENCER` for the RPC URL, and `LOGOS_PROGRAM_<NAME>` (base58) or `LOGOS_PROGRAM_<NAME>_HEX` (what `lez_core` takes as `program_id_hex`) for its programs, falling back to the testnet values when they're not set. A test that drives several sessions at once reads each one's `LOGOS_DEVNET_SESSION_<NAME>` (its `LOGOSCTL_CONFIG_DIR`), `_WALLET`, `_ACCOUNT` and `_ACCOUNT_HEX` from the env file too, so `logos-devnet exec -- pytest` sees them all.

Program ids stay the same. `deploy my-program` always deploys from the dev wallet's `program-my-program` header key, and a program id is that key's account id, so `reset` and redeploy give you the same `LOGOS_PROGRAM_MY_PROGRAM`. A second `deploy` of the same name on the same chain does nothing, unless the program is mutable and the binary changed: then it's updated in place.

Programs are immutable unless you ask, because undisclosed private calls only run against a program that was immutable from the start. A mutable one is for iterating: `up` after a rebuild takes about 10 s, against a `reset`. Each update writes new segment accounts, and the old ones stay. A program keeps its mutability on a chain, so a missing `--mutable` can't freeze it by accident: `reset` deploys it the other way.

For a wallet, give it its own directory. `wallet-config` refuses a directory that already holds a wallet, because repointing a testnet wallet at the devnet mixes up its sync state. Then create the wallet the usual way, e.g. `logosctl call lez_core create_new str:<dir>/wallet_config.json str:<dir>/storage.json str:`, and fund its public account. After a `reset`, such a wallet still holds the old chain's sync position and private notes, and skips the new chain's blocks up to the old height: make a new one.

## Sessions

A session is a throwaway `logosctl` session wired to the devnet, for an app or an agent that talks to Logos through `logosctl`. You need `logosctl` on your `PATH`.

```bash
logos-devnet session new alice                          # lez_core and a funded wallet
logos-devnet session new bob --module chat_module       # plus any module from the catalog
logos-devnet session new carol --amount 5000000         # fund it with less than 10B lepta
source <(logos-devnet session env alice)                # LOGOSCTL_CONFIG_DIR and the wallet
```

Each session runs its own daemon in `~/.logos-devnet/sessions/<name>`, with `lez_core` and any `--module` installed from the official catalog. Its wallet points at the devnet, is open in that daemon, and holds a public account the faucet funded. `session env` prints the devnet's env plus:

| Variable | |
| --- | --- |
| `LOGOSCTL_CONFIG_DIR` | the session, so `logosctl` and anything built on it talk to its daemon |
| `LOGOS_DEVNET_WALLET_CONFIG` | the wallet's `wallet_config.json` |
| `LOGOS_DEVNET_WALLET_STORAGE` | the wallet's `storage.json` |
| `LOGOS_DEVNET_ACCOUNT` | its funded account, `Public/<id>` |
| `LOGOS_DEVNET_ACCOUNT_HEX` | the same account as 64 hex digits, as `lez_core` takes it |

Sessions follow the chain. `down` stops their daemons. `up` starts them again, loads their modules and opens their wallets, also after a `--port` change. `reset` funds each account again with what it was created with, and makes each wallet forget the old chain: its sync position and the state of its private accounts. Its keys and accounts stay. `session rm` deletes a session and its wallet.

The catalog's `lez_core` 0.5.1 is built on LEZ `v0.3.1`, the default `--lez` tag, so sessions match it. They install that version even once the catalog has a newer one, which may be built for LEZ v0.4.

## Messaging

An empty `[messaging]` table in the project file gives the devnet a local messaging hub, so your app's messages and service lookups stay on your machine instead of going through the public fleet:

```toml
[messaging]
```

`up` then runs a `logosctl` daemon of the devnet's own with two nodes, both on `127.0.0.1` only. A `delivery_module` node relays and stores messages on a private cluster, and a `libp2p_module` node is the discovery bootstrap. You need `logosctl` on your `PATH`. The env file gains:

| Variable | |
| --- | --- |
| `LOGOS_DEVNET_DELIVERY_CONFIG` | the `delivery_module.createNode` config for your app's node: Edge mode, no preset, the hub as its only entry node |
| `LOGOS_DEVNET_DELIVERY_ENTRY_NODE` | the hub's delivery node, `/ip4/127.0.0.1/tcp/<port>/p2p/<id>` |
| `LOGOS_DEVNET_DISCOVERY_BOOTSTRAP` | the hub's discovery node, `/ip4/127.0.0.1/tcp/<port>/p2p/<id>` |
| `LOGOS_DEVNET_DISCOVERY_CONFIG` | the `libp2p_module.createNode` config for your app's node: the hub as its only bootstrap node, service discovery on |

Sessions join the hub on their own. When a session's daemon has `delivery_module` or `libp2p_module` loaded, `chat_module` included since it loads `delivery_module`, `up` gives it a node on the hub and starts it, unless an app already made one. That matters because the first `createNode` in a daemon decides its network, and a module that creates its own node uses the public one. `up` does it again after a daemon restart, which drops the nodes. Apps then use the node that's there, and `chat_module` adopts it.

Outside a session, create your app's nodes from `LOGOS_DEVNET_DELIVERY_CONFIG` and `LOGOS_DEVNET_DISCOVERY_CONFIG` before anything else in that daemon does. Two apps whose nodes use the hub exchange messages through it. With only the hub as a peer, their nodes report `PartiallyConnected`.

The hub keeps its ports and peer ids, so the variables stay the same across `down`, `up` and `reset`. `reset` keeps its message store too. `up` brings the hub's nodes back when its daemon restarted, and `down` stops it. `status` shows whether each node runs. Once a devnet has a hub, `up` runs it for every project sharing that devnet: `logos-devnet messaging rm` stops and deletes it, and the next `up` of a project that asks for one starts a new hub, with new ports and peer ids.

Chat isn't fully local yet. `chat_module` 0.3.0 runs over the hub's delivery node once your app created it, but it still looks up keys at a hard-coded public registry.

## Storage

Every session runs a Logos Storage node, `storage_module`, which its daemon starts on its own. Each one keeps its data in its session, on `127.0.0.1`, and joins no network: an upload in a session stays in that session.

An empty `[storage]` table connects them through a storage node in the hub, so a file one session uploads, another downloads by its CID:

```toml
[storage]
```

Sessions need nothing passed in: logos-devnet configures their storage before it starts their daemons. `status` shows whether the hub's storage node runs. Once a project asked for it, the hub keeps its storage node for every project on that devnet, until `logos-devnet messaging rm`.

`storage_module` keeps its data next to the config in `$HOME/.logos_storage`, one store per user, which a second daemon can't open. So each session's daemon, and the hub's, runs with a `HOME` of its own, `home/` in its directory, and logos-devnet never changes your own `~/.logos_storage`. A daemon started before logos-devnet did this is restarted once by `up`, and gets a store of its own. `session env` doesn't set `HOME`: if you start a session's daemon yourself, set `HOME` to its `home/`.

## What runs

LEZ's own `sequencer_service` runs, built with two features. `standalone` replaces Bedrock, the L1, with an in-process mock that accepts every block. `prove` runs programs in-process, so you don't need `r0vm`. The default is `v0.3.1`. `--lez` picks another `v0.3` tag, and a chain only runs one tag: `reset --lez <tag>` to switch. LEZ `v0.4` changes the sequencer's config and commands, and isn't supported yet.

| | devnet | testnet |
| --- | --- | --- |
| Block time | 1 s (`--block-time`) | 60 s, often several minutes |
| Funding | faucet, funded at genesis | testnet faucet |
| Base fee | 8 | 8 |
| Block timestamp | your clock | the sequencer's clock |
| L1 finality, indexer | none | yes |

Fees are the testnet's. Every call holds `gas_limit × base fee` up front, plus a little for its size, so about 1.6M lepta at a `gas_limit` of 200,000. The faucet makes that irrelevant.

Private calls still prove on your machine, so they take as long as on the testnet: a shielded transfer from a session took 118 s on an Apple M-series laptop. Dev mode skips that.

## Dev mode

`up --dev-mode`, or `dev_mode = true` under `[devnet]`, runs the chain with `RISC0_DEV_MODE=1`: provers make fake proofs and the sequencer accepts them. The same transfer lands in 1 s.

The env file sets `RISC0_DEV_MODE` for your app and tests, `1` or `0`, so a value left in your shell can't send fake proofs to a chain that checks them. Sessions' daemons get it too. Real proofs still work in dev mode, so an app that doesn't set it is only slow.

A chain keeps its dev mode: one holding fake proofs crashes when it restarts without it. `reset --dev-mode` and `reset --dev-mode false` start a new chain with or without it, and `status` shows which runs. Don't use dev mode for anything that should prove a private call is valid.

## Where things live

Everything is in `~/.logos-devnet` (or `LOGOS_DEVNET_HOME`):

| Path | |
| --- | --- |
| `settings.json` | LEZ tag, runtime, port, block time, transactions per block, dev mode, log filter, faucet |
| `env` | the env file |
| `chain/` | the sequencer's config, database and keys, and whether the chain runs in dev mode. `reset` deletes it. |
| `wallet/` | the dev wallet: the faucet and each program's header key. Kept across resets. |
| `compose.yaml` | the Docker runtime's compose file |
| `native/` | the native runtime's LEZ checkout, build, log and pid file |
| `sessions/` | one directory per session: its `logosctl` home, its daemon's `HOME` with its storage, wallet and `session.json` |
| `accounts/` | one directory per account group: its wallet and `accounts.json` |
| `programs.json` | the binary hash each program was deployed from |
| `downloads/` | program binaries the project file names by URL, by SHA-256, and in `packages/` the module packages every daemon installs from, downloaded once and checked against the catalog's SHA-256 |
| `messaging/` | the hub, for `[messaging]` and `[storage]`: its `logosctl` home, its daemon's `HOME`, message store, ports and node keys in `hub.json` |
| `lock` | held while a command changes the devnet, so two projects sharing the home take turns |

The RPC listens on `127.0.0.1` only. It has no authentication, so don't publish the port.

## JSON

With `--json`, a command prints one JSON document and nothing else on stdout. Notes go to stderr. The fields:

| Command | Fields |
| --- | --- |
| `up`, `reset`, `status` | `lez`, `runtime`, `sequencer` (`url`, `running`, `runtime_answered`, `block`, `max_txs_per_block`, `block_time`, `dev_mode`), `faucet` (`account`, `balance`), `programs`, `account_groups`, `sessions`, `messaging` (`running`, `delivery_node`, `discovery_node`, `delivery`, `discovery`, or `null` without a hub), `storage` (`running`, or `null` until a project asks for `[storage]`), `env_file` |
| `deploy` | `name`, `id`, `hex`, `var`, `deployed_now`, `updated` |
| `fund` | `account`, `balance` |
| `tx` | `hash`, `block` (or `null` while no block holds it) |
| `account` | `account`, `hex`, `balance`, `nonce`, `in_use`, `data` (each program's data, as a byte count, by program id) |
| `accounts new`, `accounts ls` | `name`, `accounts`, `amount`, `file`, `wallet`, `var` (a list for `ls`) |
| `session new` | `name`, `modules`, `vars` |
| `session ls` | a list of `name`, `running`, `account`, `balance`, `error` |
| `env`, `session env` | an object of variable to value |
| `wallet-config` | `path` |
| `down`, `session rm`, `messaging rm` | `message` |
| `doctor` | `ok`, `checks` (`name`, `ok`, `detail`, `fix`) |

## In CI

The repo is a GitHub Action. On a Linux runner it installs `logos-devnet`, starts the devnet from your project file and exports the env into the job:

```yaml
- uses: drptbl/logos-devnet@v0.7.0
  with:
    args: --max-txs-per-block 100     # optional, more options for up
- run: cargo test                     # LOGOS_DEVNET_SEQUENCER, LOGOS_PROGRAM_*, LOGOS_DEVNET_ACCOUNTS_*, RISC0_DEV_MODE are set
- if: failure()
  run: logos-devnet logs              # the sequencer's log, when a test fails
```

Inputs: `version` (default: the release tag you use the action at, or, pinned to a commit, the release that commit's `Cargo.toml` names), `config`, `working-directory`, `args` (space-separated, without shell quoting. Set the home with `LOGOS_DEVNET_HOME`, not `--home`), `start: "false"` to only install, which also works on macOS runners, and `logosctl-version` (default `0.3.2`). When the project file has sessions, `[messaging]` or `[storage]` and no `logosctl` is on `PATH`, the action installs that release of the Logos Core CLI, checked against its SHA-256. Output: `sequencer`. When `up` itself fails, the action prints the sequencer's log, each session's and the hub's. Starting the devnet needs a Linux runner, for Docker.

## The image

`ghcr.io/drptbl/logos-devnet-sequencer:<lez tag>` holds the sequencer and the LEZ `wallet` for `linux/amd64` and `linux/arm64`. `up` pulls it, and builds it locally from [`docker/Dockerfile`](docker/Dockerfile) when no image is published for your tag. To build it yourself:

```bash
docker build --build-arg LEZ_TAG=v0.3.1 -t ghcr.io/drptbl/logos-devnet-sequencer:v0.3.1 - < docker/Dockerfile
```

## License

MIT or Apache-2.0, at your option.
