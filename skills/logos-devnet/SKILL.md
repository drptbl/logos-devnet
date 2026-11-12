---
name: logos-devnet
description: logos-devnet runs a local LEZ (Logos Execution Zone) chain. Use when testing, deploying or calling LEZ programs locally, when tests need funded LEZ accounts or logosctl sessions on a chain, when the LEZ testnet is too slow or its faucet is dry, or when setting up LEZ integration tests in CI.
---

# logos-devnet

`logos-devnet` runs LEZ's own sequencer on this machine, with 1 s blocks and a faucet funded at genesis. A project describes what it needs in `logos-devnet.toml`, and `up` makes the devnet match it. The README (https://github.com/drptbl/logos-devnet) has every detail, and `logos-devnet <command> --help` every flag.

## The loop

Pass `--json` to every command: on success stdout holds one JSON document, and notes go to stderr. On failure the exit is non-zero, stdout is empty, and the error is on stderr, starting `Error:`.

1. Put what the project needs in `logos-devnet.toml` at its root, so every `up` reproduces it:
   ```toml
   home = ".logos-devnet"   # a devnet of this project's own
   [devnet]
   port = 3041              # its own port: the shared devnet uses 3040
   dev_mode = true          # only if something makes private calls
   [programs]
   counter = "target/riscv32im-risc0-zkvm-elf/docker/counter.bin"
   [accounts.users]
   count = 20               # funded at genesis, 10B lepta each
   [sessions.alice]         # a logosctl session with a funded wallet
   ```
   Program, group and session names are lower-case letters, digits and `-`. A project-local home is state, not source: add `.logos-devnet/` to `.gitignore`.
2. `logos-devnet doctor --json`, from the project's directory so it checks the file's settings. It checks the setup (Docker, the port, the chain), not program builds: `up` does that. Done when `.ok` is true. Each failed check carries a `detail`, and a `fix` when the detail doesn't already say what to do.
3. `logos-devnet up --json`. Done when it exits 0 and `.sequencer.running` is true. It is idempotent, so run it again after any change to the file or a rebuild. When it exits non-zero after starting, the error starts "the devnet is up, but" and names what failed: fix that and run `up` again.
4. `source <(logos-devnet env)`, or read `logos-devnet env --json`. Code and tests read these, so they work on any devnet:
   - `LOGOS_DEVNET_SEQUENCER`: the RPC URL.
   - `LOGOS_PROGRAM_<NAME>`, `LOGOS_PROGRAM_<NAME>_HEX`: a program's id, base58 and hex, with the name upper-cased and `-` as `_` (`hello-world` is `LOGOS_PROGRAM_HELLO_WORLD`). It stays the same across `reset`.
   - `LOGOS_DEVNET_ACCOUNTS_<NAME>`: the group's `accounts.json`, `{"amount": ..., "accounts": [{"index", "id", "hex"}]}` with `id` in base58.
   - `LOGOS_DEVNET_ACCOUNTS_<NAME>_WALLET`: the group's LEZ wallet, for `LEE_WALLET_HOME_DIR`.
   - `RISC0_DEV_MODE`: `1` or `0`, matching the chain.
   - With `[messaging]`: `LOGOS_DEVNET_DELIVERY_CONFIG`, the `delivery_module.createNode` config of a node that uses the local hub, `LOGOS_DEVNET_DELIVERY_ENTRY_NODE`, and `LOGOS_DEVNET_DISCOVERY_BOOTSTRAP`, a `libp2p_module` bootstrap node.
5. `logos-devnet status --json` shows what runs: chain head, faucet, programs, groups, sessions. `.sequencer.runtime_answered` is false when Docker didn't answer within 10 s: Docker is stuck, so restart it rather than running `up`. The RPC answers anything else, e.g. an account's balance by its base58 id:
   ```bash
   curl -fsS "$LOGOS_DEVNET_SEQUENCER" -H 'content-type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"getAccountBalance","params":["<base58 id>"]}'
   ```

## Choices

