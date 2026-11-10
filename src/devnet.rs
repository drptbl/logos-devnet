//! The commands: start a chain, fund accounts, deploy programs, and write what projects need.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::account::{PublicId, first_public_id};
use crate::chain_config::{self, ChainOptions};
use crate::envfile::{self, Env};
use crate::groups::{self, Group, GroupInfo};
use crate::home::{Home, RuntimeKind, Settings, write_json};
use crate::project::Project;
use crate::report::{
    self, Balance, Done, EnvVars, GroupSummary, Groups, Program, Report, SessionMade, SessionRow,
    Sessions, Status, WalletConfig,
};
use crate::rpc::Rpc;
use crate::runtime::{BATCH_CHUNK, Docker, Native, Runtime, WalletArg};
use crate::session::{self, Session, SessionInfo};

/// The faucet's genesis balance in lepta: enough that a devnet never runs dry.
const FAUCET_BALANCE: u64 = 1_000_000_000_000_000_000;
/// The dev wallet's label for the faucet account.
const FAUCET_LABEL: &str = "faucet";
/// What `fund`, sessions and account groups send when no amount is given.
pub const DEFAULT_FUND: u64 = 10_000_000_000;
/// LEZ's program loader stores a program in segments of at most 96 KiB
/// (`program_loader_core::MAX_SEGMENT_DATA_LEN` in v0.3.1).
const SEGMENT_LEN: u64 = 96 * 1024;
/// How long `up` waits for the first block.
const READY_TIMEOUT: Duration = Duration::from_secs(180);
/// How often wallets poll the devnet for a transaction: a block or two.
const WALLET_POLL_TIMEOUT: &str = "2s";

/// What `up` and `reset` may change. Unset options keep the stored setting.
#[derive(Debug, Default, Clone, clap::Args)]
pub struct UpOptions {
    /// Run in Docker (the default).
    #[arg(long, conflicts_with = "native")]
    pub docker: bool,
    /// Build LEZ and run it natively, without Docker.
    #[arg(long)]
    pub native: bool,
    /// The LEZ release tag to run.
    #[arg(long)]
    pub lez: Option<String>,
    /// The RPC port on 127.0.0.1.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: Option<u16>,
    /// Block interval, e.g. `1s` or `500ms`.
    #[arg(long)]
    pub block_time: Option<String>,
    /// The most transactions a block takes (upstream's default is 20).
    #[arg(long)]
    pub max_txs_per_block: Option<u32>,
    /// Build the image or binaries even if they exist. Not remembered.
    #[arg(long)]
    pub rebuild: bool,
}

impl UpOptions {
    pub(crate) fn apply(&self, mut settings: Settings) -> Settings {
        if self.docker {
            settings.runtime = RuntimeKind::Docker;
        }
        if self.native {
            settings.runtime = RuntimeKind::Native;
        }
        if let Some(lez) = &self.lez {
            settings.lez.clone_from(lez);
        }
        if let Some(port) = self.port {
            settings.port = port;
        }
        if let Some(block_time) = &self.block_time {
            settings.block_time.clone_from(block_time);
        }
        if let Some(max) = self.max_txs_per_block {
            settings.max_txs_per_block = max;
        }
        settings
    }
}

pub struct Devnet {
    home: Home,
    settings: Settings,
    runtime: Box<dyn Runtime>,
    /// The project's `logos-devnet.toml`, when there is one.
    project: Option<Project>,
}

impl Devnet {
    pub fn open(home: Home, project: Option<Project>) -> Result<Self> {
        let settings = home.load_settings()?;
        Ok(Self::with(home, settings, project))
    }

    fn with(home: Home, settings: Settings, project: Option<Project>) -> Self {
        let runtime: Box<dyn Runtime> = match settings.runtime {
            RuntimeKind::Docker => Box::new(Docker::new(home.clone(), settings.clone())),
            RuntimeKind::Native => Box::new(Native::new(home.clone(), settings.clone())),
        };
        Self {
            home,
            settings,
            runtime,
            project,
        }
    }

