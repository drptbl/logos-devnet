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
//! downloads/      program binaries the project file names by URL, and module packages. Every
//!                 home uses the one in `~/.logos-devnet` (see `with_shared_downloads`)
//! messaging/      the local messaging hub, when a project asks for one (see messaging)
//! lock            held by the command changing the home, so two take turns
//! ```

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::account::PublicId;

/// The LEZ release the devnet runs unless asked for another v0.4 tag. Bump with LEZ's releases;
/// the image workflow builds this tag when it changes.
pub const DEFAULT_LEZ: &str = "v0.4.0-rc3";
const IMAGE_REPO: &str = "ghcr.io/drptbl/logos-devnet-sequencer";
const DEFAULT_PORT: u16 = 3040;
const DEFAULT_BLOCK_TIME: &str = "1s";
/// Upstream's local config allows 20 transactions per block.
pub const DEFAULT_MAX_TXS_PER_BLOCK: u32 = 20;
/// The sequencer's log filter (`RUST_LOG`): its own progress, without its actor framework's.
pub const DEFAULT_LOG_FILTER: &str = "info,kameo=warn";

/// A LEZ tag logos-devnet runs: one of the v0.4 line, e.g. `v0.4.0-rc3`.
pub fn check_lez(tag: &str) -> Result<()> {
    if is_v0_3(tag) {
        anyhow::bail!("{tag} is a LEZ v0.3 tag. {V0_3_RELEASE}");
    }
    if !numbered(tag, "v0.4.") {
        anyhow::bail!(
            "logos-devnet runs LEZ v0.4 tags, and {tag} isn't one: use `--lez {DEFAULT_LEZ}`"
        );
    }
    Ok(())
}

/// Where LEZ v0.3 lives on.
pub const V0_3_RELEASE: &str = "logos-devnet runs LEZ v0.4 since v0.10.0, and v0.9.4 is the last \
     release that runs v0.3: `cargo install --locked logos-devnet --version 0.9.4`, or \
     https://github.com/drptbl/logos-devnet/releases/tag/v0.9.4";

/// Whether `tag` is a LEZ v0.3 tag, which homes from before v0.10.0 ran.
pub fn is_v0_3(tag: &str) -> bool {
    numbered(tag, "v0.3.")
}

