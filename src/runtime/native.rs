//! The native runtime: LEZ cloned at the pinned tag into `<home>/native/<tag>`, built with
//! cargo, and the sequencer run as a background process with a pid file. For machines without
//! Docker. Needs git, a Rust toolchain and the LEZ build dependencies (clang, pkg-config,
//! OpenSSL); the tag's `rust-toolchain.toml` picks the compiler.

use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::{
    LEZ_REPO, Runtime, SEQUENCER_FEATURES, SEQUENCER_LOG, WalletArg, all_output_of, batch_script,
    redact_recovery_phrase, redacted, run_attached, split_batch, stdout_of,
};
use crate::home::{Home, Settings};

pub struct Native {
    home: Home,
    settings: Settings,
}

impl Native {
    pub fn new(home: Home, settings: Settings) -> Self {
        Self { home, settings }
    }

    fn tag_dir(&self) -> PathBuf {
        self.home.native_dir().join(&self.settings.lez)
    }
    fn src(&self) -> PathBuf {
        self.tag_dir().join("src")
    }
    fn bin(&self, name: &str) -> PathBuf {
        self.tag_dir().join("target/release").join(name)
    }
    fn pid_file(&self) -> PathBuf {
        self.home.native_dir().join("sequencer.pid")
    }
    fn log_file(&self) -> PathBuf {
        self.home.native_dir().join("sequencer.log")
    }

    /// The pid in the pid file, if that process is still our sequencer. A pid the OS has
    /// handed to something else since doesn't count, even one whose arguments name our binary.
    fn live_pid(&self) -> Option<u32> {
        let pid: u32 = std::fs::read_to_string(self.pid_file())
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let command = Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let command = String::from_utf8_lossy(&command.stdout);
        runs(command.trim_end(), &self.bin("sequencer_service")).then_some(pid)
    }

    fn clone_source(&self) -> Result<()> {
        let src = self.src();
        if src.join(".git").exists() {
            let tag = stdout_of(Command::new("git").arg("-C").arg(&src).args([
                "describe",
                "--tags",
                "--exact-match",
            ]))
            .unwrap_or_default();
            if tag.trim() == self.settings.lez {
                return Ok(());
            }
            std::fs::remove_dir_all(&src)?;
        }
        std::fs::create_dir_all(self.tag_dir())?;
        eprintln!("Cloning LEZ {}...", self.settings.lez);
        run_attached(
            Command::new("git")
                .args([
                    "clone",
                    "--quiet",
                    "--depth",
                    "1",
                    "--branch",
                    &self.settings.lez,
                    LEZ_REPO,
                ])
                .arg(&src),
        )
    }
}

impl Runtime for Native {
    fn describe(&self) -> String {
        format!("native, {}", self.tag_dir().display())
    }

    fn prepare(&self, rebuild: bool) -> Result<()> {
        self.clone_source()?;
        if !rebuild && self.bin("sequencer_service").exists() && self.bin("wallet").exists() {
            return Ok(());
        }
        eprintln!(
            "Building LEZ {} (the first build takes a few minutes)...",
            self.settings.lez
        );
        let cargo = |package: &str| {
            let mut cmd = Command::new("cargo");
            cmd.current_dir(self.src())
                .env("CARGO_TARGET_DIR", self.tag_dir().join("target"))
                .args(["build", "--release", "-p", package]);
            cmd
        };
        // Two builds: in one, `prove` would reach the wallet too, which then links the prover
        // kernels that RISC0_SKIP_BUILD_KERNELS leaves out.
        run_attached(
            cargo("sequencer_service")
                .env("RISC0_SKIP_BUILD_KERNELS", "1")
                .args(["--features", SEQUENCER_FEATURES]),
        )?;
        run_attached(&mut cargo("wallet"))
    }

