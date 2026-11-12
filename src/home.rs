//! The devnet's state directory (`~/.logos-devnet`, or `LOGOS_DEVNET_HOME`) and the settings
//! kept in it.
//!
//! ```text
//! settings.json   LEZ tag, runtime, port, block time, faucet id
//! env             export lines for projects (see envfile)
//! chain/          the sequencer's home: its config, database and keys. `reset` deletes it.
//! wallet/         the dev wallet: the faucet and each program's header key. Kept across resets.
//! compose.yaml    the Docker runtime's compose file
//! native/         the native runtime's checkout, build, log and pid file
//! sessions/       throwaway logosctl sessions with devnet wallets (see session)
//! accounts/       account groups: many funded keys for tests (see groups)
//! programs.json   the binary hash each program was deployed from
//! downloads/      program binaries the project file names by URL
//! messaging/      the local messaging hub, when a project asks for one (see messaging)
//! lock            held by the command changing the home, so two take turns
//! ```

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::account::PublicId;

/// The LEZ release `lez_core` 0.5.1 is built on, which the public testnet runs. Bump with the
/// testnet; the image workflow builds this tag when it changes.
pub const DEFAULT_LEZ: &str = "v0.3.1";
const IMAGE_REPO: &str = "ghcr.io/drptbl/logos-devnet-sequencer";
const DEFAULT_PORT: u16 = 3040;
const DEFAULT_BLOCK_TIME: &str = "1s";
/// Upstream's debug config allows 20 transactions per block.
pub const DEFAULT_MAX_TXS_PER_BLOCK: u32 = 20;
/// The sequencer's log filter (`RUST_LOG`): its own progress, without its actor framework's.
pub const DEFAULT_LOG_FILTER: &str = "info,kameo=warn";

/// A LEZ tag logos-devnet runs: one of the v0.3 line, e.g. `v0.3.1`. LEZ v0.4 changes the
/// sequencer's config and commands, the wallet's format and how accounts are funded.
pub fn check_lez(tag: &str) -> Result<()> {
    let patch = tag.strip_prefix("v0.3.").unwrap_or_default();
    let numbered = patch
        .split(['-', '.'])
        .next()
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !numbered {
        anyhow::bail!(
            "logos-devnet runs LEZ v0.3 tags, and {tag} isn't one: use `--lez {DEFAULT_LEZ}` or \
             another v0.3 tag. LEZ v0.4 changes the sequencer and wallet, and isn't supported yet"
        );
    }
    Ok(())
}

/// A log filter the sequencer takes: `RUST_LOG` syntax, e.g. `debug` or
/// `info,sequencer_core=debug`.
pub fn check_log_filter(filter: &str) -> Result<()> {
    if filter.trim().is_empty() {
        anyhow::bail!("the log filter can't be empty: `info` is the least");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    Docker,
    Native,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub lez: String,
    pub runtime: RuntimeKind,
    pub port: u16,
    pub block_time: String,
    /// The most user transactions a block takes. Added after v0.1.0, so older settings files
    /// load with the default.
    #[serde(default = "default_max_txs_per_block")]
    pub max_txs_per_block: u32,
    /// Whether the chain skips proof checks (`RISC0_DEV_MODE`), so private calls take a second
    /// instead of minutes. A chain keeps it: one with fake proofs crashes without it.
    #[serde(default)]
    pub dev_mode: bool,
    /// The sequencer's `RUST_LOG`. Added in v0.3.2.
    #[serde(default = "default_log_filter")]
    pub log_filter: String,
    /// The dev wallet's faucet account, funded at genesis. Created on the first `up`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faucet: Option<PublicId>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lez: DEFAULT_LEZ.into(),
            runtime: RuntimeKind::Docker,
            port: DEFAULT_PORT,
            block_time: DEFAULT_BLOCK_TIME.into(),
            max_txs_per_block: DEFAULT_MAX_TXS_PER_BLOCK,
            dev_mode: false,
            log_filter: DEFAULT_LOG_FILTER.into(),
            faucet: None,
        }
    }
}

fn default_max_txs_per_block() -> u32 {
    DEFAULT_MAX_TXS_PER_BLOCK
}

fn default_log_filter() -> String {
    DEFAULT_LOG_FILTER.into()
}

impl Settings {
    pub fn image(&self) -> String {
        format!("{IMAGE_REPO}:{}", self.lez)
    }