/// Whether `tag` is `prefix` and a patch number, with any suffix, e.g. `v0.4.` and `0-rc3`.
fn numbered(tag: &str, prefix: &str) -> bool {
    tag.strip_prefix(prefix)
        .and_then(|patch| patch.split(['-', '.']).next())
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
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
    downloads: PathBuf,
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
        let downloads = root.join("downloads");
        Ok(Self { root, downloads })
    }

    /// This home, downloading into `~/.logos-devnet/downloads` like every other home, so a
    /// project's own home or a worktree's doesn't download the same files again. Each file is
    /// checked against its SHA-256 before use, whichever home put it there. Without `HOME`, or
    /// when that directory can't be made, the home keeps its own.
    pub fn with_shared_downloads(self) -> Self {
        let shared = default_root().map(|root| root.join("downloads"));
        match shared {
            Ok(dir) if std::fs::create_dir_all(&dir).is_ok() => self.with_downloads(dir),
            _ => self,
        }
    }

    fn with_downloads(self, downloads: PathBuf) -> Self {
        Self { downloads, ..self }
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
    /// The config a new LEZ v0.4 chain is set up from; `setup` copies it to `chain_config`.
    pub fn setup_template(&self) -> PathBuf {
        self.chain_dir().join("setup-template.json")
    }
    /// Whether a LEZ v0.4 chain here has been set up. `setup` writes its channel signing key,
    /// then this file, then the config, which `up` writes again anyway.
    pub fn chain_set_up(&self) -> bool {
        self.chain_dir().join("bedrock_funding_public_key").exists()
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

    /// The LEZ tag the chain here runs, kept with the chain like its dev mode: a lost or edited
    /// settings file would read as the default tag, and v0.3 and v0.4 can't run each other's
    /// chain. Chains from before v0.10.0 have none.
    pub fn chain_lez(&self) -> Option<String> {
        std::fs::read_to_string(self.chain_dir().join("lez"))
            .ok()
            .map(|tag| tag.trim().to_owned())
            .filter(|tag| !tag.is_empty())
    }

    pub fn record_chain_lez(&self, tag: &str) -> Result<()> {
        let file = self.chain_dir().join("lez");
        std::fs::write(&file, tag).with_context(|| format!("writing {}", file.display()))
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

    /// Whether the chain here ran LEZ v0.3: its own record says so, or it has none and either
    /// the settings (`settings_lez`) say so or it was never set up the way v0.4 chains are.
    pub fn runs_v0_3(&self, settings_lez: &str) -> bool {
        match self.chain_lez() {
            Some(tag) => is_v0_3(&tag),
            None => is_v0_3(settings_lez) || (self.has_chain() && !self.chain_set_up()),
        }
    }

    /// Moves a LEZ v0.3 home's wallets aside, to `<dir>-v0.3/`, so a v0.4 chain starts its own:
    /// a v0.4 wallet can't read v0.3 storage. Nothing is deleted or overwritten, and a home moved
    /// once isn't moved again, so a `reset` that fails later can run again. Returns the
    /// directories moved.
    pub fn move_v0_3_aside(&self) -> Result<Vec<String>> {
        let done = self.root.join("moved-v0.3");
        if done.exists() {
            return Ok(Vec::new());
        }
        let dirs = ["wallet", "accounts", "sessions"]
            .map(|dir| (self.root.join(dir), self.root.join(format!("{dir}-v0.3"))));
        for (from, to) in &dirs {
            if from.exists() && to.exists() {
                anyhow::bail!(
                    "{} and {} both exist: move one of them elsewhere, then `reset` again",
                    from.display(),
                    to.display()
                );
            }
        }
        let mut moved = Vec::new();
        for (from, to) in &dirs {
            if from.exists() {
                std::fs::rename(from, to)
                    .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
                moved.push(
                    to.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        std::fs::write(&done, "").with_context(|| format!("writing {}", done.display()))?;
        Ok(moved)
    }
    pub fn groups_dir(&self) -> PathBuf {
        self.root.join("accounts")
    }
    /// Program binaries downloaded from a URL, named by their SHA-256.
    pub fn downloads_dir(&self) -> PathBuf {
        self.downloads.clone()
    }
    /// Module packages from the catalog, `<name>-<version>.lgx`, downloaded once for every
    /// daemon.
    pub fn packages_dir(&self) -> PathBuf {
        self.downloads_dir().join("packages")
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
    fn runs_lez_v0_4_tags_and_points_v0_3_at_its_last_release() {
        for tag in ["v0.4.0-rc3", "v0.4.0", "v0.4.12"] {
            assert!(check_lez(tag).is_ok(), "{tag}");
        }
        for tag in ["v0.3.1", "v0.3.2-rc1"] {
            let err = check_lez(tag).unwrap_err().to_string();
            assert!(err.contains("v0.9.4"), "{err}");
        }
        for tag in ["v0.5.0", "v0.4", "v0.4.", "v0.4.x", "0.4.0", "main"] {
            assert!(check_lez(tag).is_err(), "{tag}");
        }
        assert!(check_lez(DEFAULT_LEZ).is_ok());
    }

    #[test]
    fn a_v0_4_chain_is_set_up_once_setup_wrote_its_funding_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        std::fs::create_dir_all(home.chain_dir()).unwrap();
        // `setup` writes the channel key first: stopped there, it must run again.
        std::fs::write(home.chain_dir().join("channel_signing_key"), "k").unwrap();
        assert!(!home.chain_set_up());
        std::fs::write(home.chain_dir().join("bedrock_funding_public_key"), "f").unwrap();
        assert!(home.chain_set_up());
    }

    #[test]
    fn moves_a_v0_3_homes_wallets_aside_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        for d in ["wallet", "accounts", "sessions"] {
            std::fs::create_dir_all(home.root().join(d)).unwrap();
            std::fs::write(home.root().join(d).join("data"), "v0.3").unwrap();
        }
        assert_eq!(
            home.move_v0_3_aside().unwrap(),
            ["wallet-v0.3", "accounts-v0.3", "sessions-v0.3"]
        );
        let kept = home.root().join("accounts-v0.3/data");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "v0.3");
        // A `reset` that failed after the move runs again: v0.4's own groups stay where they are.
        std::fs::create_dir_all(home.groups_dir()).unwrap();
        assert!(home.move_v0_3_aside().unwrap().is_empty());
        assert!(home.groups_dir().exists());
        assert_eq!(std::fs::read_to_string(kept).unwrap(), "v0.3");
    }

    #[test]
    fn refuses_to_move_over_an_earlier_move() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        for d in ["wallet", "wallet-v0.3"] {
            std::fs::create_dir_all(home.root().join(d)).unwrap();
        }
        let err = home.move_v0_3_aside().unwrap_err().to_string();
        assert!(err.contains("both exist"), "{err}");
        assert!(home.wallet_dir().exists());
    }

    #[test]
    fn tells_a_v0_3_chain_from_a_v0_4_one() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().to_path_buf())).unwrap();
        assert!(!home.runs_v0_3(DEFAULT_LEZ));
        assert!(home.runs_v0_3("v0.3.1"), "the settings say so");
        // A chain with neither a record nor v0.4's setup, its settings lost.
        std::fs::create_dir_all(home.chain_dir().join("rocksdb-0101")).unwrap();
        assert!(home.runs_v0_3(DEFAULT_LEZ));
        std::fs::write(home.chain_dir().join("bedrock_funding_public_key"), "f").unwrap();
        assert!(!home.runs_v0_3(DEFAULT_LEZ));
        home.record_chain_lez("v0.3.1").unwrap();
        assert!(home.runs_v0_3(DEFAULT_LEZ), "its own record wins");
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
    fn homes_can_share_one_download_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().join("a"))).unwrap();
        assert_eq!(home.downloads_dir(), home.root().join("downloads"));
        let shared = dir.path().join("shared");
        let home = home.with_downloads(shared.clone());
        assert_eq!(home.downloads_dir(), shared);
        assert_eq!(home.packages_dir(), shared.join("packages"));
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
            "ghcr.io/drptbl/logos-devnet-sequencer:v0.4.0-rc3"
        );
        assert_eq!(Settings::default().sequencer_url(), "http://127.0.0.1:3040");
    }
}