- **Home.** Without `home`, every project on the machine shares `~/.logos-devnet` on port 3040, and `up` refuses settings that disagree with what other projects run there. Tests, worktrees and parallel agents get `home = ".logos-devnet"` and a port of their own.
- **Dev mode.** `dev_mode = true` makes private calls prove in about 1 s instead of about 2 min, with fake proofs. A chain keeps its dev mode for life: switching it takes `reset`. Use a chain without dev mode to check that real proofs verify.
- **Programs.** A program can be a path, or a release: `{ url = "...", sha256 = "..." }`, downloaded once and deployed only if its SHA-256 matches. Programs deploy immutable, which undisclosed private calls require. While iterating on one, give it `{ path = "...", mutable = true }`: a rebuild then updates it in place with the same id on the next `up`. Its mutability is fixed per chain, so changing it takes `reset`. `.logos-devnet/programs.json` records each deployed program's binary SHA-256 and mutability: compare it with `shasum -a 256 <bin>` to show which build runs.
- **Funding.** Groups that exist when a chain starts are funded at genesis, at once. A group added to a running chain is funded one transfer at a time, about 2.4 s per account. Every call holds about 1.6M lepta of fees up front, so an account that sends needs more than that. `logos-devnet fund Public/<id> [lepta]` tops one account up.
- **Messaging.** An empty `[messaging]` table runs a local hub: a delivery node and a discovery bootstrap on `127.0.0.1`, so messages and service lookups skip the public fleet. Create the app's delivery node from `LOGOS_DEVNET_DELIVERY_CONFIG` first in its daemon: the first `createNode` picks the network. `chat_module` still reaches a public key registry. `status --json` shows whether each hub node runs, in `.messaging.delivery_node` and `.messaging.discovery_node`, and `messaging rm` deletes the hub: a project that asks for one gets a new hub, with new addresses, on its next `up`.
- **Sessions** need `logosctl` on `PATH`. They suit apps that talk to Logos through `logosctl` and `lez_core`. Before calling `lez_core` yourself, read [lez_core.md](lez_core.md): it takes ids and amounts in forms its errors don't explain.

## Reset

`reset` deletes the chain: every balance, transaction and program state. It then starts a new one with every group funded at genesis and the project's programs deployed again at the same ids. It acts on the devnet the current directory's `logos-devnet.toml` names (or `--home`), so run it from the project's directory: on the shared devnet it resets every project's chain. Session and group wallets forget the old chain; stop any app holding a group wallet open first. Reach for `reset` when an error names it, or to change the LEZ tag or dev mode. For a fresh state between test runs, give each run its own `home` instead. `reset` is how to start over: it keeps the home's dev wallet, whose program header keys keep program ids stable.

## When it refuses

Each refusal changes nothing. `up` prefixes a program's refusal with `program <name>:`. Match what the message says:

| Message says | Next step |
| --- | --- |
| `<bin> changed since it was deployed, immutable` | The old build still runs. `reset` deploys the new one, or mark it `mutable = true` and `reset` once. |
| `<name> is deployed mutable, asked for immutable` (or the reverse) | Put `mutable` back as deployed, or `reset`. |
| `this chain runs in dev mode` / `this chain checks proofs` | Match the chain's dev mode, or `reset --dev-mode` / `reset --dev-mode false`. |
| `this chain runs LEZ <tag>` | `reset --lez <tag>`. |
| `logos-devnet runs LEZ v0.3 tags` | Use a v0.3 tag, e.g. `--lez v0.3.1`. LEZ v0.4 isn't supported yet. |
| `the shared devnet runs with ..., but <file> asks for ...` | Other projects use that devnet. Give this project its own `home` and port. |
| `port <n> on 127.0.0.1 is taken` | `up --port <free port>`, which is remembered, or the file's `[devnet] port`. |
| `sessions and the messaging hub need logosctl` | Install the Logos Core CLI, or remove the sessions and `[messaging]`. |
| `port <n> on 127.0.0.1, which the messaging hub keeps, is taken` | Stop what holds it, or `messaging rm` and `up` for new ports and peer ids. |
| `not a RISC Zero program binary` | Deploy the guest's `.bin` from `cargo risczero build`, not its ELF. |
| `no Docker daemon answers` | Start Docker (OrbStack, Docker Desktop or Colima). |
| `no block within` / `the sequencer stopped while starting` | Read `logos-devnet logs`. |

## Debugging

- `logos-devnet logs` prints the last 200 lines of the sequencer's log, and `logs -f` follows it. `up --log-filter debug`, or `[devnet] log_filter`, raises its level for guest failures.
- The RPC has no authentication and listens on `127.0.0.1` only. Keep it there.
