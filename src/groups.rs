//! Account groups: many funded accounts for tests that need many users, e.g. a hundred
//! parties driving one program. Each group keeps its keys in one LEZ wallet and lists them in
//! `accounts.json`, so `lez_core`, the LEZ wallet and scripts can all use them.
//!
//! ```text
//! accounts/<name>/wallet/          a LEZ wallet holding the group's keys
//! accounts/<name>/accounts.json    the amount each account is funded with, and the accounts
//! ```
//!
//! A group that exists when a chain starts is funded at genesis, so any number of accounts is
//! ready at once. One added to a running chain is funded one transfer at a time.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::account::PublicId;
use crate::envfile::check_name;
use crate::home::{read_dir_if_present, write_json};

/// The most accounts one group holds. Genesis takes every one of them.
pub const MAX_ACCOUNTS: usize = 10_000;

/// A group's size and per-account amount, as `accounts new` and the project file take them.
pub fn check_spec(count: usize, amount: u64) -> anyhow::Result<()> {
    if count == 0 || count > MAX_ACCOUNTS {
        anyhow::bail!("an account group's count must be 1 to {MAX_ACCOUNTS}");
    }
    if amount == 0 {
        anyhow::bail!("an account group's amount can't be 0");
    }
    Ok(())
}

/// What `accounts.json` holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    /// What each account is funded with, in lepta.
    pub amount: u64,
    pub accounts: Vec<GroupAccount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAccount {
    /// Its place in the group, from 0.
    pub index: usize,
    /// Base58, the form the LEZ wallet takes as `Public/<id>`.
    pub id: PublicId,
    /// Hex, the form `lez_core` takes.
    pub hex: String,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    dir: PathBuf,
}

impl Group {
    pub fn new(groups_dir: &Path, name: &str) -> Result<Self> {
        check_name("account group", name)?;
        Ok(Self {
            name: name.to_owned(),
            dir: groups_dir.join(name),
        })
    }

    pub fn wallet_dir(&self) -> PathBuf {
        self.dir.join("wallet")
    }

    /// The group wallet's `storage.json`, which the LEZ wallet writes on first use.
    pub fn wallet_storage(&self) -> PathBuf {
        self.wallet_dir().join("storage.json")
    }

    pub fn manifest(&self) -> PathBuf {
        self.dir.join("accounts.json")
    }

    /// The stored info, or an empty group.
    pub fn info(&self) -> Result<GroupInfo> {
        match std::fs::read_to_string(self.manifest()) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("reading {}", self.manifest().display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GroupInfo::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.manifest().display())),
        }
    }

    pub fn save_info(&self, info: &GroupInfo) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_json(&self.manifest(), &serde_json::to_value(info)?)
    }

    /// `info` with `ids` appended, numbered after the accounts it has.
    pub fn extended(mut info: GroupInfo, ids: Vec<PublicId>) -> GroupInfo {
        let start = info.accounts.len();
        info.accounts
            .extend(ids.into_iter().enumerate().map(|(i, id)| GroupAccount {
                index: start + i,
                hex: id.hex(),
                id,
            }));
        info
    }
}

/// The groups under `dir`, by name.
pub fn list(dir: &Path) -> Result<Vec<Group>> {
    let mut groups = Vec::new();
    for entry in read_dir_if_present(dir)?.into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_dir() && check_name("account group", &name).is_ok() {
            groups.push(Group::new(dir, &name)?);
        }
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d";
    const B: &str = "6YvesSEDWW25kpi8SSki2c8o5m3uACsigvGnNQ6wMruh";

    #[test]
    fn numbers_new_accounts_after_the_existing_ones() {
        let one = Group::extended(GroupInfo::default(), vec![PublicId::parse(A).unwrap()]);
        let two = Group::extended(one, vec![PublicId::parse(B).unwrap()]);
        assert_eq!(two.accounts[0].index, 0);
        assert_eq!(two.accounts[1].index, 1);
        assert_eq!(two.accounts[1].id.as_str(), B);
        assert_eq!(two.accounts[1].hex, PublicId::parse(B).unwrap().hex());
    }

    #[test]
    fn a_missing_group_is_empty_and_info_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let group = Group::new(dir.path(), "users").unwrap();
        assert_eq!(group.info().unwrap(), GroupInfo::default());
        let info = Group::extended(
            GroupInfo {
                amount: 5,
                ..GroupInfo::default()
            },
            vec![PublicId::parse(A).unwrap()],
        );
        group.save_info(&info).unwrap();
        assert_eq!(group.info().unwrap(), info);
        // The file is plain JSON a script can read.
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(group.manifest()).unwrap()).unwrap();
        assert_eq!(json["accounts"][0]["id"], A);
    }

    #[test]
    fn refuses_bad_names_and_lists_groups() {
        assert!(Group::new(Path::new("/x"), "Users").is_err());
        let dir = tempfile::tempdir().unwrap();
        for name in ["users", "admins", "Not-A-Group"] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        let names: Vec<_> = list(dir.path())
            .unwrap()
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["admins", "users"]);
    }
}