    fn rpc(&self) -> Rpc {
        Rpc::new(self.settings.sequencer_url())
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn project(&self) -> Option<&Project> {
        self.project.as_ref()
    }

    pub fn running(&self) -> bool {
        self.runtime.running()
    }

    /// The faucet's balance, when there is a faucet and the chain answers.
    pub fn faucet_balance(&self) -> Option<u128> {
        self.settings
            .faucet
            .as_ref()
            .and_then(|faucet| self.rpc().balance(faucet).ok())
    }

    /// Whether sessions exist here or the project declares some.
    pub fn needs_logosctl(&self) -> bool {
        !self.sessions().is_empty()
            || self
                .project
                .as_ref()
                .is_some_and(|p| !p.sessions.is_empty())
    }

    /// Runs the LEZ wallet on the dev wallet. Its config is rewritten first: the wallet would
    /// otherwise create a default one pointing at the public testnet.
    fn wallet(&self, args: Vec<WalletArg>) -> Result<String> {
        let home = self.home.wallet_dir();
        self.point_wallet_at(&home, &self.runtime.wallet_sequencer_url())?;
        self.runtime.wallet(&home, args)
    }

    /// Writes the wallet config in `home` pointing at `sequencer`. Done before every wallet
    /// run: without one the wallet creates a default pointing at the public testnet.
    fn point_wallet_at(&self, home: &Path, sequencer: &str) -> Result<()> {
        std::fs::create_dir_all(home)?;
        write_json(&home.join("wallet_config.json"), &wallet_config(sequencer))
    }

    fn faucet(&self) -> Result<&PublicId> {
        self.settings
            .faucet
            .as_ref()
            .context("no faucet yet: run `logos-devnet up` first")
    }

    fn require_running(&self) -> Result<()> {
        if !self.runtime.running() {
            bail!("the devnet is not running: run `logos-devnet up`");
        }
        Ok(())
    }

    pub fn up(self, opts: &UpOptions) -> Result<Status> {
        self.launch(opts, false)
    }

    /// Deletes the chain and starts a new one. Keys in the dev wallet stay, so the faucet and
    /// program ids come back the same.
    pub fn reset(self, opts: &UpOptions) -> Result<Status> {
        self.launch(opts, true)
    }

    /// Starts the devnet with `opts` applied, on a new chain when `wipe`. Everything that can
    /// fail runs before the running sequencer is stopped, the settings saved or the chain
    /// deleted, so a bad option leaves the devnet as it was.
    fn launch(self, opts: &UpOptions, wipe: bool) -> Result<Status> {
        let running = self.runtime.running();
        let settings = opts.apply(self.with_project_settings(opts, running && !wipe)?);
        chain_config::block_time_secs(&settings.block_time)?;
        chain_config::check_max_txs_per_block(settings.max_txs_per_block)?;
        let continuing = !wipe && self.home.has_chain();
        if continuing && settings.lez != self.settings.lez {
            bail!(
                "this chain runs LEZ {}. `logos-devnet reset --lez {}` starts a new chain on it",
                self.settings.lez,
                settings.lez
            );
        }
        self.check_project_needs()?;
        // Nothing to restart, unless the settings file is gone and needs writing again.
        let unchanged = settings == self.settings && self.home.settings_file().exists();
        if !wipe && unchanged && !opts.rebuild && running {
            eprintln!("Already running.");
            self.make_project_groups()?;
            self.write_env(&self.read_env().unwrap_or_default())?;
            let applied = self.apply_project();
            let resumed = self.resume_sessions(false);
            return with_status(self.status(), applied.and(resumed));
        }
        // The running sequencer holds its own port, and gives it up when it stops.
        if !(running && settings.port == self.settings.port) {
            port_free(settings.port)?;
        }

        let mut next = Self::with(self.home.clone(), settings, self.project.clone());
        next.runtime.prepare(opts.rebuild)?;
        let faucet = next.dev_faucet()?;
        if continuing && funds_another_faucet(self.settings.faucet.as_ref(), &faucet) {
            bail!(
                "this chain's genesis funds another faucet than the dev wallet holds. \
                 `logos-devnet reset` starts a chain that funds it"
            );
        }
        next.settings.faucet = Some(faucet);
        // Keys first, so a new chain funds every group at genesis.
        next.make_project_groups()?;
        let config = next.chain_config()?;

        self.runtime.stop()?;
        if wipe {
            next.runtime.wipe_chain()?;
            remove_if_present(&self.home.env_file(), |p| std::fs::remove_file(p))?;
        }
        next.home.save_settings(&next.settings)?;
        std::fs::create_dir_all(next.home.chain_dir())?;
        write_json(&next.home.chain_config(), &config)?;
        next.runtime.start()?;
        next.wait_ready()?;

        let existing = std::fs::read_to_string(next.home.env_file()).ok();
        let env = carried_env(
            continuing,
            existing.as_deref(),
            next.settings.sequencer_url(),
        )?;
        next.write_env(&env)?;
        // A new chain starts every session's account at zero.
        let resumed = next.resume_sessions(!continuing);
        let applied = next.apply_project();
        with_status(next.status(), resumed.and(applied))
    }

    /// The stored settings with the project's `[devnet]` section applied. A shared devnet that
    /// runs with other settings is not restarted for one project (`refuse_changes`), unless
    /// `opts` asks for the same change explicitly.
    fn with_project_settings(&self, opts: &UpOptions, refuse_changes: bool) -> Result<Settings> {
        let Some(project) = &self.project else {
            return Ok(self.settings.clone());
        };
        let wanted = project.devnet.applied_to(self.settings.clone());
        // A project with its own home is the only one on its devnet: its file just applies.
        let shared = project.home.is_none();
        if refuse_changes
            && shared
            && opts.apply(wanted.clone()) != opts.apply(self.settings.clone())
        {
            bail!(
                "the shared devnet runs with {}, but {} asks for {}. `logos-devnet down` and `up` \
                 apply it, restarting the chain for everything on it, or give this project its \
                 own devnet with `home = \".logos-devnet\"` in the file",
                project.devnet.describe(&self.settings),
                project.path.display(),
                project.devnet.describe(&wanted)
            );
        }
        Ok(wanted)
    }

    /// What the project needs that `up` can't make by itself, checked before anything changes.
    fn check_project_needs(&self) -> Result<()> {
        let Some(project) = &self.project else {
            return Ok(());
        };
        project.check_binaries()?;
        let new_session = project
            .sessions
            .keys()
            .any(|name| Session::new(&self.home.sessions_dir(), name).is_ok_and(|s| !s.exists()));
        if new_session && !session::logosctl_on_path() {
            bail!(
                "{} declares sessions, which need logosctl, the Logos Core CLI, on your PATH",
                project.path.display()
            );
        }
        Ok(())
    }

    /// Creates the keys of every account group the project declares. Funding comes later: at
    /// genesis on a new chain, by transfer on a running one.
    fn make_project_groups(&self) -> Result<()> {
        let Some(project) = &self.project else {
            return Ok(());
        };
        for (name, spec) in &project.accounts {
            self.make_group(name, spec.count, spec.amount.unwrap_or(DEFAULT_FUND))?;
        }
        Ok(())
    }

    /// Makes the devnet match the project once the chain runs: deploys its programs, funds
    /// every account group's new accounts and makes missing sessions. Every item is tried;
    /// failures are reported together after.
    fn apply_project(&self) -> Result<()> {
        let mut failed: Vec<String> = Vec::new();
        if let Some(project) = &self.project {
            for (name, binary) in &project.programs {
                if let Err(e) = self.deploy_program(name, binary) {
                    eprintln!("program {name}: {e:#}");
                    failed.push(format!("program {name}"));
                }
            }
        }
        if let Err(e) = self.fund_groups(None) {
            eprintln!("accounts: {e:#}");
            failed.push("accounts".into());
        }
        if let Some(project) = &self.project {
            for (name, spec) in &project.sessions {
                if Session::new(&self.home.sessions_dir(), name).is_ok_and(|s| s.exists()) {
                    continue;
                }
                let amount = spec.amount.unwrap_or(DEFAULT_FUND);
                if let Err(e) = self.session_new(name, &spec.modules, u128::from(amount)) {
                    eprintln!("session {name}: {e:#}");
                    failed.push(format!("session {name}"));
                }
            }
        }
        if !failed.is_empty() {
            bail!("the devnet is up, but these failed: {}", failed.join(", "));
        }
        Ok(())
    }

    /// Group `name` with at least `count` accounts, each to be funded with `amount`. Missing
    /// keys are made in one wallet batch. A group never shrinks: its keys stay.
    fn make_group(&self, name: &str, count: usize, amount: u64) -> Result<GroupInfo> {
        let group = Group::new(&self.home.groups_dir(), name)?;
        let mut info = group.info()?;
        let wallet = group.wallet_dir();
        // The group's wallet is for the project to use from this machine: it points at the
        // devnet's local URL. Making keys needs no network.
        self.point_wallet_at(&wallet, &self.settings.sequencer_url())?;
        if !info.accounts.is_empty() && info.amount != amount {
            eprintln!(
                "group {name}: new accounts get {amount} lepta; the ones funded already keep {} \
                 until `logos-devnet reset`",
                info.amount
            );
        }
        if count < info.accounts.len() {
            eprintln!(
                "group {name} keeps its {} accounts: groups never shrink",
                info.accounts.len()
            );
        }
        if info.accounts.len() < count {
            eprintln!(
                "Making {} accounts in group {name}...",
                count - info.accounts.len()
            );
        }
        info.amount = amount;
        // Keys are made here and imported: the wallet's own `account new` slows down as a
        // wallet grows (300 keys took minutes), while importing 400 takes under 2 s. In
        // chunks, saving after each: a batch is one shell argument, and a long one fails.
        while info.accounts.len() < count {
            let missing = (count - info.accounts.len()).min(BATCH_CHUNK);
            let runs = (0..missing)
                .map(|_| {
                    Ok(vec![
                        "account".into(),
                        "import".into(),
                        "public".into(),
                        "--private-key".into(),
                        random_key()?,
                    ])
                })
                .collect::<Result<Vec<Vec<String>>>>()?;
            let ids = self
                .runtime
                .wallet_batch(&wallet, &runs)?
                .iter()
                .map(|out| first_public_id(out).context("the wallet made no account"))
                .collect::<Result<Vec<_>>>()?;
            info = Group::extended(info, ids);
            group.save_info(&info)?;
        }
        group.save_info(&info)?;
        Ok(info)
    }

    /// Funds every account that was never funded in the account groups (`only` one of them,
    /// when given), one transfer each, and points their wallets at the devnet's current URL.
    /// "Never funded" is read from the chain, so spent accounts aren't topped up.
    fn fund_groups(&self, only: Option<&str>) -> Result<()> {
        let rpc = self.rpc();
        for group in groups::list(&self.home.groups_dir())? {
            if only.is_some_and(|name| name != group.name) {
                continue;
            }
            self.point_wallet_at(&group.wallet_dir(), &self.settings.sequencer_url())?;
            let info = group.info()?;
            let mut unfunded = Vec::new();
            for account in &info.accounts {
                if !rpc.account_in_use(&account.id)? {
                    unfunded.push(&account.id);
                }
            }
            if unfunded.is_empty() {
                continue;
            }
            eprintln!(
                "Funding {} accounts in group {} one transfer at a time \
                 (`logos-devnet reset` funds them all at genesis)...",
                unfunded.len(),
                group.name
            );
            for id in unfunded {
                self.send(id, u128::from(info.amount))?;
            }
        }
        Ok(())
    }

    /// Every account group's accounts with their genesis balance.
    fn genesis_accounts(&self) -> Result<Vec<(PublicId, u64)>> {
        let mut accounts = Vec::new();
        for group in groups::list(&self.home.groups_dir())? {
            let info = group.info()?;
            accounts.extend(info.accounts.into_iter().map(|a| (a.id, info.amount)));
        }
        Ok(accounts)
    }

    /// Brings each session up to date with the chain: its wallet config names the current
    /// port, its daemon runs with its modules loaded and its wallet open, and on a new chain
    /// (`refund`) its account is funded again. Every session is tried; failures are reported
    /// together after, with the chain already up.
    fn resume_sessions(&self, refund: bool) -> Result<()> {
        let mut failed = Vec::new();
        for session in self.sessions() {
            let resumed = session.info().and_then(|info| {
                // Funding only needs the dev wallet: a daemon that won't start doesn't cost the
                // account its funds.
                let funded = if refund {
                    self.send(&info.account, info.funded).map(drop)
                } else {
                    Ok(())
                };
                self.write_session_wallet_config(&session)?;
                let ctl = session.logosctl();
                ctl.start()?;
                ctl.load(&info.modules)?;
                ctl.ensure_wallet(
                    &session.wallet_config(),
                    &session.wallet_storage(),
                    &self.settings.sequencer_url(),
                )?;
                funded
            });
            if let Err(e) = resumed {
                eprintln!("session {}: {e:#}", session.name);
                failed.push(session.name);
            }
        }
        if !failed.is_empty() {
            bail!(
                "the devnet is up, but sessions {} failed. If one keeps failing, \
                 `logos-devnet session rm <name>` and make it again",
                failed.join(", ")
            );
        }
        Ok(())
    }

    /// The sessions, or none (with a note) when their directory can't be read.
    fn sessions(&self) -> Vec<Session> {
        session::list(&self.home.sessions_dir()).unwrap_or_else(|e| {
            eprintln!("sessions: {e:#}");
            Vec::new()
        })
    }

    /// Points the session's wallet at the devnet's current RPC URL.
    fn write_session_wallet_config(&self, session: &Session) -> Result<()> {
        std::fs::create_dir_all(session.wallet_dir())?;
        write_json(
            &session.wallet_config(),
            &wallet_config(&self.settings.sequencer_url()),
        )
    }

    /// Sends `amount` lepta from the faucet to `to` and returns its new balance.
    fn send(&self, to: &PublicId, amount: u128) -> Result<u128> {
        let faucet = self.faucet()?;
        self.wallet(vec![
            "auth-transfer".into(),
            "send".into(),
            "--from".into(),
            faucet.wallet_form().into(),
            "--to".into(),
            to.wallet_form().into(),
            "--amount".into(),
            amount.to_string().into(),
        ])?;
        self.rpc().balance(to)
    }

    /// The dev wallet's faucet account, made on first use. Read from the wallet rather than
    /// the settings, so a lost settings file finds the same faucet.
    fn dev_faucet(&self) -> Result<PublicId> {
        match self.labelled_account(FAUCET_LABEL)? {
            Some(faucet) => Ok(faucet),
            None => self.new_account(Some(FAUCET_LABEL)),
        }
    }

    fn chain_config(&self) -> Result<Value> {
        let genesis_accounts = self.genesis_accounts()?;
        chain_config::patch(
            self.runtime.upstream_config()?,
            &ChainOptions {
                home: &self.runtime.chain_home(),
                block_time: &self.settings.block_time,
                faucet: self.faucet()?,
                faucet_balance: FAUCET_BALANCE,
                max_txs_per_block: self.settings.max_txs_per_block,
                genesis_accounts: &genesis_accounts,
            },
        )
    }

    fn wait_ready(&self) -> Result<()> {
        let rpc = self.rpc();
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if rpc.last_block_id().is_ok_and(|head| head >= 1) {
                return Ok(());
            }
            if !self.runtime.running() {
                bail!("the sequencer stopped while starting. See `logos-devnet logs`");
            }
            if Instant::now() > deadline {
                bail!(
                    "no block within {}s. See `logos-devnet logs`",
                    READY_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Writes the env file: `env`'s sequencer and programs, and every account group.
    fn write_env(&self, env: &Env) -> Result<()> {
        let mut env = Env {
            sequencer: self.settings.sequencer_url(),
            programs: env.programs.clone(),
            groups: BTreeMap::new(),
        };
        for group in groups::list(&self.home.groups_dir())? {
            env.groups.insert(group.name.clone(), group.manifest());
        }
        std::fs::write(self.home.env_file(), env.render()).context("writing the env file")
    }

    fn read_env(&self) -> Result<Env> {
        let text = std::fs::read_to_string(self.home.env_file())
            .context("no env file: run `logos-devnet up` first")?;
        Env::parse(&text)
    }

    /// Stops the sessions' daemons, then the sequencer. The chain and sessions are kept.
    pub fn down(&self) -> Result<Done> {
        for session in self.sessions() {
            if let Err(e) = session.logosctl().stop() {
                eprintln!("session {}: {e:#}", session.name);
            }
        }
        self.runtime.stop()?;
        Ok(Done {
            message: "Stopped. The chain and sessions are kept; `logos-devnet up` resumes them."
                .into(),
        })
    }

    pub fn status(&self) -> Status {
        let s = &self.settings;
        let running = self.runtime.running();
        let head = running.then(|| self.rpc().last_block_id().ok()).flatten();
        let env = self.read_env().ok();
        let programs = env
            .as_ref()
            .map(|env| {
                env.programs
                    .iter()
                    .map(|(name, id)| program_report(name, id, false))
                    .collect()
            })
            .unwrap_or_default();
        Status {
            lez: s.lez.clone(),
            runtime: self.runtime.describe(),
            sequencer: report::Sequencer {
                url: s.sequencer_url(),
                running,
                block: head,
                max_txs_per_block: s.max_txs_per_block,
                block_time: s.block_time.clone(),
            },
            faucet: s.faucet.as_ref().map(|faucet| Balance {
                account: faucet.wallet_form(),
                balance: head.and_then(|_| self.rpc().balance(faucet).ok()),
            }),
            programs,
            account_groups: self.group_summaries().unwrap_or_default(),
            sessions: self.sessions().into_iter().map(|s| s.name).collect(),
            env_file: env.map(|_| self.home.env_file()),
        }
    }

    pub fn fund(&self, account: &str, amount: u128) -> Result<Balance> {
        let to = PublicId::parse(account)?;
        // Fails before any work when there is no faucet yet.
        self.faucet()?;
        self.require_running()?;
        let balance = self.send(&to, amount)?;
        Ok(Balance {
            account: to.wallet_form(),
            balance: Some(balance),
        })
    }

    /// Makes account group `name` with `count` accounts (at least: a group never shrinks),
    /// each funded with `amount`, now when the devnet runs, else when it starts.
    pub fn accounts_new(&self, name: &str, count: usize, amount: u64) -> Result<GroupSummary> {
        groups::check_spec(count, amount)?;
        self.runtime.prepare(false)?;
        self.make_group(name, count, amount)?;
        if self.runtime.running() {
            self.fund_groups(Some(name))?;
            self.write_env(&self.read_env().unwrap_or_default())?;
        } else {
            eprintln!(
                "The devnet isn't running. `logos-devnet up` funds the group, by transfer on the \
                 chain it resumes, and `reset` funds it at genesis on a new one."
            );
        }
        self.group_summaries()?
            .into_iter()
            .find(|g| g.name == name)
            .context("the group just made is missing")
    }

    pub fn accounts_list(&self) -> Result<Groups> {
        Ok(Groups(self.group_summaries()?))
    }

    fn group_summaries(&self) -> Result<Vec<GroupSummary>> {
        groups::list(&self.home.groups_dir())?
            .into_iter()
            .map(|group| {
                let info = group.info()?;
                Ok(GroupSummary {
                    var: envfile::group_var(&group.name),
                    accounts: info.accounts.len(),
                    amount: info.amount,
                    file: group.manifest(),
                    wallet: group.wallet_dir(),
                    name: group.name,
                })
            })
            .collect()
    }

    /// Makes session `name`: its own logosctl daemon with `lez_core` (and `extra` modules)
    /// from the catalog, a devnet wallet opened in it, and a public account funded with
    /// `amount`. A failed attempt leaves nothing behind.
    pub fn session_new(&self, name: &str, extra: &[String], amount: u128) -> Result<SessionMade> {
        let session = Session::new(&self.home.sessions_dir(), name)?;
        if session.exists() {
            bail!("session {name} exists. `logos-devnet session rm {name}` first");
        }
        let modules = session::modules_with(extra)?;
        // Fails before any work when there is no faucet yet.
        self.faucet()?;
        self.require_running()?;
        let made = self.make_session(&session, modules, amount);
        if made.is_err() {
            // Deleting the directory under a daemon that still runs would orphan it.
            match session.logosctl().stop() {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(session.dir());
                }
                Err(e) => eprintln!(
                    "session {name}: its daemon didn't stop ({e:#}), so {} stays",
                    session.dir().display()
                ),
            }
        }
        let info = made?;
        Ok(SessionMade {
            name: name.to_owned(),
            vars: session
                .vars(&info)
                .into_iter()
                .map(|(var, value)| (var.to_owned(), value))
                .collect(),
            modules: info.modules,
        })
    }

    fn make_session(
        &self,
        session: &Session,
        modules: Vec<String>,
        amount: u128,
    ) -> Result<SessionInfo> {
        self.write_session_wallet_config(session)?;
        let ctl = session.logosctl();
        eprintln!(
            "Starting session {} and installing {}...",
            session.name,
            modules.join(", ")
        );
        ctl.start()?;
        ctl.install(&modules)?;
        ctl.load(&modules)?;
        ctl.create_wallet(&session.wallet_config(), &session.wallet_storage())?;
        let account = ctl.call("lez_core", "create_account_public", &[])?;
        let account =
            PublicId::from_hex(account.as_str().context("lez_core returned no account")?)?;
        ctl.call("lez_core", "save", &[])?;
        let info = SessionInfo {
            modules,
            account,
            funded: amount,
        };
        session.save_info(&info)?;
        self.send(&info.account, amount)?;
        Ok(info)
    }

    pub fn session_list(&self) -> Result<Sessions> {
        let chain_up = self.runtime.running();
        let rows = session::list(&self.home.sessions_dir())?
            .into_iter()
            .map(|session| {
                let running = session.logosctl().running();
                match session.info() {
                    Ok(info) => SessionRow {
                        balance: chain_up
                            .then(|| self.rpc().balance(&info.account).ok())
                            .flatten(),
                        account: Some(info.account.wallet_form()),
                        name: session.name,
                        running,
                        error: None,
                    },
                    Err(e) => SessionRow {
                        name: session.name,
                        running,
                        account: None,
                        balance: None,
                        error: Some(format!("{e:#}")),
                    },
                }
            })
            .collect();
        Ok(Sessions(rows))
    }

    /// The devnet's env and the session's own variables, to `source`.
    pub fn session_env(&self, name: &str) -> Result<EnvVars> {
        let session = self.existing_session(name)?;
        let info = session.info()?;
        // Without the devnet's env the session's variables still load.
        let mut vars = self.read_env().map(|env| env.vars()).unwrap_or_else(|e| {
            eprintln!("{e:#}");
            Vec::new()
        });
        vars.extend(
            session
                .vars(&info)
                .into_iter()
                .map(|(var, value)| (var.to_owned(), value)),
        );
        Ok(EnvVars::new(vars))
    }

    /// Stops the session's daemon and deletes it, wallet and all.
    pub fn session_rm(&self, name: &str) -> Result<Done> {
        let session = self.existing_session(name)?;
        session.logosctl().stop()?;
        std::fs::remove_dir_all(session.dir())
            .with_context(|| format!("deleting {}", session.dir().display()))?;
        Ok(Done {
            message: format!("Removed session {name} and its wallet."),
        })
    }

    fn existing_session(&self, name: &str) -> Result<Session> {
        let session = Session::new(&self.home.sessions_dir(), name)?;
        if !session.exists() {
            bail!("no session {name}. `logos-devnet session ls` lists them");
        }
        Ok(session)
    }

    /// Deploys `elf` as program `name`, and adds it to the env file.
    pub fn deploy(&self, name: &str, elf: &Path) -> Result<Program> {
        self.faucet()?;
        self.require_running()?;
        self.deploy_program(name, elf)
    }

    /// Deploys `elf` as program `name` from the dev wallet's header key for that name, so the
    /// program id is the same on every chain this home starts. A program already on the chain
    /// stays: deployed programs are immutable. When its binary changed since, that's said.
    fn deploy_program(&self, name: &str, elf: &Path) -> Result<Program> {
        envfile::check_program_name(name)?;
        let bytes =
            std::fs::read(elf).with_context(|| format!("{} does not exist", elf.display()))?;
        let hash = hex_digest(&bytes);
        let faucet = self.faucet()?;
        let mut env = self.read_env()?;
        let mut hashes = self.program_hashes()?;

        let label = format!("program-{name}");
        let header = match self.labelled_account(&label)? {
            Some(header) => header,
            None => self.new_account(Some(&label))?,
        };
        let deployed_now = !self.rpc().account_in_use(&header)?;
        if deployed_now {
            let mut args: Vec<WalletArg> = vec![
                "program-loader".into(),
                "deploy".into(),
                "--elf".into(),
                WalletArg::File(elf.to_path_buf()),
                "--header".into(),
                header.wallet_form().into(),
                "--segments".into(),
            ];
            for _ in 0..segment_count(bytes.len() as u64) {
                args.push(self.new_account(None)?.wallet_form().into());
            }
            args.extend([
                "--immutable".into(),
                "--payer".into(),
                faucet.wallet_form().into(),
            ]);
            let output = self.wallet(args)?;
            let deployed = output
                .lines()
                .find_map(|line| line.strip_prefix("Program deployed. Header account id: "))
                .with_context(|| format!("the wallet did not report a deploy:\n{output}"))?;
            if PublicId::parse(deployed.trim())? != header {
                bail!(
                    "the wallet deployed at {deployed}, not at {}",
                    header.as_str()
                );
            }
            hashes.insert(name.to_owned(), hash);
            write_json(&self.home.programs_file(), &serde_json::to_value(&hashes)?)?;
        } else if hashes.get(name).is_some_and(|deployed| *deployed != hash) {
            eprintln!(
                "{name}: {} changed since it was deployed. `logos-devnet reset` deploys the new \
                 build (programs are immutable)",
                elf.display()
            );
        }
        env.programs.insert(name.to_owned(), header.clone());
        self.write_env(&env)?;
        Ok(program_report(name, &header, deployed_now))
    }

    /// Each program's deployed binary hash, by name.
    fn program_hashes(&self) -> Result<BTreeMap<String, String>> {
        match std::fs::read_to_string(self.home.programs_file()) {
            Ok(text) => serde_json::from_str(&text).context("reading programs.json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).context("reading programs.json"),
        }
    }

    fn new_account(&self, label: Option<&str>) -> Result<PublicId> {
        let mut args: Vec<WalletArg> = vec!["account".into(), "new".into(), "public".into()];
        if let Some(label) = label {
            args.extend(["--label".into(), label.into()]);
        }
        let output = self.wallet(args)?;
        first_public_id(&output).with_context(|| format!("the wallet made no account:\n{output}"))
    }

    fn labelled_account(&self, label: &str) -> Result<Option<PublicId>> {
        let list = self.wallet(vec!["account".into(), "list".into()])?;
        Ok(labelled(&list, label))
    }

    pub fn env_vars(&self) -> Result<EnvVars> {
        Ok(EnvVars::new(self.read_env()?.vars()))
    }

    pub fn logs(&self, follow: bool) -> Result<()> {
        self.runtime.logs(follow)
    }

    /// Writes `<dir>/wallet_config.json` for a project's own wallet (`lez_core` or the LEZ
    /// CLI). Refuses a directory that already holds a wallet or a config: repointing a testnet
    /// wallet at the devnet would mix up its sync state.
    pub fn write_project_wallet_config(&self, dir: &Path) -> Result<WalletConfig> {
        let path = dir.join("wallet_config.json");
        for existing in [&path, &dir.join("storage.json")] {
            if existing.exists() {
                bail!(
                    "{} exists. Give the devnet wallet a new directory",
                    existing.display()
                );
            }
        }
        std::fs::create_dir_all(dir)?;
        write_json(&path, &wallet_config(&self.settings.sequencer_url()))?;
        Ok(WalletConfig { path })
    }
}

/// `status` when everything worked. When something failed, the status goes to stderr (stdout
/// stays clean for `--json`) and the error is returned.
fn with_status(status: Status, outcome: Result<()>) -> Result<Status> {
    match outcome {
        Ok(()) => Ok(status),
        Err(e) => {
            eprint!("{}", status.human());
            Err(e)
        }
    }
}

fn program_report(name: &str, id: &PublicId, deployed_now: bool) -> Program {
    Program {
        name: name.to_owned(),
        id: id.as_str().to_owned(),
        hex: id.hex(),
        var: envfile::program_var(name),
        deployed_now,
    }
}

/// A fresh private key: 32 random bytes as hex, the form `account import` takes.
fn random_key() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Lower-case hex SHA-256 of `bytes`.
fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The env file after `up`: a chain that carries on keeps its programs, a new one has none.
pub(crate) fn port_free(port: u16) -> Result<()> {
    std::net::TcpListener::bind(("127.0.0.1", port))
        .map(drop)
        .with_context(|| format!("port {port} on 127.0.0.1 is taken. Pick another with --port"))
}

/// Whether a running chain's genesis funded another faucet than the dev wallet holds. Without
/// a stored faucet (the settings file was lost) the chain is taken to fund the wallet's.
fn funds_another_faucet(stored: Option<&PublicId>, wallet: &PublicId) -> bool {
    stored.is_some_and(|stored| stored != wallet)
}

fn carried_env(continuing: bool, existing: Option<&str>, sequencer: String) -> Result<Env> {
    let mut env = match existing {
        Some(text) if continuing => Env::parse(text)
            .context("the env file is damaged. Delete it, then `logos-devnet up`")?,
        _ => Env::default(),
    };
    env.sequencer = sequencer;
    Ok(env)
}

fn remove_if_present(path: &Path, remove: fn(&Path) -> std::io::Result<()>) -> Result<()> {
    match remove(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("deleting {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// A wallet config in the shape `lez_core` 0.5.1 writes. It reads `sequencer.sequencer_addr`;
/// the top-level `sequencer_addr` and `sequencers` are set too, so a build that reads those
/// finds the devnet as well.
fn wallet_config(sequencer: &str) -> Value {
    json!({
        "gas_limit": 200_000,
        "multi_sequencer_client_config": { "calibration_limit": 3, "distribution_limit": 1 },
        "seq_block_poll_max_amount": 100,
        "seq_poll_max_retries": 10,
        "seq_poll_timeout": WALLET_POLL_TIMEOUT,
        "seq_tx_poll_max_blocks": 15,
        "sequencer": { "sequencer_addr": sequencer },
        "sequencer_addr": sequencer,
        "sequencers": [{ "sequencer_addr": sequencer }]
    })
}

fn segment_count(elf_len: u64) -> u64 {
    elf_len.div_ceil(SEGMENT_LEN).max(1)
}

/// The account `wallet account list` shows with `[label]`.
fn labelled(list: &str, label: &str) -> Option<PublicId> {
    let tag = format!("[{label}]");
    list.lines()
        .filter(|line| line.split_whitespace().any(|word| word == tag))
        .find_map(first_public_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d";

    #[test]
    fn makes_distinct_64_hex_keys() {
        let a = random_key().unwrap();
        let b = random_key().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn counts_segments() {
        assert_eq!(segment_count(1), 1);
        assert_eq!(segment_count(SEGMENT_LEN), 1);
        assert_eq!(segment_count(SEGMENT_LEN + 1), 2);
        // A 241,064-byte program, which the program loader took in 3 segments.
        assert_eq!(segment_count(241_064), 3);
    }

    #[test]
    fn finds_a_labelled_account() {
        // As `wallet account list` printed it.
        let list = "Public/CbgR6tj5kWx5oziiFptM7jMvrQeYY3Mzaao6ciuhSr2r\n\
                    /0 Public/Xuuz5DXfz3knxKf7jTvbhQBrrH5HzpwZnEzTEv6N3Pn [faucet]\n\
                    /1 Public/6YvesSEDWW25kpi8SSki2c8o5m3uACsigvGnNQ6wMruh [program-counter]\n";
        let found = labelled(list, "program-counter").unwrap();
        assert_eq!(
            found.as_str(),
            "6YvesSEDWW25kpi8SSki2c8o5m3uACsigvGnNQ6wMruh"
        );
        assert!(
            labelled(list, "program-count").is_none(),
            "labels match whole"
        );
        assert!(labelled(list, "missing").is_none());
    }

    #[test]
    fn wallet_configs_point_every_address_field_at_the_devnet() {
        let config = wallet_config("http://127.0.0.1:3040");
        assert_eq!(
            config["sequencer"]["sequencer_addr"],
            "http://127.0.0.1:3040"
        );
        assert_eq!(config["sequencer_addr"], "http://127.0.0.1:3040");
        assert_eq!(
            config["sequencers"][0]["sequencer_addr"],
            "http://127.0.0.1:3040"
        );
        assert_eq!(config["gas_limit"], 200_000);
        assert_eq!(config["seq_poll_timeout"], "2s");
    }

    #[test]
    fn options_change_only_what_they_name() {
        let base = Settings::default();
        let opts = UpOptions {
            port: Some(4000),
            native: true,
            ..UpOptions::default()
        };
        let applied = opts.apply(base.clone());
        assert_eq!(applied.port, 4000);
        assert_eq!(applied.runtime, RuntimeKind::Native);
        let back = UpOptions {
            docker: true,
            ..UpOptions::default()
        }
        .apply(applied.clone());
        assert_eq!(back.runtime, RuntimeKind::Docker);
        assert_eq!(
            Settings {
                port: base.port,
                runtime: base.runtime,
                ..applied
            },
            base
        );
    }

    #[test]
    fn a_lost_settings_file_keeps_the_wallets_faucet() {
        let wallet = PublicId::parse(ID).unwrap();
        let other = PublicId::parse("6YvesSEDWW25kpi8SSki2c8o5m3uACsigvGnNQ6wMruh").unwrap();
        assert!(!funds_another_faucet(None, &wallet), "settings lost");
        assert!(!funds_another_faucet(Some(&wallet), &wallet));
        assert!(
            funds_another_faucet(Some(&other), &wallet),
            "wallet replaced"
        );
    }

    #[test]
    fn a_continuing_chain_keeps_its_programs_and_a_new_one_drops_them() {
        let mut before = Env {
            sequencer: "http://127.0.0.1:3040".into(),
            ..Env::default()
        };
        before
            .programs
            .insert("counter".into(), PublicId::parse(ID).unwrap());
        let text = before.render();
        let url = "http://127.0.0.1:3041".to_owned();
        let kept = carried_env(true, Some(&text), url.clone()).unwrap();
        assert_eq!(kept.programs, before.programs);
        assert_eq!(kept.sequencer, url);
        assert!(
            carried_env(false, Some(&text), url.clone())
                .unwrap()
                .programs
                .is_empty()
        );
        assert!(
            carried_env(true, None, url.clone())
                .unwrap()
                .programs
                .is_empty()
        );
        assert!(
            carried_env(true, Some("garbage"), url).is_err(),
            "never drops programs silently"
        );
    }

    fn devnet_in(dir: &Path) -> Devnet {
        Devnet::open(Home::locate(Some(dir.into())).unwrap(), None).unwrap()
    }

    #[test]
    fn project_wallet_configs_never_touch_a_wallet() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_in(&dir.path().join("home"));
        let wallet = dir.path().join("provider");
        std::fs::create_dir_all(&wallet).unwrap();
        std::fs::write(wallet.join("storage.json"), "{}").unwrap();
        assert!(devnet.write_project_wallet_config(&wallet).is_err());
        assert!(!wallet.join("wallet_config.json").exists());
    }

    #[test]
    fn writes_a_project_wallet_config_into_a_new_directory_once() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_in(&dir.path().join("home"));
        let target = dir.path().join("new/buyer");
        let written = devnet.write_project_wallet_config(&target).unwrap();
        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(&written.path).unwrap()).unwrap();
        assert_eq!(config["sequencer_addr"], "http://127.0.0.1:3040");
        assert!(devnet.write_project_wallet_config(&target).is_err());
    }

    #[test]
    fn fund_and_deploy_refuse_before_up() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_in(dir.path());
        let err = devnet.fund(ID, 1).unwrap_err();
        assert!(err.to_string().contains("logos-devnet up"), "{err}");
        let elf = dir.path().join("program.bin");
        std::fs::write(&elf, [0u8; 8]).unwrap();
        let err = devnet.deploy("counter", &elf).unwrap_err();
        assert!(err.to_string().contains("logos-devnet up"), "{err}");
        assert!(
            devnet.deploy("Counter", &elf).is_err(),
            "bad names are refused first"
        );
    }

    #[test]
    fn up_refuses_another_lez_tag_on_an_existing_chain() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        std::fs::create_dir_all(home.chain_dir().join("rocksdb-0101")).unwrap();
        let opts = UpOptions {
            lez: Some("v9.9.9".into()),
            ..UpOptions::default()
        };
        let err = Devnet::open(home.clone(), None)
            .unwrap()
            .up(&opts)
            .unwrap_err();
        assert!(err.to_string().contains("reset --lez v9.9.9"), "{err}");
        assert!(!home.settings_file().exists(), "nothing saved");
    }

    #[test]
    fn a_taken_port_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        std::fs::create_dir_all(home.chain_dir().join("rocksdb-0101")).unwrap();
        let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let opts = UpOptions {
            port: Some(taken.local_addr().unwrap().port()),
            ..UpOptions::default()
        };
        let err = Devnet::open(home.clone(), None)
            .unwrap()
            .reset(&opts)
            .unwrap_err();
        assert!(err.to_string().contains("is taken"), "{err}");
        assert!(!home.settings_file().exists(), "nothing saved");
        assert!(home.has_chain(), "nothing deleted");
    }

    #[test]
    fn a_bad_block_time_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        std::fs::create_dir_all(home.chain_dir().join("rocksdb-0101")).unwrap();
        for command in [Devnet::up, Devnet::reset] {
            let opts = UpOptions {
                block_time: Some("1".into()),
                ..UpOptions::default()
            };
            assert!(command(Devnet::open(home.clone(), None).unwrap(), &opts).is_err());
            assert!(!home.settings_file().exists(), "nothing saved");
            assert!(home.has_chain(), "nothing deleted");
        }
    }
}
