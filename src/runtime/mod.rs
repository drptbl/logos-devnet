//! Where the sequencer and the LEZ `wallet` run: in Docker (the default, from our image) or
//! natively (built from the pinned LEZ tag). The rest of the CLI only talks to `Runtime`.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

mod docker;
mod native;

pub use docker::Docker;
pub use native::Native;

/// The cargo features the sequencer is built with: no Bedrock (`standalone`), and guests run
/// in-process (`prove`) so no `r0vm` is needed. Shared by the image and native builds.
pub const SEQUENCER_FEATURES: &str = "standalone,prove";
/// Where both runtimes get LEZ from.
pub const LEZ_REPO: &str = "https://github.com/logos-blockchain/logos-execution-zone.git";
/// The sequencer's log filter in both runtimes.
const SEQUENCER_LOG: &str = "info,kameo=warn";

/// An argument to a wallet command. Files are passed separately so a runtime can make them
/// visible to the wallet (Docker mounts them).
pub enum WalletArg {
    Text(String),
    File(PathBuf),
}

impl From<&str> for WalletArg {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<String> for WalletArg {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

pub trait Runtime {
    /// What runs, for `status`.
    fn describe(&self) -> String;
    /// Makes the sequencer and wallet available: pulls or builds the image, or clones and
    /// builds natively. `rebuild` builds even when they already exist.
    fn prepare(&self, rebuild: bool) -> Result<()>;
    /// Upstream's debug sequencer config for the pinned LEZ tag, to patch.
    fn upstream_config(&self) -> Result<Value>;
    /// The chain directory as the sequencer sees it.
    fn chain_home(&self) -> String;
    /// The sequencer's RPC URL as the dev wallet sees it.
    fn wallet_sequencer_url(&self) -> String;
    /// Starts the sequencer on the config in the chain directory. Returns once it is launched,
    /// not once it serves.
    fn start(&self) -> Result<()>;
    /// Stops the sequencer if it runs.
    fn stop(&self) -> Result<()>;
    /// Whether the sequencer runs. False when the runtime can't be asked (Docker is down).
    fn running(&self) -> bool;
    /// Runs the LEZ `wallet` on the dev wallet and returns everything it printed: it writes
    /// some results to stdout and some to stderr. Fails when the wallet does.
    fn wallet(&self, args: Vec<WalletArg>) -> Result<String>;
    /// Prints the sequencer's log.
    fn logs(&self, follow: bool) -> Result<()>;
}

/// Runs `cmd` with stdin closed and returns its stdout. On failure the error carries both
/// streams.
pub(crate) fn stdout_of(cmd: &mut Command) -> Result<String> {
    let output = run(cmd)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs `cmd` with stdin closed and returns stdout and stderr together.
pub(crate) fn all_output_of(cmd: &mut Command) -> Result<String> {
    run(cmd).map(|output| combined(&output))
}

fn run(cmd: &mut Command) -> Result<Output> {
    let output = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("could not run {:?}", cmd.get_program()))?;
    if !output.status.success() {
        bail!(
            "{:?} failed ({}):\n{}",
            cmd.get_program(),
            output.status,
            combined(&output).trim_end()
        );
    }
    Ok(output)
}

/// Runs `cmd` with the terminal attached, for long builds and logs.
pub(crate) fn run_attached(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("could not run {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} failed ({status})", cmd.get_program());
    }
    Ok(())
}

fn combined(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_stdout_apart_from_stderr() {
        let out = stdout_of(Command::new("sh").args(["-c", "echo out; echo warn >&2"])).unwrap();
        assert_eq!(out, "out\n");
        let all = all_output_of(Command::new("sh").args(["-c", "echo out; echo err >&2"])).unwrap();
        assert_eq!(all, "out\nerr\n");
    }

    #[test]
    fn a_failure_carries_both_streams() {
        let err = stdout_of(Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("out") && err.contains("err"), "{err}");
    }

    #[test]
    fn the_image_and_native_builds_agree() {
        let dockerfile = docker::DOCKERFILE;
        assert!(dockerfile.contains(LEZ_REPO));
        assert!(dockerfile.contains(&format!("FEATURES={SEQUENCER_FEATURES}")));
    }
}
