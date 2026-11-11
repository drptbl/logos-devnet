//! `logos-devnet.toml`: what a project needs on the devnet, committed with the project. `up`
//! makes the devnet match it, and changes nothing when it already does.
//!
//! ```toml
//! home = ".logos-devnet"           # optional: this project's own devnet instead of the shared one
//!
//! [devnet]                         # optional: the same settings as `up`'s flags
//! block_time = "1s"
//! max_txs_per_block = 100
//!
//! [programs]                       # name = guest binary, relative to this file
//! counter = "target/riscv32im-risc0-zkvm-elf/docker/counter.bin"
//!
//! [accounts.users]                 # a group of funded accounts
//! count = 100
//! amount = 10_000_000_000
//!
//! [sessions.alice]                 # a logosctl session, as `session new`
//! modules = ["chat_module"]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::chain_config;
use crate::devnet::{DEFAULT_FUND, UpOptions};
use crate::envfile::{check_name, check_program_name};
use crate::groups;
use crate::home::{RuntimeKind, Settings};
use crate::session::{check_session_name, modules_with};

pub const FILE_NAME: &str = "logos-devnet.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    home: Option<PathBuf>,
    #[serde(default)]
    devnet: DevnetSection,
    #[serde(default)]
    programs: BTreeMap<String, PathBuf>,
    #[serde(default)]
    accounts: BTreeMap<String, GroupSpec>,
    #[serde(default)]
    sessions: BTreeMap<String, SessionSpec>,
}

/// `[devnet]`: settings the project needs. Each is optional.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevnetSection {
    pub lez: Option<String>,
    pub runtime: Option<RuntimeKind>,
    pub port: Option<u16>,
    pub block_time: Option<String>,
    pub max_txs_per_block: Option<u32>,
    pub dev_mode: Option<bool>,
}

impl DevnetSection {
    /// `settings` with every key this section sets, the way `up`'s flags apply.
    pub fn applied_to(&self, settings: Settings) -> Settings {
        UpOptions {
            docker: self.runtime == Some(RuntimeKind::Docker),
            native: self.runtime == Some(RuntimeKind::Native),
            lez: self.lez.clone(),
            port: self.port,
            block_time: self.block_time.clone(),
            max_txs_per_block: self.max_txs_per_block,
            dev_mode: self.dev_mode,
            rebuild: false,
        }
        .apply(settings)
    }

