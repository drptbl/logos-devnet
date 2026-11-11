//! Sessions: throwaway `logosctl` sessions wired to the devnet, so an app (or two agents) can
//! run against it with a funded wallet in two commands.
//!
//! ```text
//! sessions/<name>/logosctl/      the session's LOGOSCTL_CONFIG_DIR: its daemon, modules, data
//! sessions/<name>/wallet/        wallet_config.json pointing at the devnet, and storage.json
//! sessions/<name>/session.json   the modules to load, the account and what it was funded with
//! ```
//!
//! Modules come from the official catalog. `lez_core` is always installed, and the session's
//! wallet is open in its daemon whenever the daemon runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::account::PublicId;
use crate::home::{read_dir_if_present, write_json};
use crate::runtime::stdout_of;

const LEZ_CORE: &str = "lez_core";
/// How long `stop` waits for a daemon to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// What a session remembers about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Every module installed, `lez_core` first; `up` loads them again after a restart.
    pub modules: Vec<String>,
    /// The wallet's public account, funded from the faucet.
    pub account: PublicId,
    /// What the account was funded with; `reset` funds it with this again.
    pub funded: u128,
}

/// One session's directory.
#[derive(Debug, Clone)]
pub struct Session {
    pub name: String,
    dir: PathBuf,
}

