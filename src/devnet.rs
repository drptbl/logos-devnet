//! The commands: start a chain, fund accounts, deploy programs, and write what projects need.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::account::{PublicId, first_public_id};
use crate::chain_config::{self, ChainOptions};
use crate::envfile::{self, Env};
use crate::home::{Home, RuntimeKind, Settings, write_json};
use crate::rpc::Rpc;
use crate::runtime::{Docker, Native, Runtime, WalletArg};

/// The faucet's genesis balance in lepta: enough that a devnet never runs dry.
const FAUCET_BALANCE: u64 = 1_000_000_000_000_000_000;
/// The dev wallet's label for the faucet account.
const FAUCET_LABEL: &str = "faucet";
/// What `fund` sends when no amount is given.
pub const DEFAULT_FUND: u128 = 10_000_000_000;
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
    /// Build the image or binaries even if they exist. Not remembered.
    #[arg(long)]
    pub rebuild: bool,
}

impl UpOptions {
    fn apply(&self, mut settings: Settings) -> Settings {
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
        settings
    }
}

pub struct Devnet {
    home: Home,
    settings: Settings,
    runtime: Box<dyn Runtime>,
}

impl Devnet {
    pub fn open(home: Home) -> Result<Self> {
        let settings = home.load_settings()?;
        Ok(Self::with(home, settings))
    }

    fn with(home: Home, settings: Settings) -> Self {
        let runtime: Box<dyn Runtime> = match settings.runtime {
            RuntimeKind::Docker => Box::new(Docker::new(home.clone(), settings.clone())),
            RuntimeKind::Native => Box::new(Native::new(home.clone(), settings.clone())),
        };
        Self {
            home,
            settings,
            runtime,
        }
    }

    fn rpc(&self) -> Rpc {
        Rpc::new(self.settings.sequencer_url())
    }