    fn upstream_config(&self) -> Result<Value> {
        let path = find_debug_config(&self.src()).context(
            "the LEZ checkout has no sequencer/service/configs/debug/sequencer_config.json",
        )?;
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))
    }

    fn chain_home(&self) -> String {
        self.home.chain_dir().to_string_lossy().into_owned()
    }

    fn wipe_chain(&self) -> Result<()> {
        crate::home::empty_dir(&self.home.chain_dir()).context("deleting the chain")
    }

    fn wallet_sequencer_url(&self) -> String {
        self.settings.sequencer_url()
    }

    fn start(&self) -> Result<()> {
        if self.live_pid().is_some() {
            return Ok(());
        }
        std::fs::create_dir_all(self.home.native_dir())?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_file())?;
        let child = Command::new(self.bin("sequencer_service"))
            .arg(self.home.chain_config())
            .args(["--listen-address", "127.0.0.1", "--port"])
            .arg(self.settings.port.to_string())
            .current_dir(self.home.chain_dir())
            .env("RUST_LOG", SEQUENCER_LOG)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Its own process group, so a Ctrl-C in this terminal doesn't reach it.
            .process_group(0)
            .spawn()
            .context("could not start sequencer_service")?;
        std::fs::write(self.pid_file(), child.id().to_string())?;
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        let Some(pid) = self.live_pid() else {
            let _ = std::fs::remove_file(self.pid_file());
            return Ok(());
        };
        stdout_of(Command::new("kill").args(["-TERM", &pid.to_string()]))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.live_pid().is_some() {
            if Instant::now() > deadline {
                bail!("the sequencer (pid {pid}) did not stop within 30s");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = std::fs::remove_file(self.pid_file());
        Ok(())
    }

    fn running(&self) -> bool {
        self.live_pid().is_some()
    }

    fn wallet(&self, home: &Path, args: Vec<WalletArg>) -> Result<String> {
        let args = args.into_iter().map(|arg| match arg {
            WalletArg::Text(text) => text.into(),
            WalletArg::File(path) => path.into_os_string(),
        });
        all_output_of(
            Command::new(self.bin("wallet"))
                .env("LEE_WALLET_HOME_DIR", home)
                .args(args),
        )
        .map(|out| redact_recovery_phrase(&out))
        .map_err(redacted)
    }

    fn wallet_batch(&self, home: &Path, runs: &[Vec<String>]) -> Result<Vec<String>> {
        let script = batch_script(&self.bin("wallet").to_string_lossy(), runs);
        let out = all_output_of(
            Command::new("sh")
                .env("LEE_WALLET_HOME_DIR", home)
                .arg("-c")
                .arg(script),
        )
        .map_err(redacted)?;
        split_batch(&out, runs.len())
    }

    fn logs(&self, follow: bool) -> Result<()> {
        let mut cmd = Command::new("tail");
        cmd.args(["-n", "200"]);
        if follow {
            cmd.arg("-f");
        }
        run_attached(cmd.arg(self.log_file()))
    }
}

/// Whether a process's command line (as `ps -o command=` prints it) runs `bin`: it starts
/// with that path, then ends or goes on to the arguments.
fn runs(command: &str, bin: &Path) -> bool {
    command
        .strip_prefix(&*bin.to_string_lossy())
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// Upstream's debug sequencer config, wherever this tag keeps it.
fn find_debug_config(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let file = dir.join("sequencer_config.json");
        if dir.ends_with("sequencer/service/configs/debug") && file.exists() {
            return Some(file);
        }
        if depth >= 6 {
            continue;
        }
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let name = entry.file_name();
            if entry.path().is_dir() && name != "target" && name != ".git" {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_only_the_sequencer_binary_itself() {
        let bin = Path::new("/h/native/v0.3.1/target/release/sequencer_service");
        assert!(runs(
            "/h/native/v0.3.1/target/release/sequencer_service /h/chain/c.json --port 3040",
            bin
        ));
        assert!(runs(
            "/h/native/v0.3.1/target/release/sequencer_service",
            bin
        ));
        for other in [
            "lldb /h/native/v0.3.1/target/release/sequencer_service",
            "vim /h/native/v0.3.1/target/release/sequencer_service.rs",
            "/h/native/v0.3.1/target/release/sequencer_service_old --port 1",
        ] {
            assert!(!runs(other, bin), "{other}");
        }
    }

    #[test]
    fn a_reused_pid_is_not_our_sequencer() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::locate(Some(dir.path().into())).unwrap();
        let native = Native::new(home.clone(), Settings::default());
        assert!(!native.running(), "no pid file");
        std::fs::create_dir_all(home.native_dir()).unwrap();
        // A live process that isn't the sequencer: this test.
        std::fs::write(native.pid_file(), std::process::id().to_string()).unwrap();
        assert!(!native.running());
        native.stop().unwrap();
        assert!(!native.pid_file().exists(), "a stale pid file is cleared");
    }

    #[test]
    fn finds_the_debug_config_in_either_layout() {
        for layout in ["lez/sequencer/service", "sequencer/service"] {
            let dir = tempfile::tempdir().unwrap();
            let debug = dir.path().join(layout).join("configs/debug");
            std::fs::create_dir_all(&debug).unwrap();
            std::fs::write(debug.join("sequencer_config.json"), "{}").unwrap();
            let found = find_debug_config(dir.path()).unwrap();
            assert_eq!(found, debug.join("sequencer_config.json"), "{layout}");
        }
    }

    #[test]
    fn ignores_build_output() {
        let dir = tempfile::tempdir().unwrap();
        let debug = dir.path().join("target/sequencer/service/configs/debug");
        std::fs::create_dir_all(&debug).unwrap();
        std::fs::write(debug.join("sequencer_config.json"), "{}").unwrap();
        assert!(find_debug_config(dir.path()).is_none());
    }
}