    /// The RPC URL on this machine.
    pub fn sequencer_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// `explicit` (from `--home` or `LOGOS_DEVNET_HOME`) if given, else `~/.logos-devnet`,
    /// resolved to one real path: the sequencer and Docker read it from other directories, and
    /// the runtimes tell their processes and containers apart by it, so two spellings of one
    /// directory must be one home.
    pub fn locate(explicit: Option<PathBuf>) -> Result<Self> {
        let root = match explicit {
            Some(path) => resolve(&path)?,
            None => default_root()?,
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Holds the home for one command that changes it, until dropped. A second such command,
    /// e.g. from another project sharing the home, waits, so starting, wiping and making
    /// accounts never interleave.
    pub fn lock(&self) -> Result<std::fs::File> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.root.join("lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => eprintln!(
                "Waiting for another logos-devnet command using {}...",
                self.root.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    pub fn is_default(&self) -> bool {
        default_root().is_ok_and(|d| d == self.root)
    }

    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }
    pub fn env_file(&self) -> PathBuf {
        self.root.join("env")
    }
    pub fn chain_dir(&self) -> PathBuf {
        self.root.join("chain")
    }
    pub fn chain_config(&self) -> PathBuf {
        self.chain_dir().join("sequencer_config.json")
    }
    /// The chain's own record of its dev mode, which `reset` deletes with it.
    fn chain_dev_mode_file(&self) -> PathBuf {
        self.chain_dir().join("dev-mode")
    }

    /// Whether the chain here was started in dev mode. Kept with the chain rather than in
    /// `settings.json`, which can be lost or edited, since a chain with fake proofs crashes
    /// without it. Chains from before v0.3.0 have no record, and check proofs.
    pub fn chain_dev_mode(&self) -> bool {
        std::fs::read_to_string(self.chain_dev_mode_file()).is_ok_and(|text| text.trim() == "1")
    }

    pub fn record_chain_dev_mode(&self, dev_mode: bool) -> Result<()> {
        let file = self.chain_dev_mode_file();
        let value = crate::envfile::dev_mode_value(dev_mode);
        std::fs::write(&file, value).with_context(|| format!("writing {}", file.display()))
    }
    pub fn wallet_dir(&self) -> PathBuf {
        self.root.join("wallet")
    }
    pub fn compose_file(&self) -> PathBuf {
        self.root.join("compose.yaml")
    }
    pub fn native_dir(&self) -> PathBuf {
        self.root.join("native")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }
    pub fn groups_dir(&self) -> PathBuf {
        self.root.join("accounts")
    }
    /// Program binaries downloaded from a URL, named by their SHA-256.
    pub fn downloads_dir(&self) -> PathBuf {
        self.root.join("downloads")
    }
    /// The local messaging hub's state and daemon.
    pub fn messaging_dir(&self) -> PathBuf {
        self.root.join("messaging")
    }
    /// Each deployed program's binary hash, to notice a changed build.
    pub fn programs_file(&self) -> PathBuf {
        self.root.join("programs.json")
    }

    /// Whether a chain has been started here: the sequencer's database exists.
    pub fn has_chain(&self) -> bool {
        std::fs::read_dir(self.chain_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("rocksdb"))
            })
            .unwrap_or(false)
    }

    /// The stored settings, or the defaults on a fresh home.
    pub fn load_settings(&self) -> Result<Settings> {
        let path = self.settings_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        write_json(&self.settings_file(), &serde_json::to_value(settings)?)
    }
}

fn default_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    resolve(&Path::new(&home).join(".logos-devnet"))
}

/// `path` made absolute with symlinks resolved, without creating anything: the deepest part
/// that exists is canonicalized and the rest appended.
fn resolve(path: &Path) -> Result<PathBuf> {
    let path = normalize(&std::path::absolute(path)?);
    let mut existing = path.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(mut real) = existing.canonicalize() {
            real.extend(missing.iter().rev());
            return Ok(real);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_owned());
                existing = parent;
            }
            _ => return Ok(path),
        }
    }
}

/// Drops `.` and folds `..` without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Deletes everything in `dir` but keeps the directory itself.
pub fn empty_dir(dir: &Path) -> Result<()> {
    let Some(entries) = read_dir_if_present(dir)? else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        let removed = if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.with_context(|| format!("deleting {}", path.display()))?;
    }
    Ok(())
}

/// `dir`'s entries, or `None` when it doesn't exist.
pub fn read_dir_if_present(dir: &Path) -> Result<Option<std::fs::ReadDir>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", dir.display())),
    }
}