    /// Runs the LEZ wallet on the dev wallet. Its config is rewritten first: the wallet would
    /// otherwise create a default one pointing at the public testnet.
    fn wallet(&self, args: Vec<WalletArg>) -> Result<String> {
        std::fs::create_dir_all(self.home.wallet_dir())?;
        write_json(
            &self.home.wallet_dir().join("wallet_config.json"),
            &wallet_config(&self.runtime.wallet_sequencer_url()),
        )?;
        self.runtime.wallet(args)
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

    pub fn up(self, opts: &UpOptions) -> Result<()> {
        self.launch(opts, false)
    }

    /// Deletes the chain and starts a new one. Keys in the dev wallet stay, so the faucet and
    /// program ids come back the same.
    pub fn reset(self, opts: &UpOptions) -> Result<()> {
        self.launch(opts, true)
    }

    /// Starts the devnet with `opts` applied, on a new chain when `wipe`. Everything that can
    /// fail runs before the running sequencer is stopped, the settings saved or the chain
    /// deleted, so a bad option leaves the devnet as it was.
    fn launch(self, opts: &UpOptions, wipe: bool) -> Result<()> {
        let settings = opts.apply(self.settings.clone());
        chain_config::block_time_secs(&settings.block_time)?;
        let continuing = !wipe && self.home.has_chain();
        if continuing && settings.lez != self.settings.lez {
            bail!(
                "this chain runs LEZ {}. `logos-devnet reset --lez {}` starts a new chain on it",
                self.settings.lez,
                settings.lez
            );
        }
        // Nothing to do, unless the settings file is gone and needs writing again.
        let unchanged = settings == self.settings && self.home.settings_file().exists();
        if !wipe && unchanged && !opts.rebuild && self.runtime.running() {
            println!("Already running.");
            return self.status();
        }
        // The running sequencer holds its own port, and gives it up when it stops.
        if !(self.runtime.running() && settings.port == self.settings.port) {
            port_free(settings.port)?;
        }

        let mut next = Self::with(self.home.clone(), settings);
        next.runtime.prepare(opts.rebuild)?;
        let faucet = next.dev_faucet()?;
        if continuing && funds_another_faucet(self.settings.faucet.as_ref(), &faucet) {
            bail!(
                "this chain's genesis funds another faucet than the dev wallet holds. \
                 `logos-devnet reset` starts a chain that funds it"
            );
        }
        next.settings.faucet = Some(faucet);
        let config = next.chain_config()?;

        self.runtime.stop()?;
        if wipe {
            remove_if_present(&self.home.chain_dir(), |p| std::fs::remove_dir_all(p))
                .context("deleting the chain")?;
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
        next.status()
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
        chain_config::patch(
            self.runtime.upstream_config()?,
            &ChainOptions {
                home: &self.runtime.chain_home(),
                block_time: &self.settings.block_time,
                faucet: self.faucet()?,
                faucet_balance: FAUCET_BALANCE,
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

    fn write_env(&self, env: &Env) -> Result<()> {
        std::fs::write(self.home.env_file(), env.render()).context("writing the env file")
    }

    fn read_env(&self) -> Result<Env> {
        let text = std::fs::read_to_string(self.home.env_file())
            .context("no env file: run `logos-devnet up` first")?;
        Env::parse(&text)
    }

    pub fn down(&self) -> Result<()> {
        self.runtime.stop()?;
        println!("Stopped. The chain is kept; `logos-devnet up` resumes it.");
        Ok(())
    }

    pub fn status(&self) -> Result<()> {
        let s = &self.settings;
        println!("lez        {} ({})", s.lez, self.runtime.describe());
        let running = self.runtime.running();
        let head = running.then(|| self.rpc().last_block_id().ok()).flatten();
        match head {
            Some(head) => println!("sequencer  {}  running, block {head}", s.sequencer_url()),
            None if running => println!("sequencer  {}  starting", s.sequencer_url()),
            None => println!("sequencer  {}  stopped", s.sequencer_url()),
        }
        if let Some(faucet) = &s.faucet {
            let balance = head.and_then(|_| self.rpc().balance(faucet).ok());
            let balance = balance.map_or(String::new(), |b| format!("  {b} lepta"));
            println!("faucet     {}{balance}", faucet.wallet_form());
        }
        if let Ok(env) = self.read_env() {
            for (name, id) in &env.programs {
                println!(
                    "program    {name}  {}  ({})",
                    id.as_str(),
                    envfile::program_var(name)
                );
            }
            println!("env        {}", self.home.env_file().display());
        }
        Ok(())
    }

    pub fn fund(&self, account: &str, amount: u128) -> Result<()> {
        let to = PublicId::parse(account)?;
        let faucet = self.faucet()?;
        self.require_running()?;
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
        let balance = self.rpc().balance(&to)?;
        println!("{}  {balance} lepta", to.wallet_form());
        Ok(())
    }

    /// Deploys `elf` as program `name` from the dev wallet's header key for that name, so the
    /// program id is the same on every chain this home starts.
    pub fn deploy(&self, name: &str, elf: &Path) -> Result<()> {
        envfile::check_program_name(name)?;
        let size = std::fs::metadata(elf)
            .with_context(|| format!("{} does not exist", elf.display()))?
            .len();
        let faucet = self.faucet()?;
        self.require_running()?;
        let mut env = self.read_env()?;

        let label = format!("program-{name}");
        let header = match self.labelled_account(&label)? {
            Some(header) => header,
            None => self.new_account(Some(&label))?,
        };
        if self.rpc().account_in_use(&header)? {
            println!(
                "{name} is already deployed on this chain. `logos-devnet reset` to deploy a new build."
            );
        } else {
            let mut args: Vec<WalletArg> = vec![
                "program-loader".into(),
                "deploy".into(),
                "--elf".into(),
                WalletArg::File(elf.to_path_buf()),
                "--header".into(),
                header.wallet_form().into(),
                "--segments".into(),
            ];
            for _ in 0..segment_count(size) {
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
        }
        env.programs.insert(name.to_owned(), header.clone());
        self.write_env(&env)?;
        println!("{}={}", envfile::program_var(name), header.as_str());
        Ok(())
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

    pub fn print_env(&self) -> Result<()> {
        print!("{}", self.read_env()?.render());
        Ok(())
    }

    pub fn logs(&self, follow: bool) -> Result<()> {
        self.runtime.logs(follow)
    }

    /// Writes `<dir>/wallet_config.json` for a project's own wallet (`lez_core` or the LEZ
    /// CLI). Refuses a directory that already holds a wallet or a config: repointing a testnet
    /// wallet at the devnet would mix up its sync state.
    pub fn write_project_wallet_config(&self, dir: &Path) -> Result<PathBuf> {
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
        Ok(path)
    }
}

/// The env file after `up`: a chain that carries on keeps its programs, a new one has none.
fn port_free(port: u16) -> Result<()> {
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
        Devnet::open(Home::locate(Some(dir.into())).unwrap()).unwrap()
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
        let path = devnet.write_project_wallet_config(&target).unwrap();
        let config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
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
        let err = Devnet::open(home.clone()).unwrap().up(&opts).unwrap_err();
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
        let err = Devnet::open(home.clone())
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
            assert!(command(Devnet::open(home.clone()).unwrap(), &opts).is_err());
            assert!(!home.settings_file().exists(), "nothing saved");
            assert!(home.has_chain(), "nothing deleted");
        }
    }
}
