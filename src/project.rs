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
//! shop = { path = "target/riscv32im-risc0-zkvm-elf/docker/shop.bin", mutable = true }
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
use crate::fetch;
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
    programs: BTreeMap<String, ProgramEntry>,
    #[serde(default)]
    accounts: BTreeMap<String, GroupSpec>,
    #[serde(default)]
    sessions: BTreeMap<String, SessionSpec>,
    messaging: Option<MessagingSection>,
    storage: Option<StorageSection>,
}

/// `[messaging]`: an empty table that asks for the local messaging hub.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagingSection {}

/// `[storage]`: an empty table that asks for a local storage network between sessions.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageSection {}

/// A `[programs]` entry: the binary's path, or a table that can name a release instead and make
/// it mutable.
#[derive(Debug, Deserialize)]
#[serde(
    untagged,
    expecting = "a binary's path, or { path = \"...\" } or { url = \"...\", sha256 = \"...\" }, with mutable = true if wanted"
)]
enum ProgramEntry {
    Path(PathBuf),
    Table(ProgramTable),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgramTable {
    path: Option<PathBuf>,
    url: Option<String>,
    sha256: Option<String>,
    #[serde(default)]
    mutable: bool,
}

/// Where a program's binary comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramSource {
    /// A file on this machine, absolute.
    File(PathBuf),
    /// A download, e.g. a program's GitHub release, kept only when its SHA-256 matches.
    Release { url: String, sha256: String },
}

/// A program the project deploys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramSpec {
    /// Where its binary comes from.
    pub source: ProgramSource,
    /// Deployed mutable, so a new build updates it in place. Undisclosed private calls need an
    /// immutable program, so it's off unless asked for.
    pub mutable: bool,
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
    pub log_filter: Option<String>,
}