impl Session {
    pub fn new(sessions_dir: &Path, name: &str) -> Result<Self> {
        check_session_name(name)?;
        Ok(Self {
            name: name.to_owned(),
            dir: sessions_dir.join(name),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn logosctl_dir(&self) -> PathBuf {
        self.dir.join("logosctl")
    }
    pub fn wallet_dir(&self) -> PathBuf {
        self.dir.join("wallet")
    }
    pub fn wallet_config(&self) -> PathBuf {
        self.wallet_dir().join("wallet_config.json")
    }
    pub fn wallet_storage(&self) -> PathBuf {
        self.wallet_dir().join("storage.json")
    }
    fn info_file(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    pub fn exists(&self) -> bool {
        self.dir.exists()
    }

    /// The stored info. Module names are checked again, since they reach logosctl's argv.
    pub fn info(&self) -> Result<SessionInfo> {
        let path = self.info_file();
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("session {} is incomplete: no session.json", self.name))?;
        let info: SessionInfo =
            serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
        info.modules.iter().try_for_each(|m| check_module(m))?;
        Ok(info)
    }

    pub fn save_info(&self, info: &SessionInfo) -> Result<()> {
        write_json(&self.info_file(), &serde_json::to_value(info)?)
    }

    pub fn logosctl(&self) -> Logosctl {
        Logosctl::new(self.logosctl_dir())
    }

    /// What an app needs from the session, by variable name.
    pub fn vars(&self, info: &SessionInfo) -> [(&'static str, String); 4] {
        [
            ("LOGOSCTL_CONFIG_DIR", path_text(&self.logosctl_dir())),
            (
                "LOGOS_DEVNET_WALLET_CONFIG",
                path_text(&self.wallet_config()),
            ),
            (
                "LOGOS_DEVNET_WALLET_STORAGE",
                path_text(&self.wallet_storage()),
            ),
            ("LOGOS_DEVNET_ACCOUNT", info.account.wallet_form()),
        ]
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The sessions under `dir`, by name.
pub fn list(dir: &Path) -> Result<Vec<Session>> {
    let mut sessions = Vec::new();
    for entry in read_dir_if_present(dir)?.into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_dir() && check_session_name(&name).is_ok() {
            sessions.push(Session::new(dir, &name)?);
        }
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sessions)
}

/// The modules a session installs: `lez_core`, then the extras in order, without repeats.
pub fn modules_with(extra: &[String]) -> Result<Vec<String>> {
    let mut modules = vec![LEZ_CORE.to_owned()];
    for module in extra {
        check_module(module)?;
        if !modules.contains(module) {
            modules.push(module.clone());
        }
    }
    Ok(modules)
}

/// A catalog module name: letters, digits, `_` and `-`, starting with a letter or digit, so a
/// name can't reach logosctl as a flag.
fn check_module(name: &str) -> Result<()> {
    let ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !ok {
        bail!("not a module name: {name:?}");
    }
    Ok(())
}

/// Whether `logosctl` runs from the PATH.
pub fn logosctl_on_path() -> bool {
    Command::new("logosctl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Session names are lower-case letters, digits and `-`, starting with a letter: they become
/// directory names.
pub fn check_session_name(name: &str) -> Result<()> {
    crate::envfile::check_name("session", name)
}

/// The Logos Core CLI for one session directory.
pub struct Logosctl {
    program: PathBuf,
    dir: PathBuf,
}

impl Logosctl {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            program: "logosctl".into(),
            dir,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.env("LOGOSCTL_CONFIG_DIR", &self.dir).arg("--json");
        cmd
    }

    /// Runs logosctl and returns its stdout. Fails when it exits non-zero.
    fn output(&self, args: &[&str]) -> Result<String> {
        stdout_of(self.cmd().args(args)).map_err(|e| {
            let missing = e
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
            if missing {
                e.context("sessions need logosctl, the Logos Core CLI, on your PATH")
            } else {
                e
            }
        })
    }

    /// Runs a logosctl command that answers in JSON.
    fn run(&self, args: &[&str]) -> Result<Value> {
        last_json(&self.output(args)?)
    }

    /// Starts the session's daemon and returns once it takes commands. (`daemon start`
    /// answers in plain text even with `--json`.)
    pub fn start(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        if !self.running() {
            self.output(&["daemon", "start", "--detach"])?;
        }
        Ok(())
    }

    /// Stops the session's daemon and waits for it to exit.
    pub fn stop(&self) -> Result<()> {
        if !self.running() {
            return Ok(());
        }
        self.output(&["daemon", "stop"])?;
        let deadline = Instant::now() + STOP_TIMEOUT;
        while self.running() {
            if Instant::now() > deadline {
                bail!(
                    "the session's daemon did not stop within {}s",
                    STOP_TIMEOUT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(())
    }

    /// Whether the session's daemon answers.
    pub fn running(&self) -> bool {
        self.dir.exists()
            && self
                .cmd()
                .args(["daemon", "status"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
    }

    /// Installs `modules` from the catalog, unsigned ones included (`lez_core` is unsigned).
    pub fn install(&self, modules: &[String]) -> Result<()> {
        self.run(&["catalog", "refresh"])?;
        for module in modules {
            self.run(&["package", "install", module, "--yes"])
                .with_context(|| format!("installing {module}"))?;
        }
        Ok(())
    }

    /// Loads the `modules` that aren't loaded yet.
    pub fn load(&self, modules: &[String]) -> Result<()> {
        let listed = self.run(&["module", "ls"])?;
        for module in modules.iter().filter(|m| !module_loaded(&listed, m)) {
            self.run(&["module", "load", module])
                .with_context(|| format!("loading {module}"))?;
        }
        Ok(())
    }

    /// Calls `method` on `module` with string arguments and returns its result.
    pub fn call(&self, module: &str, method: &str, args: &[&str]) -> Result<Value> {
        let args: Vec<String> = args.iter().map(|a| format!("str:{a}")).collect();
        let mut argv = vec!["call", module, method];
        argv.extend(args.iter().map(String::as_str));
        call_result(self.run(&argv)?, &format!("{module}.{method}"))
    }

    /// Creates the wallet with an empty password. `create_new` answers with the recovery
    /// phrase, so nothing it printed is passed on, not even on failure.
    pub fn create_wallet(&self, config: &Path, storage: &Path) -> Result<()> {
        let (config, storage) = (path_text(config), path_text(storage));
        self.call(LEZ_CORE, "create_new", &[&config, &storage, ""])
            .map(drop)
            .map_err(|_| {
                anyhow!(
                    "lez_core.create_new failed. Its output is left out: it can hold the \
                     wallet's recovery phrase"
                )
            })
    }

    /// Makes the session's wallet (`storage`) forget the chain it synced, for a new one. A
    /// wallet open in `lez_core` is closed first, so `lez_core` can't write its copy of the old
    /// chain back over the file. `ensure_wallet` opens it again.
    pub fn forget_chain(&self, storage: &Path) -> Result<()> {
        if self.running() && module_loaded(&self.run(&["module", "ls"])?, LEZ_CORE) {
            let current = self.call(LEZ_CORE, "get_sequencer_addr", &[])?;
            if !current.as_str().unwrap_or_default().is_empty() {
                self.run(&["module", "reload", LEZ_CORE])?;
            }
        }
        forget_chain_in(storage)
    }

    /// Makes sure `lez_core` has the session's wallet open on `sequencer`. `open` fails
    /// while a wallet is open, so one open on another sequencer (an old port) is closed by
    /// reloading `lez_core` first.
    pub fn ensure_wallet(&self, config: &Path, storage: &Path, sequencer: &str) -> Result<()> {
        let current = self.call(LEZ_CORE, "get_sequencer_addr", &[])?;
        let current = current.as_str().unwrap_or_default();
        if !current.is_empty() {
            if same_url(current, sequencer) {
                return Ok(());
            }
            self.run(&["module", "reload", LEZ_CORE])?;
        }
        let (config, storage) = (path_text(config), path_text(storage));
        let code = self.call(LEZ_CORE, "open", &[&config, &storage])?;
        if code != 0 {
            bail!("lez_core could not open the session's wallet (code {code})");
        }
        Ok(())
    }
}

/// Whether `module ls` (`listed`) shows `name` loaded.
fn module_loaded(listed: &Value, name: &str) -> bool {
    listed.as_array().is_some_and(|all| {
        all.iter()
            .any(|m| m["name"] == name && m["status"] == "loaded")
    })
}

/// A JSON value kept as written. The wallet stores `u128` nonces, which `Value` would turn into
/// floats the wallet can't read, so `storage.json` is only ever changed where it must be.
type Raw = Box<RawValue>;
type RawObject = BTreeMap<String, Raw>;

/// The LEZ wallet's state of a fresh account: what every account holds on a new chain.
fn fresh_account() -> Result<Raw> {
    Ok(RawValue::from_string(
        r#"{"nonce":0,"data":{"shards":{}}}"#.into(),
    )?)
}

/// `raw`, an object, with `change` applied to its members.
fn edit_object(raw: &mut Raw, change: impl FnOnce(&mut RawObject) -> Result<()>) -> Result<()> {
    let mut object: RawObject = serde_json::from_str(raw.get())?;
    change(&mut object)?;
    *raw = serde_json::value::to_raw_value(&object)?;
    Ok(())
}

/// `change` applied to `object`'s member `key`, when it has one.
fn edit_member(
    object: &mut RawObject,
    key: &str,
    change: impl FnOnce(&mut Raw) -> Result<()>,
) -> Result<()> {
    object.get_mut(key).map_or(Ok(()), change)
}

/// A LEZ wallet's `storage.json` (`text`) without what it learned from a chain: how far it
/// synced, and the state of its private and shared private accounts. Keys, labels and
/// everything else stay as written, so the wallet's accounts are the same on the next chain.
/// Accounts imported from genesis data keep their state. A layout it doesn't know is an error,
/// not a guess.
fn forget_chain(text: &str) -> Result<String> {
    let mut storage: RawObject = serde_json::from_str(text)?;
    storage.insert(
        "last_synced_block".into(),
        RawValue::from_string("0".into())?,
    );
    edit_member(&mut storage, "key_chain", |key_chain| {
        edit_object(key_chain, |key_chain| {
            edit_member(key_chain, "accounts", |accounts| {
                let mut accounts_list: Vec<RawObject> = serde_json::from_str(accounts.get())?;
                for account in &mut accounts_list {
                    edit_member(account, "Private", |private| {
                        edit_object(private, |private| {
                            edit_member(private, "data", |data| {
                                edit_object(data, |data| {
                                    edit_member(data, "value", forget_private_states)
                                })
                            })
                        })
                    })?;
                }
                *accounts = serde_json::value::to_raw_value(&accounts_list)?;
                Ok(())
            })?;
            edit_member(key_chain, "shared_private_accounts", |shared| {
                let mut entries: RawObject = serde_json::from_str(shared.get())?;
                for entry in entries.values_mut() {
                    edit_object(entry, |entry| {
                        if entry.contains_key("account") {
                            entry.insert("account".into(), fresh_account()?);
                        }
                        Ok(())
                    })?;
                }
                *shared = serde_json::value::to_raw_value(&entries)?;
                Ok(())
            })
        })
    })?;
    Ok(serde_json::to_string_pretty(&storage)? + "\n")
}

/// A private account's `value`: its keys, then its state per kind of private account, each
/// set back to a fresh account.
fn forget_private_states(value: &mut Raw) -> Result<()> {
    let (keys, states): (Raw, Vec<(Raw, Raw)>) = serde_json::from_str(value.get())?;
    let states = states
        .into_iter()
        .map(|(kind, _)| Ok((kind, fresh_account()?)))
        .collect::<Result<Vec<_>>>()?;
    *value = serde_json::value::to_raw_value(&(keys, states))?;
    Ok(())
}

/// `forget_chain` on the file, when the wallet has written one. The new file replaces the old
/// one whole: it holds the wallet's keys.
fn forget_chain_in(storage: &Path) -> Result<()> {
    let text = match std::fs::read_to_string(storage) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", storage.display())),
    };
    let forgotten = forget_chain(&text).with_context(|| {
        format!(
            "{}: not a LEZ wallet storage this version knows",
            storage.display()
        )
    })?;
    let next = storage.with_extension("json.new");
    std::fs::write(&next, forgotten).with_context(|| format!("writing {}", next.display()))?;
    std::fs::rename(&next, storage).with_context(|| format!("replacing {}", storage.display()))
}

/// URLs equal but for a trailing `/`: lez_core reports `http://127.0.0.1:3040/`.
fn same_url(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The last line of `out` that parses as JSON: logosctl may print notices before it.
fn last_json(out: &str) -> Result<Value> {
    out.lines()
        .rev()
        .find_map(|line| serde_json::from_str(line.trim()).ok())
        .with_context(|| format!("logosctl printed no JSON:\n{out}"))
}

/// The `result` of a `logosctl call` reply, or its error.
fn call_result(reply: Value, what: &str) -> Result<Value> {
    if reply["status"] != "ok" {
        let message = reply["message"].as_str().unwrap_or("no message");
        bail!("{what} failed: {message}");
    }
    reply
        .get("result")
        .cloned()
        .with_context(|| format!("{what} returned no result"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d";
    const PHRASE: &str = "amused river fossil";

    /// A logosctl stand-in: records its arguments, one call per line, and answers per `script`.
    fn fake_logosctl(dir: &Path, script: &str) -> Logosctl {
        let program = dir.join("logosctl");
        let source = dir.join("logosctl.sh");
        let log = dir.join("calls.log");
        std::fs::write(
            &source,
            format!("#!/bin/sh\necho \"$@\" >> '{}'\n{script}\n", log.display()),
        )
        .unwrap();
        // `install` writes the executable in its own process. Written here, a process another
        // test thread forks in that moment would hold it open for writing, and running it would
        // fail with "Text file busy" on Linux.
        let installed = std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&source)
            .arg(&program)
            .status()
            .unwrap();
        assert!(installed.success());
        Logosctl {
            program,
            dir: dir.join("session"),
        }
    }

    fn calls(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
    }

    /// A logosctl whose lez_core has a wallet open on `addr` ("" for none).
    fn lez_core_on(dir: &Path, addr: &str) -> Logosctl {
        fake_logosctl(
            dir,
            &format!(
                r#"case "$4" in
  get_sequencer_addr) echo '{{"result":"{addr}","status":"ok"}}' ;;
  open) echo '{{"result":0,"status":"ok"}}' ;;
  *) echo '{{"status":"ok"}}' ;;
esac"#
            ),
        )
    }

    fn ensure(ctl: &Logosctl) {
        ctl.ensure_wallet(
            Path::new("/w/c.json"),
            Path::new("/w/s.json"),
            "http://127.0.0.1:3055",
        )
        .unwrap();
    }

    #[test]
    fn opens_a_closed_wallet_with_string_arguments() {
        let dir = tempfile::tempdir().unwrap();
        ensure(&lez_core_on(dir.path(), ""));
        assert_eq!(
            calls(dir.path()),
            "--json call lez_core get_sequencer_addr\n\
             --json call lez_core open str:/w/c.json str:/w/s.json\n"
        );
    }

    #[test]
    fn leaves_a_wallet_open_on_the_right_sequencer() {
        let dir = tempfile::tempdir().unwrap();
        ensure(&lez_core_on(dir.path(), "http://127.0.0.1:3055/"));
        assert_eq!(
            calls(dir.path()),
            "--json call lez_core get_sequencer_addr\n"
        );
    }

    #[test]
    fn reopens_a_wallet_left_on_an_old_port() {
        let dir = tempfile::tempdir().unwrap();
        ensure(&lez_core_on(dir.path(), "http://127.0.0.1:3040/"));
        assert_eq!(
            calls(dir.path()),
            "--json call lez_core get_sequencer_addr\n\
             --json module reload lez_core\n\
             --json call lez_core open str:/w/c.json str:/w/s.json\n"
        );
    }

    /// A session's `storage.json` after a chain ran, as the LEZ wallet writes it: synced to
    /// block 42, a private account and a shared one holding notes, and an account imported from
    /// genesis data whose nonce only fits a u128.
    const SYNCED_STORAGE: &str = r#"{
  "key_chain": {
    "accounts": [
      { "Public": { "account_id": "P", "chain_index": "/0", "data": { "k": [1, 2] } } },
      { "Private": { "account_id": "Q", "chain_index": "/0", "data": {
        "value": [ { "keys": 2 }, [
          [ { "Regular": "111" }, { "nonce": 7, "data": { "shards": { "T": [1, 2] } } } ],
          [ { "Regular": "222" }, { "nonce": 3, "data": { "shards": {} } } ]
        ] ],
        "ccc": [0], "cci": 0 } } },
      { "ImportedPrivate": { "account": { "nonce": 340282366920938463463374607431768211455 } } }
    ],
    "group_key_holders": { "g": { "k": 3 } },
    "shared_private_accounts": {
      "S": { "group_label": "g", "identifier": "1", "pda_seed": null,
             "authority_program_id": null,
             "account": { "nonce": 5, "data": { "shards": { "T": [9] } } } }
    }
  },
  "last_synced_block": 42,
  "labels": { "main": { "Public": "P" } }
}"#;

    #[test]
    fn forgetting_a_chain_keeps_keys_and_drops_its_state() {
        let text = forget_chain(SYNCED_STORAGE).unwrap();
        // Numbers past u64 come back as written, not as floats.
        assert!(
            text.contains("340282366920938463463374607431768211455"),
            "{text}"
        );
        let storage: Value = serde_json::from_str(&text).unwrap();
        let fresh = serde_json::json!({ "nonce": 0, "data": { "shards": {} } });
        assert_eq!(storage["last_synced_block"], 0);
        let private = &storage["key_chain"]["accounts"][1]["Private"]["data"]["value"];
        assert_eq!(private[0], serde_json::json!({ "keys": 2 }), "keys stay");
        assert_eq!(private[1][0][0], serde_json::json!({ "Regular": "111" }));
        assert_eq!(private[1][0][1], fresh);
        assert_eq!(private[1][1][1], fresh);
        let shared = &storage["key_chain"]["shared_private_accounts"]["S"];
        assert_eq!(shared["account"], fresh);
        assert_eq!(shared["group_label"], "g");
        // Everything else is as it was.
        let mut expected: Value = serde_json::from_str(SYNCED_STORAGE).unwrap();
        expected["last_synced_block"] = 0.into();
        expected["key_chain"]["accounts"][1] = storage["key_chain"]["accounts"][1].clone();
        expected["key_chain"]["shared_private_accounts"]["S"]["account"] = fresh;
        assert_eq!(storage, expected);
    }

    #[test]
    fn forgetting_a_chain_adds_nothing_and_refuses_a_layout_it_doesnt_know() {
        let bare = r#"{"key_chain":{"accounts":[]},"last_synced_block":9}"#;
        let storage: Value = serde_json::from_str(&forget_chain(bare).unwrap()).unwrap();
        assert_eq!(
            storage,
            serde_json::json!({ "key_chain": { "accounts": [] }, "last_synced_block": 0 })
        );
        let odd = r#"{"key_chain":{"accounts":[{"Private":{"data":{"value":[1,2,3]}}}]}}"#;
        assert!(forget_chain(odd).is_err());
    }

    /// A logosctl whose daemon runs with lez_core loaded and a wallet open on `addr`.
    fn running_lez_core_on(dir: &Path, addr: &str) -> Logosctl {
        std::fs::create_dir_all(dir.join("session")).unwrap();
        fake_logosctl(
            dir,
            &format!(
                r#"case "$2 $3 $4" in
  "daemon status "*) echo '{{"status":"ok"}}' ;;
  "module ls ") echo '[{{"name":"lez_core","status":"loaded"}}]' ;;
  "call lez_core get_sequencer_addr") echo '{{"result":"{addr}","status":"ok"}}' ;;
  *) echo '{{"status":"ok"}}' ;;
esac"#
            ),
        )
    }

    #[test]
    fn a_new_chain_closes_an_open_wallet_before_forgetting_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("s.json");
        std::fs::write(&storage, SYNCED_STORAGE).unwrap();
        running_lez_core_on(dir.path(), "http://127.0.0.1:3055/")
            .forget_chain(&storage)
            .unwrap();
        assert_eq!(
            calls(dir.path()),
            "--json daemon status\n\
             --json module ls\n\
             --json call lez_core get_sequencer_addr\n\
             --json module reload lez_core\n"
        );
        let stored: Value =
            serde_json::from_str(&std::fs::read_to_string(&storage).unwrap()).unwrap();
        assert_eq!(stored["last_synced_block"], 0);
    }

    #[test]
    fn a_new_chain_forgets_the_old_one_in_a_stopped_session_too() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("s.json");
        std::fs::write(&storage, SYNCED_STORAGE).unwrap();
        // No session directory: its daemon doesn't run, so nothing is asked of logosctl.
        let ctl = lez_core_on(dir.path(), "");
        ctl.forget_chain(&storage).unwrap();
        assert_eq!(calls(dir.path()), "");
        let stored: Value =
            serde_json::from_str(&std::fs::read_to_string(&storage).unwrap()).unwrap();
        assert_eq!(stored["last_synced_block"], 0);
        // A wallet lez_core hasn't written yet has nothing to forget.
        let missing = dir.path().join("none.json");
        ctl.forget_chain(&missing).unwrap();
        assert!(!missing.exists());
    }

    #[test]
    fn a_failed_wallet_creation_never_shows_the_recovery_phrase() {
        let dir = tempfile::tempdir().unwrap();
        // Prints the phrase, then fails.
        let ctl = fake_logosctl(dir.path(), &format!("echo '{PHRASE}'\nexit 3"));
        let err = ctl
            .create_wallet(Path::new("/w/c.json"), Path::new("/w/s.json"))
            .unwrap_err();
        assert!(!format!("{err:#}").contains(PHRASE), "{err:#}");
        // Answers with the phrase in a non-ok reply.
        let ctl = fake_logosctl(
            dir.path(),
            &format!(r#"echo '{{"status":"error","message":"{PHRASE}"}}'"#),
        );
        let err = ctl
            .create_wallet(Path::new("/w/c.json"), Path::new("/w/s.json"))
            .unwrap_err();
        assert!(!format!("{err:#}").contains(PHRASE), "{err:#}");
    }

    #[test]
    fn loads_only_modules_that_are_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = fake_logosctl(
            dir.path(),
            r#"case "$2 $3" in
  "module ls") echo '[{"name":"lez_core","status":"loaded"},{"name":"chat_module","status":"not_loaded"}]' ;;
  *) echo '{"status":"ok"}' ;;
esac"#,
        );
        ctl.load(&["lez_core".into(), "chat_module".into()])
            .unwrap();
        assert_eq!(
            calls(dir.path()),
            "--json module ls\n--json module load chat_module\n"
        );
    }

