//! logos-devnet: a local LEZ chain for Logos development.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod account;
mod chain_config;
mod devnet;
mod envfile;
mod home;
mod rpc;
mod runtime;

use devnet::{DEFAULT_FUND, Devnet, UpOptions};
use home::Home;

/// A local LEZ chain for Logos development: a standalone sequencer with fast blocks, a funded
/// faucet, and stable program ids. Messaging stays on the Logos fleets.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The devnet's state directory.
    #[arg(long, env = "LOGOS_DEVNET_HOME", global = true)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the devnet, or resume it. Options except --rebuild are remembered.
    Up(UpOptions),
    /// Stop the devnet. The chain is kept.
    Down,
    /// Delete the chain and start a new one. The faucet and program ids stay the same.
    Reset(UpOptions),
    /// Show what runs, the chain head, the faucet and deployed programs.
    Status,
    /// Send lepta from the faucet to a public account.
    Fund {
        /// `Public/<id>` or `<id>`.
        account: String,
        /// Lepta to send.
        #[arg(default_value_t = DEFAULT_FUND)]
        amount: u128,
    },
    /// Deploy a program binary, and add `LOGOS_PROGRAM_<NAME>` to the env file.
    Deploy {
        /// A name for the program, e.g. `my-program`. The same name keeps the same program id.
        name: String,
        /// The program's RISC Zero guest binary.
        elf: PathBuf,
    },
    /// Write a `wallet_config.json` pointing at the devnet into a new wallet directory.
    WalletConfig { dir: PathBuf },
    /// Print the env file: `source <(logos-devnet env)`.
    Env,
    /// Print the sequencer's log.
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let devnet = Devnet::open(Home::locate(cli.home)?)?;
    match cli.command {
        Command::Up(opts) => devnet.up(&opts),
        Command::Down => devnet.down(),
        Command::Reset(opts) => devnet.reset(&opts),
        Command::Status => devnet.status(),
        Command::Fund { account, amount } => devnet.fund(&account, amount),
        Command::Deploy { name, elf } => devnet.deploy(&name, &elf),
        Command::WalletConfig { dir } => {
            let path = devnet.write_project_wallet_config(&dir)?;
            println!("{}", path.display());
            Ok(())
        }
        Command::Env => devnet.print_env(),
        Command::Logs { follow } => devnet.logs(follow),
    }
}
