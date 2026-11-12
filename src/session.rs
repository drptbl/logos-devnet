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

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::account::PublicId;
use crate::envfile::{DEV_MODE_VAR, dev_mode_value};
use crate::home::{read_dir_if_present, write_json};
use crate::runtime::{stdout_of, stdout_within};
use crate::wallet_storage;

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
        self.wallet_dir().join(wallet_storage::STORAGE_FILE)
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

    /// The dev mode the session's daemon was last started with.
    fn dev_mode_record(&self) -> PathBuf {
        self.dir.join("dev-mode")
    }

    /// Starts the session's daemon (`ctl`) with the chain's dev mode. One running with the
    /// other mode, or with none recorded, is restarted first: it would make proofs the chain
    /// rejects.
    pub fn start_daemon(&self, ctl: &Logosctl, dev_mode: bool) -> Result<()> {
        let record = self.dev_mode_record();
        let value = dev_mode_value(dev_mode);
        let started = std::fs::read_to_string(&record).ok();
        if started.as_deref().map(str::trim) != Some(value) {
            ctl.stop()?;
        }
        ctl.start(dev_mode)?;
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&record, value).with_context(|| format!("writing {}", record.display()))
    }

    /// Marks the session as on a new chain. `forget_old_chain` clears it.
    pub fn mark_new_chain(&self) -> Result<()> {
        wallet_storage::mark_new_chain(&self.dir)
    }

    /// Makes the session's wallet forget the old chain, if the session is marked as on a new
    /// one.
    pub fn forget_old_chain(&self) -> Result<()> {
        if !wallet_storage::marked_new_chain(&self.dir) {
            return Ok(());
        }
        self.logosctl().forget_chain(&self.wallet_storage())?;
        wallet_storage::clear_new_chain(&self.dir)
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

/// The modules `extra` adds (as `modules_with` takes them) that a session with `have` lacks, in
/// order. A module left out of `extra` is not removed.
pub fn missing_modules(have: &[String], extra: &[String]) -> Result<Vec<String>> {
    Ok(modules_with(extra)?
        .into_iter()
        .filter(|module| !have.contains(module))
        .collect())
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
        self.output_of(self.cmd().args(args))
    }

    /// `output` for a command built from `cmd`.
    fn output_of(&self, cmd: &mut Command) -> Result<String> {
        stdout_of(cmd).map_err(install_hint)
    }

    /// Runs a logosctl command that answers in JSON.
    fn run(&self, args: &[&str]) -> Result<Value> {
        last_json(&self.output(args)?)
    }

    /// Starts the session's daemon and returns once it takes commands. (`daemon start`
    /// answers in plain text even with `--json`.)
    pub fn start(&self, dev_mode: bool) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        if !self.running() {
            // The daemon's modules prove in its process, so it takes the chain's dev mode.
            self.output_of(
                self.cmd()
                    .env(DEV_MODE_VAR, dev_mode_value(dev_mode))
                    .args(["daemon", "start", "--detach"]),
            )?;
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
        let argv = call_argv(module, method, args);
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        call_result(self.run(&argv)?, &format!("{module}.{method}"))
    }

    /// `call`, giving up after `timeout`, for a caller that must not hang on a stuck daemon.
    pub fn call_within(
        &self,
        module: &str,
        method: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Value> {
        let out = stdout_within(self.cmd().args(call_argv(module, method, args)), timeout)
            .map_err(install_hint)?;
        call_result(last_json(&out)?, &format!("{module}.{method}"))
    }

    /// Whether the daemon answers within `timeout`.
    pub fn answers_within(&self, timeout: Duration) -> bool {
        self.dir.exists() && stdout_within(self.cmd().args(["daemon", "status"]), timeout).is_ok()
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

    /// Makes the session's wallet (`storage`) forget the chain it synced, for a new one. The
    /// daemon is stopped first: then nothing can write `lez_core`'s copy of the old chain back
    /// over the file, and it starts again with the new chain's settings.
    pub fn forget_chain(&self, storage: &Path) -> Result<()> {
        self.stop()?;
        wallet_storage::forget_chain_in(storage)
    }

    /// Brings a started daemon back to work: `lez_core` loaded and the wallet open first, then
    /// the session's other `modules`, so one that won't load leaves a working wallet.
    pub fn resume(
        &self,
        modules: &[String],
        config: &Path,
        storage: &Path,
        sequencer: &str,
    ) -> Result<()> {
        self.load(&[LEZ_CORE.to_owned()])?;
        self.ensure_wallet(config, storage, sequencer)?;
        self.load(modules)
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

/// `e`, saying to install logosctl when it wasn't found.
fn install_hint(e: anyhow::Error) -> anyhow::Error {
    let missing = e
        .root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
    if missing {
        e.context("sessions and the messaging hub need logosctl, the Logos Core CLI, on your PATH")
    } else {
        e
    }
}

/// `logosctl call`'s arguments, each of `args` as a string.
fn call_argv(module: &str, method: &str, args: &[&str]) -> Vec<String> {
    let mut argv = vec!["call".to_owned(), module.to_owned(), method.to_owned()];
    argv.extend(args.iter().map(|a| format!("str:{a}")));
    argv
}

/// Whether `module ls` (`listed`) shows `name` loaded.
fn module_loaded(listed: &Value, name: &str) -> bool {
    listed.as_array().is_some_and(|all| {
        all.iter()
            .any(|m| m["name"] == name && m["status"] == "loaded")
    })
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
pub(crate) mod tests {
    use super::*;
    use crate::wallet_storage::tests::SYNCED_STORAGE;
    use serde_json::json;

    const ID: &str = "HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d";
    const PHRASE: &str = "amused river fossil";

    /// A logosctl stand-in: records its arguments, one call per line, and answers per `script`.
    pub(crate) fn fake_logosctl(dir: &Path, script: &str) -> Logosctl {
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

    pub(crate) fn calls(dir: &Path) -> String {
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

    /// A logosctl whose daemon runs until `daemon stop`, and starts again on `daemon start`,
    /// noting the RISC0_DEV_MODE it started with in `started`.
    fn daemon(dir: &Path, running: bool) -> Logosctl {
        std::fs::create_dir_all(dir.join("session")).unwrap();
        let up = dir.join("up");
        if running {
            std::fs::write(&up, "").unwrap();
        }
        fake_logosctl(
            dir,
            &format!(
                r#"case "$2 $3" in
  "daemon status") [ -f '{up}' ] || exit 1; echo '{{"status":"ok"}}' ;;
  "daemon stop") rm -f '{up}' ;;
  "daemon start") echo "$RISC0_DEV_MODE" > '{started}'; touch '{up}' ;;
  *) echo '{{"status":"ok"}}' ;;
esac"#,
                up = up.display(),
                started = dir.join("started").display(),
            ),
        )
    }

    #[test]
    fn finds_the_modules_a_session_lacks() {
        let have = ["lez_core".to_owned(), "chat_module".to_owned()];
        let wanted = |extra: &[&str]| {
            let extra: Vec<String> = extra.iter().map(|m| (*m).to_owned()).collect();
            missing_modules(&have, &extra).unwrap()
        };
        assert!(wanted(&["chat_module"]).is_empty());
        assert_eq!(
            wanted(&["chat_module", "storage_module"]),
            ["storage_module"]
        );
        assert!(wanted(&[]).is_empty(), "a module left out stays installed");
        assert!(missing_modules(&have, &["Bad Name".to_owned()]).is_err());
    }

    #[test]
    fn opens_the_wallet_before_a_module_that_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = fake_logosctl(
            dir.path(),
            r#"case "$2 $3 $4" in
  "module ls ") echo '[{"name":"lez_core","status":"not_loaded"},{"name":"chat_module","status":"not_loaded"}]' ;;
  "module load chat_module") exit 1 ;;
  "call lez_core get_sequencer_addr") echo '{"result":"","status":"ok"}' ;;
  "call lez_core open") echo '{"result":0,"status":"ok"}' ;;
  *) echo '{"status":"ok"}' ;;