    #[test]
    fn says_when_logosctl_is_missing() {
        let ctl = Logosctl {
            program: "/nonexistent/logosctl".into(),
            dir: "/tmp/x".into(),
        };
        let err = ctl.run(&["module", "ls"]).unwrap_err();
        assert!(format!("{err:#}").contains("need logosctl"), "{err:#}");
    }

    #[test]
    fn checks_session_names() {
        assert!(check_session_name("alice").is_ok());
        assert!(check_session_name("agent-2").is_ok());
        for bad in ["", "Alice", "2nd", "a b", "../x", "a/b", &"a".repeat(49)] {
            assert!(check_session_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn always_installs_lez_core_first_and_once() {
        let modules = modules_with(&[
            "chat_module".into(),
            "lez_core".into(),
            "chat_module".into(),
        ])
        .unwrap();
        assert_eq!(modules, ["lez_core", "chat_module"]);
        assert!(modules_with(&["bad name".into()]).is_err());
        assert!(modules_with(&["--yes".into()]).is_err(), "never a flag");
    }

    #[test]
    fn refuses_a_flag_smuggled_into_session_json() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path(), "alice").unwrap();
        std::fs::create_dir_all(session.dir()).unwrap();
        let text = format!(r#"{{"modules":["lez_core","--force"],"account":"{ID}","funded":1}}"#);
        std::fs::write(session.info_file(), text).unwrap();
        assert!(session.info().is_err());
    }

    #[test]
    fn reads_call_results_and_errors() {
        // As logosctl --json printed them.
        let ok = json!({"method":"create_account_public","module":"lez_core","result":"c098","status":"ok"});
        assert_eq!(call_result(ok, "x").unwrap(), json!("c098"));
        let err = json!({"code":"MODULE_NOT_LOADED","message":"Module 'lez_core' is not loaded.","status":"error"});
        assert!(
            call_result(err, "x")
                .unwrap_err()
                .to_string()
                .contains("not loaded")
        );
    }

    #[test]
    fn takes_the_last_json_line() {
        let out = "warning: unsigned package\n{\"success\":true}\n";
        assert_eq!(last_json(out).unwrap(), json!({"success": true}));
        assert!(last_json("no json here").is_err());
    }

    #[test]
    fn vars_name_what_an_app_needs() {
        let session = Session::new(Path::new("/h/it's here/sessions"), "alice").unwrap();
        let info = SessionInfo {
            modules: vec!["lez_core".into()],
            account: PublicId::parse(ID).unwrap(),
            funded: 1,
        };
        let vars = session.vars(&info);
        assert_eq!(
            vars[0],
            (
                "LOGOSCTL_CONFIG_DIR",
                "/h/it's here/sessions/alice/logosctl".into()
            )
        );
        assert_eq!(vars[3], ("LOGOS_DEVNET_ACCOUNT", format!("Public/{ID}")));
        assert!(vars[2].1.ends_with("wallet/storage.json"));
    }

    #[test]
    fn lists_sessions_by_name_and_skips_strays() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["bob", "alice", "Not-A-Session"] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("stray-file"), "").unwrap();
        let names: Vec<_> = list(dir.path())
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["alice", "bob"]);
        assert!(list(&dir.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn session_info_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path(), "alice").unwrap();
        std::fs::create_dir_all(session.dir()).unwrap();
        let info = SessionInfo {
            modules: vec!["lez_core".into(), "chat_module".into()],
            account: PublicId::parse(ID).unwrap(),
            funded: 5_000_000,
        };
        session.save_info(&info).unwrap();
        assert_eq!(session.info().unwrap(), info);
    }
}
