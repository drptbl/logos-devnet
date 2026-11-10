//! Sessions: throwaway `logosctl` sessions wired to the devnet, so an app (or two agents) can
//! run against it with a funded wallet in two commands.
//!
//! ```text
//! sessions/<name>/logosctl/      the session's LOGOSCTL_CONFIG_DIR: its daemon, modules, data
//! sessions/<name>/wallet/        wallet_config.json pointing at the devnet, and storage.json
//! sessions/<name>/session.json   the modules to load and the funded account
//! ```
//!
//! Modules come from the official catalog. `lez_core` is always installed, and `create_new`
//! leaves the session's wallet open in its daemon.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::account::PublicId;
use crate::home::write_json;
use crate::runtime::stdout_of;

const LEZ_CORE: &str = "lez_core";

/// What a session remembers about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Every module installed, `lez_core` first; `up` loads them again after a restart.
    pub modules: Vec<String>,
    /// The wallet's public account, funded from the faucet.
    pub account: PublicId,
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

    pub fn info(&self) -> Result<SessionInfo> {
        let text = std::fs::read_to_string(self.info_file())
            .with_context(|| format!("session {} is incomplete: no session.json", self.name))?;
        serde_json::from_str(&text)
            .with_context(|| format!("reading {}", self.info_file().display()))
    }

    pub fn save_info(&self, info: &SessionInfo) -> Result<()> {
        write_json(&self.info_file(), &serde_json::to_value(info)?)
    }

    pub fn logosctl(&self) -> Logosctl {
        Logosctl {
            dir: self.logosctl_dir(),
        }
    }

    /// The session's export lines, values single-quoted so any path survives `source`.
    pub fn env_lines(&self, info: &SessionInfo) -> String {
        [
            (
                "LOGOSCTL_CONFIG_DIR",
                self.logosctl_dir().to_string_lossy().into_owned(),
            ),
            (
                "LOGOS_DEVNET_WALLET_CONFIG",
                self.wallet_config().to_string_lossy().into_owned(),
            ),
            (
                "LOGOS_DEVNET_WALLET_STORAGE",
                self.wallet_storage().to_string_lossy().into_owned(),
            ),
            ("LOGOS_DEVNET_ACCOUNT", info.account.wallet_form()),
        ]
        .iter()
        .map(|(var, value)| format!("export {var}={}\n", sh_quote(value)))
        .collect()
    }
}

/// The sessions under `dir`, by name.
pub fn list(dir: &Path) -> Result<Vec<Session>> {
    let mut sessions = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in entries.flatten() {
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
        // Starting with a letter or digit, so a name can't reach logosctl as a flag.
        let ok = module
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
            && module
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !ok {
            bail!("not a module name: {module:?}");
        }
        if !modules.contains(module) {
            modules.push(module.clone());
        }
    }
    Ok(modules)
}

/// Session names are lower-case letters, digits and `-`, starting with a letter: they become
/// directory names.
pub fn check_session_name(name: &str) -> Result<()> {
    let ok = name.len() <= 32
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!(
            "session names are up to 32 lower-case letters, digits and -, starting with a letter: {name}"
        );
    }
    Ok(())
}

/// The Logos Core CLI for one session directory.
pub struct Logosctl {
    dir: PathBuf,
}

impl Logosctl {
    fn cmd(&self) -> Command {
        let mut cmd = Command::new("logosctl");
        cmd.env("LOGOSCTL_CONFIG_DIR", &self.dir).arg("--json");
        cmd
    }

    /// Runs logosctl and returns its stdout. Fails when it exits non-zero.
    fn output(&self, args: &[&str]) -> Result<String> {
        stdout_of(self.cmd().args(args)).map_err(|e| {
            if e.to_string().contains("could not run") {
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

    pub fn stop(&self) -> Result<()> {
        if self.running() {
            self.output(&["daemon", "stop"])?;
        }
        Ok(())
    }

    /// Whether the session's daemon answers.
    pub fn running(&self) -> bool {
        self.dir.exists()
            && self
                .cmd()
                .args(["daemon", "status"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
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

    pub fn load(&self, modules: &[String]) -> Result<()> {
        for module in modules {
            self.run(&["module", "load", module])
                .with_context(|| format!("loading {module}"))?;
        }
        Ok(())
    }

    /// Calls `method` on `module` with string arguments and returns its result.
    pub fn call(&self, module: &str, method: &str, args: &[String]) -> Result<Value> {
        let mut argv = vec!["call", module, method];
        let args: Vec<String> = args.iter().map(|a| format!("str:{a}")).collect();
        argv.extend(args.iter().map(String::as_str));
        call_result(self.run(&argv)?, &format!("{module}.{method}"))
    }
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

/// `value` in single quotes for a POSIX shell.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d";

    #[test]
    fn checks_session_names() {
        assert!(check_session_name("alice").is_ok());
        assert!(check_session_name("agent-2").is_ok());
        for bad in ["", "Alice", "2nd", "a b", "../x", "a/b", &"a".repeat(33)] {
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
    fn env_lines_quote_paths() {
        let session = Session::new(Path::new("/h/it's here/sessions"), "alice").unwrap();
        let info = SessionInfo {
            modules: vec!["lez_core".into()],
            account: PublicId::parse(ID).unwrap(),
        };
        let lines = session.env_lines(&info);
        assert!(
            lines.contains(r"export LOGOSCTL_CONFIG_DIR='/h/it'\''s here/sessions/alice/logosctl'")
        );
        assert!(lines.contains(&format!("export LOGOS_DEVNET_ACCOUNT='Public/{ID}'")));
        assert!(lines.contains("wallet/storage.json'"));
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
        };
        session.save_info(&info).unwrap();
        assert_eq!(session.info().unwrap(), info);
    }
}