impl DevnetSection {
    /// `settings` with every key this section sets, the way `up`'s flags apply.
    pub fn applied_to(&self, settings: Settings) -> Settings {
        UpOptions {
            docker: self.runtime == Some(RuntimeKind::Docker),
            native: self.runtime == Some(RuntimeKind::Native),
            lez: self.lez.clone(),
            // 0 keeps the port the devnet took the first time (see `Devnet::open`).
            port: self.port.filter(|port| *port != 0),
            block_time: self.block_time.clone(),
            max_txs_per_block: self.max_txs_per_block,
            dev_mode: self.dev_mode,
            log_filter: self.log_filter.clone(),
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
        if self.log_filter.is_some() {
            keys.push(format!("log_filter = \"{}\"", settings.log_filter));
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
    /// Program name to what to deploy.
    pub programs: BTreeMap<String, ProgramSpec>,
    pub accounts: BTreeMap<String, GroupSpec>,
    pub sessions: BTreeMap<String, SessionSpec>,
    /// Whether it asks for the local messaging hub.
    pub messaging: bool,
    /// Whether it asks for a local storage network between sessions.
    pub storage: bool,
}

/// The nearest `logos-devnet.toml` in `start` or a directory above it.
pub fn find(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(FILE_NAME))
        .find(|file| file.is_file())
}

impl Project {
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
        if let Some(filter) = &devnet.log_filter {
            crate::home::check_log_filter(filter).context("[devnet]")?;
        }
        if devnet.port == Some(0) && file.home.is_none() {
            bail!(
                "[devnet] port = 0 needs a home of the project's own, e.g. home = \".logos-devnet\": \
                 the shared devnet keeps its port"
            );
        }
        let mut programs = BTreeMap::new();
        for (name, entry) in file.programs {
            check_program_name(&name)?;
            let program = match entry {
                ProgramEntry::Path(path) => ProgramSpec {
                    source: ProgramSource::File(dir.join(path)),
                    mutable: false,
                },
                ProgramEntry::Table(table) => {
                    let source = match (table.path, table.url, table.sha256) {
                        (Some(path), None, None) => ProgramSource::File(dir.join(path)),
                        (None, Some(url), Some(sha256)) => {
                            fetch::check_url(&url)
                                .and_then(|()| fetch::check_sha256(&sha256))
                                .with_context(|| format!("[programs] {name}"))?;
                            ProgramSource::Release { url, sha256 }
                        }
                        (None, Some(_), None) => {
                            bail!("[programs] {name}: a url needs the file's sha256, to check it")
                        }
                        _ => bail!("[programs] {name}: give a path, or a url with its sha256"),
                    };
                    ProgramSpec {
                        source,
                        mutable: table.mutable,
                    }
                }
            };
            programs.insert(name, program);
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
            messaging: file.messaging.is_some(),
            storage: file.storage.is_some(),
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
    fn port_zero_needs_a_home_of_the_projects_own() {
        let dir = tempfile::tempdir().unwrap();
        assert!(project_in(dir.path(), "[devnet]\nport = 0\n").is_err());
        assert!(project_in(dir.path(), "home = \".logos-devnet\"\n[devnet]\nport = 0\n").is_ok());
    }

    #[test]
    fn an_empty_storage_table_asks_for_local_storage() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_in(dir.path(), "[storage]\n").unwrap();
        assert!(project.storage && !project.messaging);
        assert!(!project_in(dir.path(), "").unwrap().storage);
        assert!(project_in(dir.path(), "[storage]\nport = 1\n").is_err());
    }

    #[test]
    fn an_empty_messaging_table_asks_for_the_hub() {
        let dir = tempfile::tempdir().unwrap();
        assert!(project_in(dir.path(), "[messaging]\n").unwrap().messaging);
        assert!(!project_in(dir.path(), "").unwrap().messaging);
        assert!(project_in(dir.path(), "[messaging]\nport = 1\n").is_err());
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
            ProgramSpec {
                source: ProgramSource::File(dir.path().join("out/counter.bin")),
                mutable: false
            }
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
    fn a_program_is_a_path_or_a_table_that_makes_it_mutable() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_in(
            dir.path(),
            "[programs]\ncounter = \"c.bin\"\nshop = { path = \"s.bin\", mutable = true }\n\
             vault = { path = \"v.bin\" }\n",
        )
        .unwrap();
        assert!(!project.programs["counter"].mutable);
        assert_eq!(
            project.programs["shop"],
            ProgramSpec {
                source: ProgramSource::File(dir.path().join("s.bin")),
                mutable: true
            }
        );
        assert!(!project.programs["vault"].mutable, "immutable unless asked");
        let odd = project_in(
            dir.path(),
            "[programs]\nshop = { path = \"s.bin\", mutible = true }\n",
        );
        assert!(odd.is_err(), "a misspelled key is refused");
    }

    #[test]
    fn the_devnet_section_sets_the_log_filter() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_in(dir.path(), "[devnet]\nlog_filter = \"debug\"\n").unwrap();
        let applied = project.devnet.applied_to(Settings::default());
        assert_eq!(applied.log_filter, "debug");
        assert_eq!(project.devnet.describe(&applied), "log_filter = \"debug\"");
        assert!(project_in(dir.path(), "[devnet]\nlog_filter = \"\"\n").is_err());
    }

    #[test]
    fn a_program_can_come_from_a_release() {
        let dir = tempfile::tempdir().unwrap();
        let sha = "e39f35af4f8b93ee5e1ca0ecb6c4f524c43c4446a8a6ad56d8c7d3fde7c3deb5";
        let url = "https://example.com/releases/v1.0.0/token.bin";
        let project = project_in(
            dir.path(),
            &format!("[programs]\ntoken = {{ url = \"{url}\", sha256 = \"{sha}\" }}\n"),
        )
        .unwrap();
        assert_eq!(
            project.programs["token"],
            ProgramSpec {
                source: ProgramSource::Release {
                    url: url.into(),
                    sha256: sha.into()
                },
                mutable: false
            }
        );
        for odd in [
            format!("token = {{ url = \"{url}\" }}"),
            format!("token = {{ url = \"{url}\", sha256 = \"abc\" }}"),
            format!("token = {{ path = \"e.bin\", url = \"{url}\", sha256 = \"{sha}\" }}"),
            format!("token = {{ path = \"e.bin\", sha256 = \"{sha}\" }}"),
            "token = { mutable = true }".to_owned(),
            // curl would read these as an option, or a local file.
            format!("token = {{ url = \"-K/etc/passwd\", sha256 = \"{sha}\" }}"),
            format!("token = {{ url = \"file:///etc/passwd\", sha256 = \"{sha}\" }}"),
        ] {
            let err = project_in(dir.path(), &format!("[programs]\n{odd}\n"));
            assert!(err.is_err(), "{odd}");
        }
    }

    #[test]
    fn the_examples_project_files_load() {
        let file =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello-world/logos-devnet.toml");
        let project = Project::load(&file).unwrap();
        assert_eq!(project.accounts["tutorial"].count, 2);
        let file =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/sessions/logos-devnet.toml");
        let project = Project::load(&file).unwrap();
        assert_eq!(project.devnet.dev_mode, Some(true));
        assert_eq!(project.sessions.len(), 2);
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