/// Writes `value` as pretty JSON with a trailing newline.
pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)? + "\n";
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_only_lez_v0_3_tags() {
        for tag in ["v0.3.1", "v0.3.2", "v0.3.10", "v0.3.2-rc1"] {
            assert!(check_lez(tag).is_ok(), "{tag}");
        }
        for tag in [
            "v0.4.0-rc3",
            "v0.4.0",
            "v0.3",
            "v0.3.",
            "v0.3.x",
            "0.3.1",
            "main",
        ] {
            assert!(check_lez(tag).is_err(), "{tag}");
        }
        assert!(check_lez(DEFAULT_LEZ).is_ok());
    }

    #[test]
    fn refuses_an_empty_log_filter() {
        assert!(check_log_filter("debug").is_ok());
        assert!(check_log_filter("info,sequencer_core=debug").is_ok());
        assert!(check_log_filter(" ").is_err());
    }

    #[test]
    fn a_chain_remembers_its_own_dev_mode() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        assert!(!home.chain_dev_mode(), "chains before v0.3.0 check proofs");
        std::fs::create_dir_all(home.chain_dir()).unwrap();
        home.record_chain_dev_mode(true).unwrap();
        assert!(home.chain_dev_mode());
        // Not settings.json, which can be lost or edited.
        std::fs::remove_file(home.settings_file()).ok();
        assert!(home.chain_dev_mode());
        home.record_chain_dev_mode(false).unwrap();
        assert!(!home.chain_dev_mode());
    }

    #[test]
    fn one_command_at_a_time_holds_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        let held = home.lock().unwrap();
        let other = std::fs::File::open(home.root().join("lock")).unwrap();
        assert!(matches!(
            other.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(held);
        // A process another test thread forks in this moment holds a copy of the lock until it
        // runs its program, so it can outlast the drop by a moment.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while other.try_lock().is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the lock was never released"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn a_fresh_home_has_default_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        assert_eq!(home.load_settings().unwrap(), Settings::default());
        assert!(!home.has_chain());
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().join("h"))).unwrap();
        let settings = Settings {
            runtime: RuntimeKind::Native,
            port: 3999,
            faucet: Some(PublicId::parse("HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d").unwrap()),
            ..Settings::default()
        };
        home.save_settings(&settings).unwrap();
        assert_eq!(home.load_settings().unwrap(), settings);
    }

    #[test]
    fn loads_a_v0_1_0_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        // As v0.1.0 wrote it.
        let text = r#"{"lez":"v0.3.1","runtime":"docker","port":3040,"block_time":"1s","faucet":"HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d"}"#;
        std::fs::write(home.settings_file(), text).unwrap();
        let settings = home.load_settings().unwrap();
        assert_eq!(settings.max_txs_per_block, DEFAULT_MAX_TXS_PER_BLOCK);
        assert!(!settings.dev_mode, "chains before v0.3.0 prove for real");
        assert_eq!(settings.log_filter, DEFAULT_LOG_FILTER);
        assert!(settings.faucet.is_some());
    }

    #[test]
    fn refuses_a_malformed_faucet_in_settings() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        let text =
            r#"{"lez":"v0.3.1","runtime":"docker","port":3040,"block_time":"1s","faucet":"nope"}"#;
        std::fs::write(home.settings_file(), text).unwrap();
        assert!(home.load_settings().is_err());
    }

    #[test]
    fn a_chain_exists_once_the_database_does() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        std::fs::create_dir_all(home.chain_dir()).unwrap();
        std::fs::write(home.chain_config(), "{}").unwrap();
        assert!(!home.has_chain(), "a config alone is not a chain");
        std::fs::create_dir(home.chain_dir().join("rocksdb-0101")).unwrap();
        assert!(home.has_chain());
    }

    #[test]
    fn makes_a_relative_home_absolute() {
        let home = Home::locate(Some("rel/./x/../devnet".into())).unwrap();
        assert!(home.root().is_absolute());
        assert!(home.root().ends_with("rel/devnet"));
        assert!(home.chain_config().is_absolute());
    }

    #[test]
    fn a_symlinked_home_is_the_same_home() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let via_real = Home::locate(Some(real.join("devnet"))).unwrap();
        let via_link = Home::locate(Some(link.join("devnet"))).unwrap();
        assert_eq!(via_real.root(), via_link.root());
        assert!(!via_real.root().exists(), "locating creates nothing");
    }

    #[test]
    fn emptying_a_directory_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let chain = dir.path().join("chain");
        std::fs::create_dir_all(chain.join("rocksdb-0101/sub")).unwrap();
        std::fs::write(chain.join("bedrock_signing_key"), [0u8; 32]).unwrap();
        empty_dir(&chain).unwrap();
        assert!(chain.is_dir());
        assert_eq!(std::fs::read_dir(&chain).unwrap().count(), 0);
        empty_dir(&dir.path().join("missing")).unwrap();
    }

    #[test]
    fn names_the_image_by_lez_tag() {
        assert_eq!(
            Settings::default().image(),
            "ghcr.io/drptbl/logos-devnet-sequencer:v0.3.1"
        );
        assert_eq!(Settings::default().sequencer_url(), "http://127.0.0.1:3040");
    }
}
