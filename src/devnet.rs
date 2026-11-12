//! The commands: start a chain, fund accounts, deploy programs, and write what projects need.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use std::collections::BTreeMap;

use crate::account::{PublicId, first_public_id};
use crate::binary;
use crate::chain_config::{self, ChainOptions};
use crate::envfile::{self, Env};
use crate::fetch;
use crate::groups::{self, Group, GroupInfo};
use crate::home::{Home, RuntimeKind, Settings, write_json};
use crate::messaging::{Hub, Wants};
use crate::project::{ProgramSource, ProgramSpec, Project};
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
/// How long `up` waits for the first block.
const READY_TIMEOUT: Duration = Duration::from_secs(180);
/// How long `status` waits for the runtime to say whether the sequencer runs.
const RUNTIME_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
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
    /// Skip proving: private calls take a second, with fake proofs. `--dev-mode false` turns it
    /// off. A chain keeps it, so changing it takes `reset`.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", value_name = "BOOL")]
    pub dev_mode: Option<bool>,
    /// The sequencer's log filter, as `RUST_LOG` takes it: e.g. `debug`, or
    /// `info,sequencer_core=debug`.
    #[arg(long)]
    pub log_filter: Option<String>,
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
        if let Some(dev_mode) = self.dev_mode {
            settings.dev_mode = dev_mode;
        }
        if let Some(log_filter) = &self.log_filter {
            settings.log_filter.clone_from(log_filter);
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
        let mut settings = home.load_settings()?;
        // `port = 0`: a devnet that hasn't run yet takes a free port, which `up` keeps.
        let auto_port = project.as_ref().is_some_and(|p| p.devnet.port == Some(0));
        if auto_port && !home.settings_file().exists() {
            settings.port = free_port()?;
        }
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

    /// The stored settings: what runs, or last ran.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Refuses `settings` the chain here can't run on: another LEZ tag or dev mode. Its own
    /// record of its dev mode counts, not settings.json's.
    fn check_chain_takes(&self, settings: &Settings) -> Result<()> {
        let chain = Settings {
            dev_mode: self.home.chain_dev_mode(),
            ..self.settings.clone()
        };
        check_chain_keeps(&chain, settings)
    }

    /// Whether `up` would take the project file's settings here, as `up` decides it: a running
    /// shared devnet keeps what it runs with, and a chain keeps its LEZ tag and dev mode. True
    /// when a chain exists.
    pub fn check_chain_for_project(&self) -> Result<bool> {
        let wanted = self.with_project_settings(&UpOptions::default(), self.runtime.running())?;
        let has_chain = self.home.has_chain();
        if has_chain {
            self.check_chain_takes(&wanted)?;
        }
        Ok(has_chain)
    }

    /// The settings `up` would start with: the stored ones, with the project's `[devnet]`
    /// section applied.
    pub fn settings_for_up(&self) -> Settings {
        match &self.project {
            Some(project) => project.devnet.applied_to(self.settings.clone()),
            None => self.settings.clone(),
        }
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
            || self.wants_hub()
    }

    fn hub(&self) -> Hub {
        Hub::new(self.home.messaging_dir())
    }

    /// What the project asks the hub to run.
    fn hub_wants(&self) -> Wants {
        self.project
            .as_ref()
            .map_or_else(Wants::default, |p| Wants {
                messaging: p.messaging,
                storage: p.storage,
            })
    }

    /// Whether `up` runs the hub: the project asks for messaging or storage, or the devnet has
    /// a hub. Another project's file that leaves them out doesn't stop it.
    fn wants_hub(&self) -> bool {
        let wants = self.hub_wants();
        wants.messaging || wants.storage || self.hub().exists()
    }

    /// Starts the hub, or brings back what it lacks, when `up` runs it.
    fn ensure_hub(&self) -> Result<()> {
        if !self.wants_hub() {
            return Ok(());
        }
        self.hub()
            .ensure(self.settings.dev_mode, self.hub_wants())
            .context("the messaging hub")
    }

    /// The storage config of a session's daemon: the hub's storage node as its only bootstrap
    /// node when the hub runs one, or else a node of its own that joins no network.
    fn session_storage(&self) -> crate::storage::Config {
        self.hub()
            .storage_client()
            .unwrap_or_else(crate::storage::isolated)
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
        crate::home::check_lez(&settings.lez)?;
        chain_config::block_time_secs(&settings.block_time)?;
        chain_config::check_max_txs_per_block(settings.max_txs_per_block)?;
        crate::home::check_log_filter(&settings.log_filter)?;
        let continuing = !wipe && self.home.has_chain();
        if continuing {
            self.check_chain_takes(&settings)?;
        }
        // The devnet as it will run: its runtime is the one that has to read the binaries.
        let mut next = Self::with(self.home.clone(), settings.clone(), self.project.clone());
        next.check_needs(session::logosctl_on_path)?;
        // Nothing to restart, unless the settings file is gone and needs writing again.
        let unchanged = settings == self.settings && self.home.settings_file().exists();
        if !wipe && unchanged && !opts.rebuild && running {
            eprintln!("Already running.");
            // Its container may run before the sequencer answers: a long chain takes a while to
            // load, and an `up` stopped while waiting leaves it starting.
            self.wait_ready()?;
            self.make_project_groups()?;
            let messaging = self.ensure_hub();
            self.write_env(&self.read_env().unwrap_or_default())?;
            let mut deployed = Vec::new();
            let applied = self.apply_project(&mut deployed);
            let forgot = self.forget_group_chains();
            let resumed = self.resume_sessions(false);
            return with_status(
                with_deploys(self.report(Some(true)), &deployed),
                after_start([forgot, applied, resumed, messaging]),
            );
        }
        // The running sequencer holds its own port, and gives it up when it stops.
        if !(running && settings.port == self.settings.port) {
            port_free(settings.port)?;
        }

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
        next.home.record_chain_dev_mode(next.settings.dev_mode)?;
        // Every one is tried, and a failure is reported once the chain runs.
        let marked = if continuing {
            Ok(())
        } else {
            next.mark_new_chain()
        };
        next.runtime.start()?;
        next.wait_ready()?;
        let messaging = next.ensure_hub();

        let existing = std::fs::read_to_string(next.home.env_file()).ok();
        let env = carried_env(
            continuing,
            existing.as_deref(),
            next.settings.sequencer_url(),
        )?;
        next.write_env(&env)?;
        // Marked on a new chain, or left marked by an `up` that failed.
        let forgot = next.forget_group_chains();
        let resumed = next.resume_sessions(!continuing);
        let mut deployed = Vec::new();
        let applied = next.apply_project(&mut deployed);
        with_status(
            with_deploys(next.report(Some(true)), &deployed),
            after_start([marked, forgot, resumed, applied, messaging]),
        )
    }

    /// The stored settings with the project's `[devnet]` section applied. A shared devnet that
    /// runs with other settings is not restarted for one project (`refuse_changes`), unless
    /// `opts` asks for the same change explicitly.
    fn with_project_settings(&self, opts: &UpOptions, refuse_changes: bool) -> Result<Settings> {
        let Some(project) = &self.project else {
            return Ok(self.settings.clone());
        };
        let wanted = self.settings_for_up();
        // A project with its own home is the only one on its devnet: its file just applies.
        let shared = project.home.is_none();
        if refuse_changes
            && shared
            && opts.apply(wanted.clone()) != opts.apply(self.settings.clone())
        {
            // A LEZ tag or dev mode only changes with a new chain, so `down` and `up` won't do.
            let apply = if check_chain_keeps(&self.settings, &opts.apply(wanted.clone())).is_ok() {
                "`logos-devnet down` and `up` apply it, restarting the chain"
            } else {
                "`logos-devnet reset` applies it, with a new chain"
            };
            bail!(
                "the shared devnet runs with {}, but {} asks for {}. {apply} for everything on \
                 it, or give this project its own devnet with `home = \".logos-devnet\"` in the \
                 file",
                project.devnet.describe(&self.settings),
                project.path.display(),
                project.devnet.describe(&wanted)
            );
        }
        Ok(wanted)
    }

    /// Where `program`'s binary is on this machine: its path, or its release, downloaded into
    /// the home once and checked against its SHA-256.
    fn program_binary(&self, program: &ProgramSpec) -> Result<PathBuf> {
        match &program.source {
            ProgramSource::File(path) if !path.is_file() => {
                bail!("{} doesn't exist. Build it first", path.display())
            }
            ProgramSource::File(path) => Ok(path.clone()),
            ProgramSource::Release { url, sha256 } => {
                fetch::cached(&self.home.downloads_dir(), url, sha256)
            }
        }
    }

    /// What `up` can't make by itself, checked before anything changes: the project's program
    /// binaries (downloading releases), and logosctl when sessions exist or the project declares
    /// some, since `up` resumes or makes each one.
    fn check_needs(&self, logosctl_on_path: impl FnOnce() -> bool) -> Result<()> {
        if let Some(project) = &self.project {
            for (name, program) in &project.programs {
                let binary = self
                    .program_binary(program)
                    .with_context(|| format!("[programs] {name}"))?;
                std::fs::read(&binary)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| binary::segments(&bytes))
                    .with_context(|| format!("[programs] {name}: {}", binary.display()))?;
                self.runtime.check_wallet_file(&binary)?;
            }
        }
        if self.needs_logosctl() && !logosctl_on_path() {
            bail!(
                "sessions and the messaging hub need logosctl, the Logos Core CLI, on your PATH. \
                 Install it, or drop what you no longer need: a session with `logos-devnet \
                 session rm <name>` and from [sessions] in the project file, and the hub \
                 from [messaging], then `logos-devnet messaging rm`"
            );
        }
        let hub = self.hub();
        if self.wants_hub() && !hub.running() {
            for port in hub.ports() {
                if port_free(port).is_err() {
                    bail!(
                        "port {port} on 127.0.0.1, which the messaging hub keeps, is taken. Free \
                         it, or `logos-devnet messaging rm` and `up` give the hub new ports and \
                         peer ids"
                    );
                }
            }
        }
        Ok(())
    }

    /// Makes every account group's wallet that's marked as on a new chain forget the old one:
    /// an app that used a group wallet would otherwise skip the new chain's blocks up to the old
    /// height. Keys and accounts stay. Every group is tried.
    fn forget_group_chains(&self) -> Result<()> {
        let mut failed = Vec::new();
        for group in groups::list(&self.home.groups_dir())? {
            if let Err(e) = group.forget_old_chain() {
                eprintln!("account group {}: {e:#}", group.name);
                failed.push(group.name);
            }
        }
        if !failed.is_empty() {
            bail!(
                "account group wallets {} kept the old chain; the next `up` tries again",
                failed.join(", ")
            );
        }
        Ok(())
    }

    /// Marks every session and account group as on a new chain, once the old one is gone, so
    /// their wallets forget it even if starting the new one fails first.
    fn mark_new_chain(&self) -> Result<()> {
        let mut failed = Vec::new();
        for session in self.sessions() {
            if let Err(e) = session.mark_new_chain() {
                eprintln!("session {}: {e:#}", session.name);
                failed.push(format!("session {}", session.name));
            }
        }
        for group in groups::list(&self.home.groups_dir())? {
            if let Err(e) = group.mark_new_chain() {
                eprintln!("account group {}: {e:#}", group.name);
                failed.push(format!("account group {}", group.name));
            }
        }
        if !failed.is_empty() {
            bail!(
                "{} couldn't be marked, so their wallets may keep the old chain",
                failed.join(", ")
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
    /// failures are reported together after. Each program it deploys or updates is added to
    /// `deployed`.
    fn apply_project(&self, deployed: &mut Vec<Program>) -> Result<()> {
        let mut failed: Vec<String> = Vec::new();
        if let Some(project) = &self.project {
            for (name, program) in &project.programs {
                let deployed_one = self
                    .program_binary(program)
                    .and_then(|binary| self.deploy_program(name, &binary, program.mutable));
                match deployed_one {
                    Ok(report) => deployed.push(report),
                    Err(e) => {
                        eprintln!("program {name}: {e:#}");
                        failed.push(format!("program {name}"));
                    }
                }
            }
        }
        if let Err(e) = self.fund_groups(None) {
            eprintln!("accounts: {e:#}");
            failed.push("accounts".into());
        }
        if let Some(project) = &self.project {
            for (name, spec) in &project.sessions {
                let session = Session::new(&self.home.sessions_dir(), name)?;
                if session.exists() {
                    match self.add_session_modules(&session, &spec.modules) {
                        Ok(added) if !added.is_empty() => {
                            eprintln!("session {name}: added {}", added.join(", "));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("session {name}: {e:#}");
                            failed.push(format!("session {name}"));
                        }
                    }
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
            bail!("these failed: {}", failed.join(", "));
        }
        Ok(())
    }

    /// Group `name` with at least `count` accounts, each to be funded with `amount`. Missing
    /// keys are made in one wallet batch. A group never shrinks: its keys stay.
    fn make_group(&self, name: &str, count: usize, amount: u64) -> Result<GroupInfo> {
        let group = Group::new(&self.home.groups_dir(), name)?;
        if !group.wallet_dir().exists() {
            groups::check_group_name(name)?;
        }
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
    /// (`new_chain`) its account is funded again and its wallet forgets the old chain. Every
    /// session is tried; failures are reported together after, with the chain already up.
    fn resume_sessions(&self, new_chain: bool) -> Result<()> {
        let mut failed = Vec::new();
        let sessions = self.sessions();
        for session in sessions {
            let resumed = session.info().and_then(|info| {
                // Funding only needs the dev wallet: a daemon that won't start doesn't cost the
                // account its funds.
                let funded = if new_chain {
                    self.send(&info.account, info.funded).map(drop)
                } else {
                    Ok(())
                };
                session.forget_old_chain()?;
                self.write_session_wallet_config(&session)?;
                let ctl = session.logosctl();
                session.start_daemon(&ctl, self.settings.dev_mode, &self.session_storage())?;
                ctl.resume(
                    &info.modules,
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
                "sessions {} failed. If one keeps failing, \
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
            dev_mode: self.settings.dev_mode,
            messaging: self.hub().addresses(),
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
        if let Err(e) = self.hub().stop() {
            eprintln!("messaging hub: {e:#}");
        }
        self.runtime.stop()?;
        Ok(Done {
            message: "Stopped. The chain and sessions are kept; `logos-devnet up` resumes them."
                .into(),
        })
    }

    /// What runs. When the runtime doesn't answer within `RUNTIME_CHECK_TIMEOUT`, whether the
    /// sequencer runs is unknown: `runtime_answered` is false, and a note says why.
    pub fn status(&self) -> Status {
        let running = self
            .runtime
            .running_within(RUNTIME_CHECK_TIMEOUT)
            .map_err(|e| eprintln!("{e:#}, so whether the sequencer runs is unknown."))
            .ok();
        self.report(running)
    }

    /// `status`, with whether the sequencer runs already known: `up` has just seen it serve.
    /// `None` when the runtime didn't say.
    fn report(&self, running: Option<bool>) -> Status {
        let s = &self.settings;
        let runtime_answered = running.is_some();
        let running = running.unwrap_or(false);
        let head = running.then(|| self.rpc().last_block_id().ok()).flatten();
        let env = self.read_env().ok();
        let programs = env
            .as_ref()
            .map(|env| {
                env.programs
                    .iter()
                    .map(|(name, id)| program_report(name, id, DeployStep::Unchanged))
                    .collect()
            })
            .unwrap_or_default();
        Status {
            lez: s.lez.clone(),
            runtime: self.runtime.describe(),
            sequencer: report::Sequencer {
                url: s.sequencer_url(),
                running,
                runtime_answered,
                block: head,
                max_txs_per_block: s.max_txs_per_block,
                block_time: s.block_time.clone(),
                dev_mode: s.dev_mode,
            },
            faucet: s.faucet.as_ref().map(|faucet| Balance {
                account: faucet.wallet_form(),
                balance: head.and_then(|_| self.rpc().balance(faucet).ok()),
            }),
            programs,
            account_groups: self.group_summaries().unwrap_or_default(),
            sessions: self.sessions().into_iter().map(|s| s.name).collect(),
            messaging: self.hub_status(),
            storage: self.storage_status(),
            env_file: env.map(|_| self.home.env_file()),
        }
    }

    fn hub_status(&self) -> Option<report::Messaging> {
        let hub = self.hub();
        hub.runs_messaging().then(|| {
            let health = hub.health();
            report::Messaging {
                running: health.daemon,
                delivery_node: health.delivery,
                discovery_node: health.discovery,
                addresses: hub.addresses(),
            }
        })
    }

    fn storage_status(&self) -> Option<report::Storage> {
        let hub = self.hub();
        hub.runs_storage().then(|| report::Storage {
            running: hub.health().storage,
        })
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

    /// Installs and loads the modules `extra` names that `session` lacks, and records them, so
    /// a module added to `[sessions.<name>]` after the session was made is there too. Returns
    /// the ones added.
    fn add_session_modules(&self, session: &Session, extra: &[String]) -> Result<Vec<String>> {
        let mut info = session.info()?;
        let missing = session::missing_modules(&info.modules, extra)?;
        if missing.is_empty() {
            return Ok(missing);
        }
        let ctl = session.logosctl();
        if session.start_daemon(&ctl, self.settings.dev_mode, &self.session_storage())? {
            // A daemon that started now has nothing loaded: bring the session back first.
            ctl.resume(
                &info.modules,
                &session.wallet_config(),
                &session.wallet_storage(),
                &self.settings.sequencer_url(),
            )?;
        }
        // Recorded one at a time, so a module that fails keeps the ones before it.
        for module in &missing {
            let one = std::slice::from_ref(module);
            ctl.install(one)?;
            // Recorded once installed: every `up` loads recorded modules that aren't loaded.
            info.modules.push(module.clone());
            session.save_info(&info)?;
            ctl.load(one)?;
        }
        Ok(missing)
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
        session.start_daemon(&ctl, self.settings.dev_mode, &self.session_storage())?;
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

    /// Stops the messaging hub and deletes it, and drops its variables from the env file.
    pub fn messaging_rm(&self) -> Result<Done> {
        let env = if self.home.env_file().exists() {
            Some(self.read_env()?)
        } else {
            None
        };
        let exported = env.as_ref().is_some_and(|env| env.messaging.is_some());
        if !self.home.messaging_dir().exists() && !exported {
            bail!("this devnet has no messaging hub");
        }
        if self.home.messaging_dir().exists() {
            self.hub().remove()?;
        }
        if let Some(env) = env {
            self.write_env(&env)?;
        }
        let message = match self.project.as_ref().filter(|p| p.messaging || p.storage) {
            Some(project) => format!(
                "Removed the messaging hub. {} asks for one, so the next `up` starts a new hub, \
                 with new ports and peer ids.",
                project.path.display()
            ),
            None => "Removed the messaging hub.".to_owned(),
        };
        Ok(Done { message })
    }

    fn existing_session(&self, name: &str) -> Result<Session> {
        let session = Session::new(&self.home.sessions_dir(), name)?;
        if !session.exists() {
            bail!("no session {name}. `logos-devnet session ls` lists them");
        }
        Ok(session)
    }

    /// Deploys `elf` as program `name`, and adds it to the env file.
    pub fn deploy(&self, name: &str, elf: &Path, mutable: bool) -> Result<Program> {
        self.faucet()?;
        self.require_running()?;
        self.deploy_program(name, elf, mutable)
    }

    /// Deploys `elf` as program `name` from the dev wallet's header key for that name, so the
    /// program id is the same on every chain this home starts. A `mutable` program whose binary
    /// changed since is updated in place. An immutable one keeps its old build, and that's an
    /// error: what runs isn't what was asked for.
    fn deploy_program(&self, name: &str, elf: &Path, mutable: bool) -> Result<Program> {
        envfile::check_program_name(name)?;
        let bytes =
            std::fs::read(elf).with_context(|| format!("{} does not exist", elf.display()))?;
        // Before any account is made: the loader takes exactly this many segments.
        let segments = binary::segments(&bytes).with_context(|| elf.display().to_string())?;
        let hash = binary::sha256_hex(&bytes);
        let faucet = self.faucet()?;
        let mut env = self.read_env()?;
        let mut records = self.program_records()?;

        let label = format!("program-{name}");
        let header = match self.labelled_account(&label)? {
            Some(header) => header,
            None => self.new_account(Some(&label))?,
        };
        let in_use = self.rpc().account_in_use(&header)?;
        let record = records.get(name).cloned();
        let step = deploy_step(in_use, record.as_ref(), &hash, mutable);
        if let Some(command) = step.loader_command() {
            let mut args: Vec<WalletArg> = vec![
                "program-loader".into(),
                command.into(),
                "--elf".into(),
                WalletArg::File(elf.to_path_buf()),
                "--header".into(),
                header.wallet_form().into(),
                "--segments".into(),
            ];
            // Segments are write-once: an update writes a new chain of them.
            for _ in 0..segments {
                args.push(self.new_account(None)?.wallet_form().into());
            }
            if !mutable {
                args.push("--immutable".into());
            }
            args.extend(["--payer".into(), faucet.wallet_form().into()]);
            let output = self.wallet(args)?;
            let reported = step
                .reported_header(&output)
                .with_context(|| format!("the wallet did not report the {command}:\n{output}"))?;
            if PublicId::parse(reported.trim())? != header {
                bail!(
                    "the wallet's {command} went to {reported}, not {}",
                    header.as_str()
                );
            }
            records.insert(name.to_owned(), DeployedProgram { hash, mutable });
            write_json(&self.home.programs_file(), &serde_json::to_value(&records)?)?;
        }
        env.programs.insert(name.to_owned(), header.clone());
        self.write_env(&env)?;
        if step == DeployStep::NeedsReset
            && let Some(record) = record
        {
            // The old build is still what runs: a caller that goes on would test the wrong one.
            bail!(needs_reset(name, elf, record.mutable, mutable));
        }
        Ok(program_report(name, &header, step))
    }

    /// What each program was deployed from, by name.
    fn program_records(&self) -> Result<BTreeMap<String, DeployedProgram>> {
        match std::fs::read_to_string(self.home.programs_file()) {
            Ok(text) => parse_program_records(&text).context("reading programs.json"),
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

/// `up`'s outcome once the devnet runs: every failure among `outcomes`, after one note that the
/// devnet itself is up.
fn after_start<const N: usize>(outcomes: [Result<()>; N]) -> Result<()> {
    let failures: Vec<String> = outcomes
        .into_iter()
        .filter_map(|outcome| outcome.err().map(|e| format!("{e:#}")))
        .collect();
    match failures.as_slice() {
        [] => Ok(()),
        [one] => Err(anyhow::anyhow!("the devnet is up, but {one}")),
        many => Err(anyhow::anyhow!(
            "the devnet is up, but:\n- {}",
            many.join("\n- ")
        )),
    }
}

/// `status`, whose programs say only what's on the chain, with `deployed` saying what this run
/// deployed or updated.
fn with_deploys(mut status: Status, deployed: &[Program]) -> Status {
    for program in &mut status.programs {
        if let Some(done) = deployed.iter().find(|d| d.name == program.name) {
            program.deployed_now = done.deployed_now;
            program.updated = done.updated;
        }
    }
    status
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

fn program_report(name: &str, id: &PublicId, step: DeployStep) -> Program {
    Program {
        name: name.to_owned(),
        id: id.as_str().to_owned(),
        hex: id.hex(),
        var: envfile::program_var(name),
        deployed_now: step == DeployStep::Deploy,
        updated: step == DeployStep::Update,
    }
}

/// What a program was deployed from, in `programs.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DeployedProgram {
    /// The binary's SHA-256.
    hash: String,
    /// Deployed mutable, so a new build updates it.
    mutable: bool,
}

/// `programs.json`. v0.2 kept only each binary's hash, and deployed every program immutable.
fn parse_program_records(text: &str) -> Result<BTreeMap<String, DeployedProgram>> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Record {
        Hash(String),
        Deployed(DeployedProgram),
    }
    let records: BTreeMap<String, Record> = serde_json::from_str(text)?;
    Ok(records
        .into_iter()
        .map(|(name, record)| {
            let deployed = match record {
                Record::Hash(hash) => DeployedProgram {
                    hash,
                    mutable: false,
                },
                Record::Deployed(deployed) => deployed,
            };
            (name, deployed)
        })
        .collect())
}

/// What deploying a program does, given whether its header is on the chain (`in_use`), what
/// it was deployed from (`record`), and the binary's `hash` now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeployStep {
    Deploy,
    Update,
    Unchanged,
    /// An immutable program's binary changed, or its mutability did: only a new chain takes it.
    NeedsReset,
}

impl DeployStep {
    /// The `wallet program-loader` subcommand that does it, if one does.
    fn loader_command(self) -> Option<&'static str> {
        match self {
            DeployStep::Deploy => Some("deploy"),
            DeployStep::Update => Some("update"),
            DeployStep::Unchanged | DeployStep::NeedsReset => None,
        }
    }

    /// The header the wallet names in `output` for having done it.
    fn reported_header(self, output: &str) -> Option<&str> {
        output.lines().find_map(|line| match self {
            DeployStep::Deploy => line.strip_prefix("Program deployed. Header account id: "),
            _ => line
                .strip_prefix("Program ")
                .and_then(|rest| rest.strip_suffix(" updated")),
        })
    }
}

/// Why a program can't take `elf` on this chain, and what does.
fn needs_reset(name: &str, elf: &Path, deployed_mutable: bool, mutable: bool) -> String {
    let kind = |mutable| if mutable { "mutable" } else { "immutable" };
    if deployed_mutable == mutable {
        format!(
            "{} changed since it was deployed, immutable, and the old build still runs. \
             `logos-devnet reset` deploys the new one",
            elf.display()
        )
    } else {
        format!(
            "{name} is deployed {}, asked for {}, and nothing changed. `logos-devnet reset` \
             deploys it {}",
            kind(deployed_mutable),
            kind(mutable),
            kind(mutable)
        )
    }
}

/// `mutable` is what's asked for now. Mutability never changes on a chain: an update that made a
/// program immutable could never be undone, and one asked for by a missing `--mutable` least of
/// all.
fn deploy_step(
    in_use: bool,
    record: Option<&DeployedProgram>,
    hash: &str,
    mutable: bool,
) -> DeployStep {
    match record {
        _ if !in_use => DeployStep::Deploy,
        // Deployed some other way: nothing to compare with.
        None => DeployStep::Unchanged,
        Some(record) if record.mutable != mutable => DeployStep::NeedsReset,
        Some(record) if record.hash == hash => DeployStep::Unchanged,
        Some(record) if record.mutable => DeployStep::Update,
        Some(_) => DeployStep::NeedsReset,
    }
}

/// A fresh private key: 32 random bytes as hex, the form `account import` takes.
pub(crate) fn random_key() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("reading /dev/urandom")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A port on 127.0.0.1 that nothing listens on now.
pub(crate) fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).context("finding a free port")?;
    Ok(listener.local_addr()?.port())
}

/// Fails when something listens on `port` on 127.0.0.1.
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

/// Refuses `next` settings an existing chain (`chain`) can't run on: another LEZ tag, or
/// another dev mode (a chain with fake proofs crashes without it).
fn check_chain_keeps(chain: &Settings, next: &Settings) -> Result<()> {
    if next.lez != chain.lez {
        bail!(
            "this chain runs LEZ {}. `logos-devnet reset --lez {}` starts a new chain on it",
            chain.lez,
            next.lez
        );
    }
    match (chain.dev_mode, next.dev_mode) {
        (false, true) => bail!(
            "this chain checks proofs. `logos-devnet reset --dev-mode` starts a new chain in \
             dev mode"
        ),
        (true, false) => bail!(
            "this chain runs in dev mode, and its fake proofs need it. `--dev-mode` keeps it, \
             and `logos-devnet reset --dev-mode false` starts a new chain that checks proofs"
        ),
        _ => Ok(()),
    }
}

/// The env file after `up`: a chain that carries on keeps its programs, a new one has none.
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
    fn refuses_a_binary_docker_cant_mount_before_touching_the_devnet() {
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build:debug");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(
            build.join("counter.bin"),
            crate::binary::tests::program_binary(1000),
        )
        .unwrap();
        std::fs::write(
            dir.path().join(crate::project::FILE_NAME),
            "[programs]\ncounter = \"build:debug/counter.bin\"\n",
        )
        .unwrap();
        let project = Project::load(&dir.path().join(crate::project::FILE_NAME)).unwrap();
        let home = Home::locate(Some(dir.path().join("home"))).unwrap();
        let devnet = Devnet::open(home, Some(project)).unwrap();
        assert_eq!(devnet.settings.runtime, RuntimeKind::Docker);
        let err = devnet.check_needs(|| true).unwrap_err();
        assert!(format!("{err:#}").contains("holds a `:`"), "{err:#}");
    }

    /// A devnet in `dir/home` for a project file in `dir` holding `text`.
    fn devnet_with_file(dir: &Path, text: &str) -> Devnet {
        let file = dir.join(crate::project::FILE_NAME);
        std::fs::write(&file, text).unwrap();
        let home = Home::locate(Some(dir.join("home"))).unwrap();
        Devnet::open(home, Some(Project::load(&file).unwrap())).unwrap()
    }

    /// A devnet in `dir/home` for a project file in `dir` holding `programs`.
    fn devnet_with_programs(dir: &Path, programs: &str) -> Devnet {
        devnet_with_file(dir, &format!("[programs]\n{programs}\n"))
    }

    #[test]
    fn port_zero_takes_a_free_port_once_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_with_file(dir.path(), "home = \"home\"\n[devnet]\nport = 0\n");
        let first = devnet.settings_for_up().port;
        assert_ne!(first, 0);
        // Once `up` saved it (or `--port` changed it), the file's 0 keeps it.
        let mut saved = devnet.settings.clone();
        saved.port = 4567;
        devnet.home.save_settings(&saved).unwrap();
        let again = devnet_with_file(dir.path(), "home = \"home\"\n[devnet]\nport = 0\n");
        assert_eq!(again.settings_for_up().port, 4567);
        // A fixed port in the file still wins.
        let fixed = devnet_with_file(dir.path(), "home = \"home\"\n[devnet]\nport = 4100\n");
        assert_eq!(fixed.settings_for_up().port, 4100);
    }

    #[test]
    fn the_messaging_hub_needs_logosctl_before_the_devnet_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_with_file(dir.path(), "[messaging]\n");
        let err = devnet.check_needs(|| false).unwrap_err();
        assert!(
            format!("{err:#}").contains("messaging hub need logosctl"),
            "{err:#}"
        );
        assert!(devnet.check_needs(|| true).is_ok());
    }

    #[test]
    fn a_hub_left_by_another_project_keeps_running() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_with_file(dir.path(), "");
        assert!(!devnet.wants_hub());
        std::fs::create_dir_all(devnet.home.messaging_dir()).unwrap();
        std::fs::write(devnet.home.messaging_dir().join("hub.json"), "{}").unwrap();
        assert!(devnet.wants_hub());
    }

    #[test]
    fn sessions_bootstrap_from_the_hubs_storage_node_when_it_runs_one() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_in(&dir.path().join("home"));
        assert_eq!(devnet.session_storage(), crate::storage::isolated());
        std::fs::create_dir_all(devnet.home.messaging_dir()).unwrap();
        std::fs::write(
            devnet.home.messaging_dir().join("hub.json"),
            r#"{"delivery_port":1,"discovery_port":2,"delivery_key":"","discovery_key":"",
                "messaging":false,"storage_port":3,"storage_spr":"spr:hub","storage_peer":"peer"}"#,
        )
        .unwrap();
        assert_eq!(
            devnet.session_storage(),
            crate::storage::client("spr:hub", "peer")
        );
    }

    #[test]
    fn the_env_file_points_at_the_hubs_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_in(&dir.path().join("home"));
        std::fs::create_dir_all(devnet.home.messaging_dir()).unwrap();
        std::fs::write(
            devnet.home.messaging_dir().join("hub.json"),
            r#"{"delivery_port":1,"discovery_port":2,"delivery_key":"","discovery_key":"",
                "addresses":{"delivery":"/ip4/127.0.0.1/tcp/1/p2p/A","discovery":"/ip4/127.0.0.1/tcp/2/p2p/B"}}"#,
        )
        .unwrap();
        devnet.write_env(&Env::default()).unwrap();
        let env = devnet.read_env().unwrap();
        let hub = env.messaging.expect("the hub's addresses");
        assert_eq!(hub.delivery, "/ip4/127.0.0.1/tcp/1/p2p/A");
        assert_eq!(hub.discovery, "/ip4/127.0.0.1/tcp/2/p2p/B");
    }

    #[test]
    fn removing_the_hub_drops_its_variables() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_with_file(dir.path(), "");
        // A hub with no ports recorded yet, so removing it needs none free.
        std::fs::create_dir_all(devnet.home.messaging_dir()).unwrap();
        let env = Env {
            sequencer: devnet.settings.sequencer_url(),
            messaging: Some(crate::messaging::Addresses {
                delivery: "/ip4/127.0.0.1/tcp/1/p2p/A".into(),
                discovery: "/ip4/127.0.0.1/tcp/2/p2p/B".into(),
            }),
            ..Env::default()
        };
        std::fs::write(devnet.home.env_file(), env.render()).unwrap();
        devnet.messaging_rm().unwrap();
        assert!(!devnet.home.messaging_dir().exists());
        assert!(devnet.read_env().unwrap().messaging.is_none());
        assert!(devnet.messaging_rm().is_err(), "nothing left to remove");
    }

    #[test]
    fn says_when_the_next_up_makes_a_new_hub() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_with_file(dir.path(), "[messaging]\n");
        std::fs::create_dir_all(devnet.home.messaging_dir()).unwrap();
        let done = devnet.messaging_rm().unwrap();
        assert!(
            done.message.contains("the next `up` starts a new hub"),
            "{}",
            done.message
        );
        assert!(!devnet.home.messaging_dir().exists());
    }

    #[test]
    fn refuses_a_taken_hub_port_before_touching_the_devnet() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_with_file(dir.path(), "[messaging]\n");
        let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        std::fs::create_dir_all(devnet.home.messaging_dir()).unwrap();
        std::fs::write(
            devnet.home.messaging_dir().join("hub.json"),
            format!(
                r#"{{"delivery_port":{port},"discovery_port":1,"delivery_key":"","discovery_key":""}}"#
            ),
        )
        .unwrap();
        let err = devnet.check_needs(|| true).unwrap_err();
        assert!(
            format!("{err:#}").contains("the messaging hub keeps"),
            "{err:#}"
        );
    }

    #[test]
    fn refuses_a_missing_build_before_touching_the_devnet() {
        let dir = tempfile::tempdir().unwrap();
        let devnet = devnet_with_programs(dir.path(), "counter = \"missing.bin\"");
        let err = devnet.check_needs(|| true).unwrap_err();
        assert!(format!("{err:#}").contains("Build it first"), "{err:#}");
    }

    #[test]
    fn refuses_a_release_with_another_sha256_before_touching_the_devnet() {
        let dir = tempfile::tempdir().unwrap();
        let url = crate::fetch::tests::serve("200 OK", b"something else", 1);
        let sha = binary::sha256_hex(b"a program");
        let devnet = devnet_with_programs(
            dir.path(),
            &format!("token = {{ url = \"{url}\", sha256 = \"{sha}\" }}"),
        );
        let err = devnet.check_needs(|| true).unwrap_err();
        assert!(format!("{err:#}").contains("has SHA-256"), "{err:#}");
    }

    #[test]
    fn checks_a_downloaded_release_like_a_build() {
        let dir = tempfile::tempdir().unwrap();
        let url = crate::fetch::tests::serve("200 OK", b"not a program", 1);
        let sha = binary::sha256_hex(b"not a program");
        let devnet = devnet_with_programs(
            dir.path(),
            &format!("token = {{ url = \"{url}\", sha256 = \"{sha}\" }}"),
        );
        let err = devnet.check_needs(|| true).unwrap_err();
        assert!(format!("{err:#}").contains("not a RISC Zero"), "{err:#}");
    }

    #[test]
    fn existing_sessions_need_logosctl_before_the_devnet_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        // No project file: the session was made with `session new`.
        std::fs::create_dir_all(home.sessions_dir().join("alice")).unwrap();
        let devnet = Devnet::open(home, None).unwrap();
        let err = devnet.check_needs(|| false).unwrap_err();
        assert!(format!("{err:#}").contains("logosctl"), "{err:#}");
        assert!(devnet.check_needs(|| true).is_ok());
    }

    #[test]
    fn a_devnet_without_sessions_needs_no_logosctl() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        assert!(
            Devnet::open(home, None)
                .unwrap()
                .check_needs(|| panic!("no session needs it"))
                .is_ok()
        );
    }

    #[test]
    fn makes_distinct_64_hex_keys() {
        let a = random_key().unwrap();
        let b = random_key().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
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
    fn reads_program_records_from_v0_2_and_now() {
        let records =
            parse_program_records(r#"{"counter":"aa","shop":{"hash":"bb","mutable":true}}"#)
                .unwrap();
        assert_eq!(
            records["counter"],
            DeployedProgram {
                hash: "aa".into(),
                mutable: false
            },
            "v0.2 deployed every program immutable"
        );
        assert!(records["shop"].mutable);
    }

    #[test]
    fn a_deploy_updates_only_a_changed_mutable_program() {
        let mutable = DeployedProgram {
            hash: "old".into(),
            mutable: true,
        };
        let immutable = DeployedProgram {
            mutable: false,
            ..mutable.clone()
        };
        use DeployStep::*;
        assert_eq!(deploy_step(false, None, "new", false), Deploy);
        assert_eq!(
            deploy_step(false, Some(&mutable), "new", true),
            Deploy,
            "a new chain"
        );
        assert_eq!(deploy_step(true, Some(&mutable), "old", true), Unchanged);
        assert_eq!(deploy_step(true, Some(&mutable), "new", true), Update);
        assert_eq!(
            deploy_step(true, Some(&immutable), "new", false),
            NeedsReset
        );
        assert_eq!(deploy_step(true, Some(&immutable), "old", false), Unchanged);
        // Deployed some other way: nothing is known to compare.
        assert_eq!(deploy_step(true, None, "new", true), Unchanged);
    }

    #[test]
    fn checks_happen_against_the_settings_up_would_use() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::project::FILE_NAME),
            "[devnet]\nruntime = \"native\"\nport = 4040\n",
        )
        .unwrap();
        let project = Project::load(&dir.path().join(crate::project::FILE_NAME)).unwrap();
        let home = Home::locate(Some(dir.path().join("home"))).unwrap();
        let devnet = Devnet::open(home, Some(project)).unwrap();
        // Before the first `up`: nothing stored yet, the file decides.
        assert_eq!(devnet.settings.port, Settings::default().port);
        let wanted = devnet.settings_for_up();
        assert_eq!(wanted.runtime, RuntimeKind::Native);
        assert_eq!(wanted.port, 4040);
    }

    #[test]
    fn refuses_a_new_group_whose_variable_would_clash() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        let err = Devnet::open(home, None)
            .unwrap()
            .make_group("users-wallet", 1, 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("-wallet"), "{err}");
    }

    #[test]
    fn every_marked_group_wallet_forgets_even_when_one_fails() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        let mut groups = Vec::new();
        for (name, storage) in [
            ("admins", "not json"),
            ("users", crate::wallet_storage::tests::SYNCED_STORAGE),
        ] {
            let group = Group::new(&home.groups_dir(), name).unwrap();
            std::fs::create_dir_all(group.wallet_dir()).unwrap();
            std::fs::write(group.wallet_storage(), storage).unwrap();
            group.mark_new_chain().unwrap();
            groups.push(group);
        }
        let err = Devnet::open(home, None)
            .unwrap()
            .forget_group_chains()
            .unwrap_err();
        assert!(format!("{err:#}").contains("admins"), "{err:#}");
        let users: Value =
            serde_json::from_str(&std::fs::read_to_string(groups[1].wallet_storage()).unwrap())
                .unwrap();
        assert_eq!(
            users["last_synced_block"], 0,
            "tried after the one that failed"
        );
        // The failed one stays marked, for the next `up`.
        assert!(groups[0].forget_old_chain().is_err());
    }

    #[test]
    fn up_reports_what_it_deployed_and_updated() {
        let id = PublicId::parse(ID).unwrap();
        let mut status = Status {
            lez: "v0.3.1".into(),
            runtime: "docker".into(),
            sequencer: report::Sequencer {
                url: "http://127.0.0.1:3040".into(),
                running: true,
                runtime_answered: true,
                block: Some(1),
                max_txs_per_block: 20,
                block_time: "1s".into(),
                dev_mode: false,
            },
            faucet: None,
            programs: vec![],
            account_groups: vec![],
            sessions: vec![],
            messaging: None,
            storage: None,
            env_file: None,
        };
        status.programs = vec![
            program_report("counter", &id, DeployStep::Unchanged),
            program_report("shop", &id, DeployStep::Unchanged),
            program_report("vault", &id, DeployStep::Unchanged),
        ];
        let done = [
            program_report("counter", &id, DeployStep::Deploy),
            program_report("shop", &id, DeployStep::Update),
        ];
        let status = with_deploys(status, &done);
        let flags: Vec<_> = status
            .programs
            .iter()
            .map(|p| (p.name.as_str(), p.deployed_now, p.updated))
            .collect();
        assert_eq!(
            flags,
            [
                ("counter", true, false),
                ("shop", false, true),
                ("vault", false, false)
            ]
        );
    }

    #[test]
    fn reports_every_failure_not_only_the_first() {
        assert!(after_start([Ok(()), Ok(())]).is_ok());
        let one = after_start([Ok(()), Err(anyhow::anyhow!("sessions alice failed"))]);
        assert_eq!(
            one.unwrap_err().to_string(),
            "the devnet is up, but sessions alice failed"
        );
        let both = after_start([
            Err(anyhow::anyhow!("these failed: program shop")),
            Err(anyhow::anyhow!("sessions alice failed")),
        ])
        .unwrap_err()
        .to_string();
        assert_eq!(
            both,
            "the devnet is up, but:\n- these failed: program shop\n- sessions alice failed"
        );
    }

    #[test]
    fn a_chain_takes_only_settings_it_can_run_on() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        std::fs::create_dir_all(home.chain_dir()).unwrap();
        home.record_chain_dev_mode(true).unwrap();
        let devnet = Devnet::open(home, None).unwrap();
        let err = devnet
            .check_chain_takes(&Settings::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("runs in dev mode"), "{err}");
        let dev = Settings {
            dev_mode: true,
            ..Settings::default()
        };
        assert!(devnet.check_chain_takes(&dev).is_ok());
    }

    #[test]
    fn says_why_a_program_needs_reset() {
        let elf = Path::new("out/counter.bin");
        let rebuilt = needs_reset("counter", elf, false, false);
        assert!(rebuilt.contains("out/counter.bin changed"), "{rebuilt}");
        assert!(rebuilt.contains("old build still runs"), "{rebuilt}");
        let flipped = needs_reset("counter", elf, true, false);
        assert!(
            flipped.contains("deployed mutable, asked for immutable"),
            "{flipped}"
        );
        assert!(flipped.contains("`logos-devnet reset` deploys it immutable"));
    }

    #[test]
    fn a_deploy_never_changes_a_programs_mutability() {
        use DeployStep::*;
        let mutable = DeployedProgram {
            hash: "old".into(),
            mutable: true,
        };
        let immutable = DeployedProgram {
            mutable: false,
            ..mutable.clone()
        };
        // A missing --mutable, or `mutable = true` dropped: it would freeze it for good.
        assert_eq!(deploy_step(true, Some(&mutable), "new", false), NeedsReset);
        assert_eq!(deploy_step(true, Some(&mutable), "old", false), NeedsReset);
        assert_eq!(deploy_step(true, Some(&immutable), "old", true), NeedsReset);
        assert_eq!(deploy_step(true, Some(&immutable), "new", true), NeedsReset);
        // On a new chain it's deployed as asked.
        assert_eq!(deploy_step(false, Some(&mutable), "old", false), Deploy);
    }

    #[test]
    fn the_wallet_reports_a_deploy_and_an_update() {
        let id = "6YvesSEDWW25kpi8SSki2c8o5m3uACsigvGnNQ6wMruh";
        let deployed =
            format!("Segment uploaded at x\nProgram deployed. Header account id: {id}\n");
        let updated = format!("Segment uploaded at x\nProgram {id} updated\n");
        assert_eq!(DeployStep::Deploy.reported_header(&deployed), Some(id));
        assert_eq!(DeployStep::Update.reported_header(&updated), Some(id));
        assert_eq!(DeployStep::Update.reported_header(&deployed), None);
        assert_eq!(DeployStep::Unchanged.loader_command(), None);
    }

    #[test]
    fn a_shared_devnet_names_reset_for_what_only_a_new_chain_changes() {
        let dir = tempfile::tempdir().unwrap();
        let ask = |devnet: &str| {
            std::fs::write(
                dir.path().join(crate::project::FILE_NAME),
                format!("[devnet]\n{devnet}\n"),
            )
            .unwrap();
            let project = Project::load(&dir.path().join(crate::project::FILE_NAME)).unwrap();
            let home = Home::locate(Some(dir.path().join("home"))).unwrap();
            Devnet::open(home, Some(project))
                .unwrap()
                .with_project_settings(&UpOptions::default(), true)
                .unwrap_err()
                .to_string()
        };
        assert!(ask("block_time = \"500ms\"").contains("`logos-devnet down` and `up`"));
        assert!(ask("dev_mode = true").contains("`logos-devnet reset`"));
        assert!(ask("lez = \"v0.3.2\"").contains("`logos-devnet reset`"));
    }

    #[test]
    fn dev_mode_is_a_flag_with_an_optional_value() {
        #[derive(clap::Parser)]
        struct Up {
            #[command(flatten)]
            options: UpOptions,
        }
        let parse = |args: &[&str]| {
            <Up as clap::Parser>::try_parse_from(std::iter::once("up").chain(args.iter().copied()))
                .unwrap()
                .options
        };
        assert_eq!(parse(&[]).dev_mode, None);
        assert_eq!(parse(&["--dev-mode"]).dev_mode, Some(true));
        assert_eq!(parse(&["--dev-mode", "false"]).dev_mode, Some(false));
        assert_eq!(
            parse(&["--dev-mode", "--port", "4000"]).dev_mode,
            Some(true)
        );
        assert!(parse(&["--dev-mode"]).apply(Settings::default()).dev_mode);
        let on = Settings {
            dev_mode: true,
            ..Settings::default()
        };
        assert!(!parse(&["--dev-mode", "false"]).apply(on.clone()).dev_mode);
        assert!(parse(&[]).apply(on).dev_mode, "unset keeps it");
        let debug = parse(&["--log-filter", "debug"]).apply(Settings::default());
        assert_eq!(debug.log_filter, "debug");
        assert_eq!(
            parse(&[]).apply(debug).log_filter,
            "debug",
            "remembered, like the others"
        );
    }

    #[test]
    fn a_chain_keeps_its_lez_tag_and_dev_mode() {
        let chain = Settings::default();
        assert!(check_chain_keeps(&chain, &chain).is_ok());
        let faster = Settings {
            block_time: "500ms".into(),
            ..chain.clone()
        };
        assert!(check_chain_keeps(&chain, &faster).is_ok());
        let dev = Settings {
            dev_mode: true,
            ..chain.clone()
        };
        let err = check_chain_keeps(&chain, &dev).unwrap_err().to_string();
        assert!(err.contains("reset --dev-mode"), "{err}");
        let err = check_chain_keeps(&dev, &chain).unwrap_err().to_string();
        assert!(err.contains("reset --dev-mode false"), "{err}");
        let other = Settings {
            lez: "v0.3.2".into(),
            ..chain.clone()
        };
        let err = check_chain_keeps(&chain, &other).unwrap_err().to_string();
        assert!(err.contains("reset --lez v0.3.2"), "{err}");
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
        let err = devnet.deploy("counter", &elf, false).unwrap_err();
        assert!(err.to_string().contains("logos-devnet up"), "{err}");
        assert!(
            devnet.deploy("Counter", &elf, false).is_err(),
            "bad names are refused first"
        );
    }

    #[test]
    fn up_refuses_a_lez_tag_outside_v0_3_before_saving_anything() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        let opts = UpOptions {
            lez: Some("v0.4.0-rc3".into()),
            ..UpOptions::default()
        };
        let err = Devnet::open(home.clone(), None)
            .unwrap()
            .up(&opts)
            .unwrap_err();
        assert!(err.to_string().contains("--lez v0.3.1"), "{err}");
        assert!(!home.settings_file().exists(), "nothing saved");
    }

    #[test]
    fn up_refuses_another_lez_tag_on_an_existing_chain() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        std::fs::create_dir_all(home.chain_dir().join("rocksdb-0101")).unwrap();
        let opts = UpOptions {
            lez: Some("v0.3.9".into()),
            ..UpOptions::default()
        };
        let err = Devnet::open(home.clone(), None)
            .unwrap()
            .up(&opts)
            .unwrap_err();
        assert!(err.to_string().contains("reset --lez v0.3.9"), "{err}");
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