    /// The keys this section sets, with `settings`' values for them: `block_time = "1s"`.
    pub fn describe(&self, settings: &Settings) -> String {
        let mut keys = Vec::new();
        if self.lez.is_some() {
            keys.push(format!("lez = \"{}\"", settings.lez));
        }
        if self.runtime.is_some() {
            keys.push(format!("runtime = \"{:?}\"", settings.runtime).to_lowercase());
        }
        if self.port.is_some() {
            keys.push(format!("port = {}", settings.port));
        }
        if self.block_time.is_some() {
            keys.push(format!("block_time = \"{}\"", settings.block_time));
        }
        if self.max_txs_per_block.is_some() {
            keys.push(format!(
                "max_txs_per_block = {}",
                settings.max_txs_per_block
            ));
        }
        if self.dev_mode.is_some() {
            keys.push(format!("dev_mode = {}", settings.dev_mode));
        }
        keys.join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupSpec {
    pub count: usize,
    /// Lepta per account; `fund`'s default when left out.
    pub amount: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSpec {
    #[serde(default)]
    pub modules: Vec<String>,
    pub amount: Option<u64>,
}

/// A checked `logos-devnet.toml`.
#[derive(Debug, Clone)]
pub struct Project {
    pub path: PathBuf,
    /// The devnet home it names, absolute.
    pub home: Option<PathBuf>,
    pub devnet: DevnetSection,
    /// Program name to its binary, absolute.
    pub programs: BTreeMap<String, PathBuf>,
    pub accounts: BTreeMap<String, GroupSpec>,
    pub sessions: BTreeMap<String, SessionSpec>,
}

/// The nearest `logos-devnet.toml` in `start` or a directory above it.
pub fn find(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(FILE_NAME))
        .find(|file| file.is_file())
}

impl Project {
    /// Every program binary exists and is one LEZ loads. Checked by `up` and `reset` only, so
    /// other commands work while a build is missing.
    pub fn check_binaries(&self) -> Result<()> {
        for (name, binary) in &self.programs {
            if !binary.is_file() {
                bail!(
                    "[programs] {name}: {} doesn't exist. Build it first",
                    binary.display()
                );
            }
            let bytes = std::fs::read(binary)
                .with_context(|| format!("[programs] {name}: reading {}", binary.display()))?;
            crate::binary::segments(&bytes)
                .with_context(|| format!("[programs] {name}: {}", binary.display()))?;
        }
        Ok(())
    }

    /// Reads and checks the file: everything `up` would trip over later is refused here,
    /// before the devnet is touched.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(path, &text).with_context(|| format!("in {}", path.display()))
    }

    fn parse(path: &Path, text: &str) -> Result<Self> {
        let file: ProjectFile = toml::from_str(text)?;
        let dir = path
            .parent()
            .context("a project file has a directory")?
            .to_path_buf();
        let devnet = file.devnet;
        if let Some(block_time) = &devnet.block_time {
            chain_config::block_time_secs(block_time)?;
        }
        if let Some(max) = devnet.max_txs_per_block {
            chain_config::check_max_txs_per_block(max).context("[devnet]")?;
        }
        if devnet.port == Some(0) {
            bail!("[devnet] port can't be 0");
        }
        let mut programs = BTreeMap::new();
        for (name, binary) in file.programs {
            check_program_name(&name)?;
            programs.insert(name, dir.join(binary));
        }
        for (name, group) in &file.accounts {
            check_name("account group", name)?;
            groups::check_spec(group.count, group.amount.unwrap_or(DEFAULT_FUND))
                .with_context(|| format!("[accounts.{name}]"))?;
        }
        for (name, session) in &file.sessions {
            check_session_name(name)?;
            modules_with(&session.modules).with_context(|| format!("[sessions.{name}]"))?;
            if session.amount == Some(0) {
                bail!("[sessions.{name}] amount can't be 0");
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            home: file.home.map(|home| dir.join(home)),
            devnet,
            programs,
            accounts: file.accounts,
            sessions: file.sessions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_in(dir: &Path, text: &str) -> Result<Project> {
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, text).unwrap();
        Project::load(&path)
    }

    #[test]
    fn reads_every_section_and_resolves_paths_against_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("out/counter.bin"), [0u8; 4]).unwrap();
        let project = project_in(
            dir.path(),
            r#"
home = ".logos-devnet"
[devnet]
block_time = "500ms"
max_txs_per_block = 100
[programs]
counter = "out/counter.bin"
[accounts.users]
count = 100
amount = 5_000
[sessions.alice]
modules = ["chat_module"]
"#,
        )
        .unwrap();
        assert_eq!(project.home, Some(dir.path().join(".logos-devnet")));
        assert_eq!(project.devnet.block_time.as_deref(), Some("500ms"));
        assert_eq!(project.devnet.max_txs_per_block, Some(100));
        assert_eq!(
            project.programs["counter"],
            dir.path().join("out/counter.bin")
        );
        assert_eq!(
            project.accounts["users"],
            GroupSpec {
                count: 100,
                amount: Some(5_000)
            }
        );
        assert_eq!(project.sessions["alice"].modules, ["chat_module"]);
    }

    #[test]
    fn the_devnet_section_sets_only_its_keys() {
        let section = DevnetSection {
            block_time: Some("500ms".into()),
            max_txs_per_block: Some(100),
            ..DevnetSection::default()
        };
        let applied = section.applied_to(Settings::default());
        assert_eq!(applied.block_time, "500ms");
        assert_eq!(applied.max_txs_per_block, 100);
        assert_eq!(applied.port, Settings::default().port);
        assert_eq!(
            section.describe(&Settings::default()),
            "block_time = \"1s\", max_txs_per_block = 20"
        );
    }

    #[test]
    fn an_empty_file_asks_for_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_in(dir.path(), "").unwrap();
        assert!(project.home.is_none() && project.programs.is_empty());
        assert_eq!(project.devnet, DevnetSection::default());
    }

    #[test]
    fn the_devnet_section_turns_on_dev_mode() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_in(dir.path(), "[devnet]\ndev_mode = true\n").unwrap();
        let applied = project.devnet.applied_to(Settings::default());
        assert!(applied.dev_mode);
        assert_eq!(project.devnet.describe(&applied), "dev_mode = true");
    }

    #[test]
    fn refuses_what_up_would_trip_over() {
        let dir = tempfile::tempdir().unwrap();
        let missing = project_in(dir.path(), "[programs]\ncounter = \"missing.bin\"").unwrap();
        assert!(
            missing.check_binaries().is_err(),
            "loads, but up refuses it"
        );
        for bad in [
            "[programs]\nCounter = \"x\"",
            "[accounts.users]\ncount = 0",
            "[accounts.users]\ncount = 10001",
            "[accounts.users]\ncount = 1\namount = 0",
            "[accounts.users]\ncount = 1\namount = 99999999999999999999",
            "[accounts.Users]\ncount = 1",
            "[sessions.alice]\nmodules = [\"--force\"]",
            "[devnet]\nblock_time = \"1\"",
            "[devnet]\nmax_txs_per_block = 0",
            "[devnet]\nport = 0",
            "[devnet]\ncolour = \"blue\"",
            "unknown = 1",
        ] {
            assert!(project_in(dir.path(), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn finds_the_nearest_file_above() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("a").join(FILE_NAME), "").unwrap();
        assert_eq!(find(&nested), Some(dir.path().join("a").join(FILE_NAME)));
    }
}
