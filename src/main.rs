//! logos-devnet: a local LEZ chain for Logos development.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod account;
mod binary;
mod chain_config;
mod devnet;
mod doctor;
mod envfile;
mod fetch;
mod groups;
mod home;
mod messaging;
mod project;
mod report;
mod rpc;
mod runtime;
mod session;
mod storage;
mod wallet_storage;

use devnet::{DEFAULT_FUND, Devnet, UpOptions};
use home::Home;
use project::Project;
use report::emit;

/// A local LEZ chain for Logos development: a standalone sequencer with fast blocks, a funded
/// faucet, stable program ids, funded account groups and logosctl sessions. A project's
/// `logos-devnet.toml` says what it needs; `up` makes it so.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The devnet's state directory. Wins over the project file's `home`.
    #[arg(long, env = "LOGOS_DEVNET_HOME", global = true)]
    home: Option<PathBuf>,
    /// The project file. By default the nearest logos-devnet.toml here or above.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Print results as JSON, for scripts and agents.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the devnet, or resume it, and make it match logos-devnet.toml. Options except
    /// --rebuild are remembered.
    Up(UpOptions),
    /// Stop the devnet. The chain and sessions are kept.
    Down,
    /// Delete the chain and start a new one. Program ids stay the same and every account group
    /// is funded at genesis.
    Reset(UpOptions),
    /// Show what runs, the chain head, the faucet, programs, account groups, sessions, the
    /// messaging hub and local storage.
    Status,
    /// Check what logos-devnet needs, and say how to fix what's missing.
    Doctor,
    /// Send lepta from the faucet to a public account.
    Fund {
        /// `Public/<id>` or `<id>`.
        account: String,
        /// Lepta to send.
        #[arg(default_value_t = u128::from(DEFAULT_FUND))]
        amount: u128,
    },
    /// Deploy a program binary, and add `LOGOS_PROGRAM_<NAME>` to the env file.
    Deploy {
        /// A name for the program, e.g. `my-program`. The same name keeps the same program id.
        name: String,
        /// The program's RISC Zero guest binary (`.bin`).
        elf: PathBuf,
        /// Deploy it mutable, so a new build updates it in place. Undisclosed private calls
        /// need an immutable program.
        #[arg(long)]
        mutable: bool,
    },
    /// Groups of funded accounts, for tests with many users.
    #[command(subcommand)]
    Accounts(AccountsCommand),
    /// Write a `wallet_config.json` pointing at the devnet into a new wallet directory.
    WalletConfig { dir: PathBuf },
    /// Print the env file: `source <(logos-devnet env)`.
    Env,
    /// Run a command with the devnet's env, and a session's with --session:
    /// `logos-devnet exec -- cargo test`.
    Exec {
        /// Add this session's variables, e.g. LOGOSCTL_CONFIG_DIR.
        #[arg(long)]
        session: Option<String>,
        /// The command and its arguments, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Show where a transaction landed, by its 64-hex hash.
    Tx {
        hash: String,
        /// Wait until a block holds it, and fail after --timeout.
        #[arg(long)]
        wait: bool,
        /// Seconds --wait waits.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Show an account: balance, nonce and data, its id in base58 and hex. Takes
    /// `Public/<id>`, base58, hex or a deployed program's name.
    Account { account: String },
    /// Print the sequencer's log, or a session's or the hub's.
    Logs {
        #[arg(short, long)]
        follow: bool,
        /// A session's logosctl daemon log, with its modules' output.
        #[arg(long, conflicts_with = "hub")]
        session: Option<String>,
        /// The hub's logosctl daemon log, for `[messaging]` and `[storage]`.
        #[arg(long)]
        hub: bool,
    },
    /// Throwaway logosctl sessions with a funded devnet wallet, for apps and agents.
    #[command(subcommand)]
    Session(SessionCommand),
    /// The local messaging hub that a project's `[messaging]` table asks for.
    #[command(subcommand)]
    Messaging(MessagingCommand),
}

#[derive(Subcommand)]
enum MessagingCommand {
    /// Stop the hub and delete it: its ports, peer ids and message store.
    Rm,
}

#[derive(Subcommand)]
enum AccountsCommand {
    /// Make a group of accounts, or grow one, each funded with the same amount.
    New {
        /// The group's name, e.g. `users`.
        name: String,
        #[arg(long)]
        count: usize,
        /// Lepta per account.
        #[arg(long, default_value_t = DEFAULT_FUND)]
        amount: u64,
    },
    /// List account groups.
    Ls,
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Make a session: its own logosctl daemon with lez_core and a funded devnet wallet.
    New {
        name: String,
        /// Another module to install from the catalog, e.g. `chat_module`. Repeatable.
        #[arg(long = "module")]
        modules: Vec<String>,
        /// Lepta to fund the session's account with.
        #[arg(long, default_value_t = u128::from(DEFAULT_FUND))]
        amount: u128,
    },
    /// List sessions, whether their daemons run, and their accounts.
    Ls,
    /// Print the devnet's env and the session's: `source <(logos-devnet session env <name>)`.
    Env { name: String },
    /// Stop a session's daemon and delete it, wallet and all.
    Rm { name: String },
    /// Restart a session's daemon and load its modules again, wallet and keys kept: a fresh
    /// start for modules that keep state while loaded.
    Restart { name: String },
}

impl Command {
    /// Whether it changes the devnet, and so takes its turn on the home.
    fn changes_devnet(&self) -> bool {
        !matches!(
            self,
            Command::Status
                | Command::Doctor
                | Command::WalletConfig { .. }
                | Command::Env
                | Command::Exec { .. }
                | Command::Tx { .. }
                | Command::Account { .. }
                | Command::Logs { .. }
                | Command::Accounts(AccountsCommand::Ls)
                | Command::Session(SessionCommand::Ls | SessionCommand::Env { .. })
        )
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let json = cli.json;
    let project_file = match cli.config {
        Some(path) => Some(path),
        None => project::find(&std::env::current_dir()?),
    };
    let doctor = matches!(cli.command, Command::Doctor);
    // `doctor` reports a broken project file instead of stopping at it.
    let mut project_error = None;
    let project = match project_file.map(|path| Project::load(&path)).transpose() {
        Ok(project) => project,
        Err(e) if doctor => {
            project_error = Some(e);
            None
        }
        Err(e) => return Err(e),
    };
    let home = cli
        .home
        .or_else(|| project.as_ref().and_then(|p| p.home.clone()));
    let home = Home::locate(home)?;
    let _lock = cli
        .command
        .changes_devnet()
        .then(|| home.lock())
        .transpose()?;
    let devnet = Devnet::open(home, project)?;
    match cli.command {
        Command::Up(opts) => emit(&devnet.up(&opts)?, json),
        Command::Down => emit(&devnet.down()?, json),
        Command::Reset(opts) => emit(&devnet.reset(&opts)?, json),
        Command::Status => emit(&devnet.status(), json),
        Command::Doctor => {
            let report = doctor::run(&devnet, project_error.as_ref());
            emit(&report, json)?;
            if !report.ok {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Fund { account, amount } => emit(&devnet.fund(&account, amount)?, json),
        Command::Deploy { name, elf, mutable } => emit(&devnet.deploy(&name, &elf, mutable)?, json),
        Command::Accounts(command) => match command {
            AccountsCommand::New {
                name,
                count,
                amount,
            } => emit(&devnet.accounts_new(&name, count, amount)?, json),
            AccountsCommand::Ls => emit(&devnet.accounts_list()?, json),
        },
        Command::WalletConfig { dir } => emit(&devnet.write_project_wallet_config(&dir)?, json),
        Command::Env => emit(&devnet.env_vars()?, json),
        Command::Exec { session, command } => {
            // Replaces this process, so the command gets Ctrl-C and its exit status as if run
            // directly. `exec` only returns when it couldn't start the command.
            use std::os::unix::process::CommandExt;
            let mut cmd = devnet.exec_command(session.as_deref(), &command)?;
            Err(anyhow::Error::from(cmd.exec()).context(format!("could not run {}", command[0])))
        }
        Command::Tx {
            hash,
            wait,
            timeout,
        } => emit(
            &devnet.tx(&hash, wait.then(|| std::time::Duration::from_secs(timeout)))?,
            json,
        ),
        Command::Account { account } => emit(&devnet.account(&account)?, json),
        Command::Logs {
            follow,
            session,
            hub,
        } => devnet.logs(follow, session.as_deref(), hub),
        Command::Session(command) => match command {
            SessionCommand::New {
                name,
                modules,
                amount,
            } => emit(&devnet.session_new(&name, &modules, amount)?, json),
            SessionCommand::Ls => emit(&devnet.session_list()?, json),
            SessionCommand::Env { name } => emit(&devnet.session_env(&name)?, json),
            SessionCommand::Rm { name } => emit(&devnet.session_rm(&name)?, json),
            SessionCommand::Restart { name } => emit(&devnet.session_restart(&name)?, json),
        },
        Command::Messaging(MessagingCommand::Rm) => emit(&devnet.messaging_rm()?, json),
    }
}