esac"#,
        );
        let modules = ["lez_core".to_owned(), "chat_module".to_owned()];
        let err = ctl
            .resume(
                &modules,
                Path::new("/w/c.json"),
                Path::new("/w/s.json"),
                "http://127.0.0.1:3055",
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("chat_module"), "{err:#}");
        let calls = calls(dir.path());
        let opened = calls.find("call lez_core open").expect("the wallet opened");
        let failed = calls.find("module load chat_module").unwrap();
        assert!(opened < failed, "{calls}");
    }

    #[test]
    fn a_daemon_started_in_the_other_mode_is_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(&dir.path().join("sessions"), "alice").unwrap();
        // Running from before v0.3.0: nothing says how it started.
        let ctl = daemon(dir.path(), true);
        session.start_daemon(&ctl, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("started")).unwrap(),
            "1\n"
        );
        assert!(calls(dir.path()).contains("daemon stop"));
        // Started in this mode: left running.
        std::fs::remove_file(dir.path().join("calls.log")).unwrap();
        session.start_daemon(&ctl, true).unwrap();
        assert!(!calls(dir.path()).contains("daemon stop"));
        // The other mode: restarted.
        session.start_daemon(&ctl, false).unwrap();
        assert!(calls(dir.path()).contains("daemon stop"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("started")).unwrap(),
            "0\n"
        );
    }

    #[test]
    fn a_daemon_starts_with_the_devnets_dev_mode() {
        let dir = tempfile::tempdir().unwrap();
        daemon(dir.path(), false).start(true).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("started")).unwrap(),
            "1\n"
        );
        let dir = tempfile::tempdir().unwrap();
        daemon(dir.path(), false).start(false).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("started")).unwrap(),
            "0\n"
        );
    }

    #[test]
    fn a_new_chain_stops_the_daemon_before_forgetting_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("s.json");
        std::fs::write(&storage, SYNCED_STORAGE).unwrap();
        daemon(dir.path(), true).forget_chain(&storage).unwrap();
        // Stopped, so nothing can reopen the wallet, and it restarts with the new chain's
        // settings.
        assert_eq!(
            calls(dir.path()),
            "--json daemon status\n\
             --json daemon stop\n\
             --json daemon status\n"
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
    fn a_marked_session_forgets_the_old_chain_once() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path(), "alice").unwrap();
        std::fs::create_dir_all(session.wallet_dir()).unwrap();
        let storage = session.wallet_storage();
        let synced_to = |path: &Path| -> Value {
            serde_json::from_str::<Value>(&std::fs::read_to_string(path).unwrap()).unwrap()
                ["last_synced_block"]
                .clone()
        };
        std::fs::write(&storage, SYNCED_STORAGE).unwrap();
        std::fs::set_permissions(&storage, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Not marked: nothing changes.
        session.forget_old_chain().unwrap();
        assert_eq!(synced_to(&storage), 42);
        // Marked: forgotten, with the file's permissions kept, and the mark cleared.
        session.mark_new_chain().unwrap();
        session.forget_old_chain().unwrap();
        assert_eq!(synced_to(&storage), 0);
        let mode = std::fs::metadata(&storage).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!crate::wallet_storage::marked_new_chain(session.dir()));
        assert!(!storage.with_extension("json.new").exists());
        // Once only: a later sync on the new chain stays.
        std::fs::write(&storage, SYNCED_STORAGE).unwrap();
        session.forget_old_chain().unwrap();
        assert_eq!(synced_to(&storage), 42);
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
