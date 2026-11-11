//! Where the sequencer and the LEZ `wallet` run: in Docker (the default, from our image) or
//! natively (built from the pinned LEZ tag). The rest of the CLI only talks to `Runtime`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

mod docker;
mod native;

pub use docker::Docker;
pub(crate) use docker::{START_DOCKER, docker_reachable, image_present};
pub use native::Native;

/// The cargo features the sequencer is built with: no Bedrock (`standalone`), and guests run
/// in-process (`prove`) so no `r0vm` is needed. Shared by the image and native builds.
pub const SEQUENCER_FEATURES: &str = "standalone,prove";
/// Where both runtimes get LEZ from.
pub const LEZ_REPO: &str = "https://github.com/logos-blockchain/logos-execution-zone.git";
/// The sequencer's log filter in both runtimes.
const SEQUENCER_LOG: &str = "info,kameo=warn";

/// The sequencer's environment: its log filter, and whether it takes fake proofs.
fn sequencer_env(dev_mode: bool) -> [(&'static str, &'static str); 2] {
    [
        ("RUST_LOG", SEQUENCER_LOG),
        (
            crate::envfile::DEV_MODE_VAR,
            crate::envfile::dev_mode_value(dev_mode),
        ),
    ]
}

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
    /// Deletes the chain's data, keeping the chain directory. Only while stopped.
    fn wipe_chain(&self) -> Result<()>;
    /// Runs the LEZ `wallet` on the wallet in `home` and returns everything it printed (it
    /// writes some results to stdout and some to stderr), recovery phrase hidden. Fails when
    /// the wallet does. `home` must hold a `wallet_config.json` pointing at the devnet.
    fn wallet(&self, home: &Path, args: Vec<WalletArg>) -> Result<String>;
    /// Fails when `wallet` couldn't read `path` as a `WalletArg::File`, e.g. a program binary.
    fn check_wallet_file(&self, _path: &Path) -> Result<()> {
        Ok(())
    }
    /// Runs several wallet commands with text arguments on the wallet in `home` in one go (one
    /// container under Docker), and returns each one's output in order, recovery phrase hidden.
    fn wallet_batch(&self, home: &Path, runs: &[Vec<String>]) -> Result<Vec<String>>;
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

/// Runs a long build with its output on stderr: stdout is for results, JSON with --json.
pub(crate) fn run_building(cmd: &mut Command) -> Result<()> {
    run_attached(cmd.stdout(std::io::stderr()))
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

const RUN_MARK: &str = "@@logos-devnet-wallet-run@@";
const FAIL_MARK: &str = "@@logos-devnet-wallet-failed@@";

/// A POSIX shell script that runs `wallet` (a command or path) once per run, each run's
/// output after a marker line, and a failure marker after a run that fails.
pub(crate) fn batch_script(wallet: &str, runs: &[Vec<String>]) -> String {
    let mut script = String::new();
    for run in runs {
        let args: Vec<String> = run.iter().map(|a| sh_quote(a)).collect();
        // Each marker starts on a line of its own, even after output without a final newline.
        script += &format!(
            "printf '\\n%s\\n' '{RUN_MARK}'; {} {} </dev/null 2>&1 || printf '\\n%s\\n' '{FAIL_MARK}'\n",
            sh_quote(wallet),
            args.join(" ")
        );
    }
    script
}

/// How many wallet runs go into one batch script: it travels as a single `sh -c` argument,
/// which Linux caps at 128 KiB, and a run takes about 150 bytes.
pub(crate) const BATCH_CHUNK: usize = 200;

/// Splits a batch's output into one output per run, recovery phrase hidden. Fails naming the
/// first run that failed.
pub(crate) fn split_batch(output: &str, runs: usize) -> Result<Vec<String>> {
    let mut outputs: Vec<String> = Vec::new();
    for line in output.lines() {
        if line == RUN_MARK {
            outputs.push(String::new());
        } else if let Some(current) = outputs.last_mut() {
            current.push_str(line);
            current.push('\n');
        }
    }
    if let Some(i) = outputs
        .iter()
        .position(|o| o.lines().any(|l| l == FAIL_MARK))
    {
        bail!(
            "wallet command {} of {runs} failed:\n{}",
            i + 1,
            redact_recovery_phrase(outputs[i].trim_end())
        );
    }
    if outputs.len() != runs {
        bail!("the wallet batch ran {} of {runs} commands", outputs.len());
    }
    // Each marker is printed after a newline of its own, so drop the blank line it leaves.
    Ok(outputs
        .iter()
        .map(|o| {
            let o = o.trim_end_matches('\n');
            redact_recovery_phrase(&if o.is_empty() {
                String::new()
            } else {
                format!("{o}\n")
            })
        })
        .collect())
}

/// `text` with the wallet's recovery phrase hidden: the wallet prints it once, the first time
/// it creates a wallet, on the line after "Recovery phrase:".
pub(crate) fn redact_recovery_phrase(text: &str) -> String {
    const MARK: &str = "Recovery phrase:";
    let mut out = String::with_capacity(text.len());
    // After the marker, every line up to the next blank one is the phrase.
    let mut hiding = false;
    for line in text.split_inclusive('\n') {
        if hiding {
            if line.trim().is_empty() {
                hiding = false;
                out.push_str(line);
            }
            continue;
        }
        if let Some(at) = line.find(MARK) {
            out.push_str(&line[..at + MARK.len()]);
            out.push_str(" <hidden>\n");
            hiding = true;
            continue;
        }
        out.push_str(line);
    }
    out
}

/// A wallet error with the recovery phrase hidden from the output it carries.
pub(crate) fn redacted(e: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(redact_recovery_phrase(&format!("{e:#}")))
}

/// `value` in single quotes for a POSIX shell.
pub(crate) fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
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
    fn hides_the_recovery_phrase() {
        let out = "Persistent storage not found\nRecovery phrase:\n  amused river fossil\n\nGenerated new account with account_id Public/X\n";
        let hidden = redact_recovery_phrase(out);
        assert!(!hidden.contains("amused"), "{hidden}");
        assert!(hidden.contains("Generated new account with account_id Public/X"));
        // On the marker's own line, or over several lines.
        for out in [
            "Recovery phrase: amused river fossil\n",
            "Recovery phrase:\n  amused river\n  fossil words\n\nnext\n",
        ] {
            let hidden = redact_recovery_phrase(out);
            assert!(
                !hidden.contains("amused") && !hidden.contains("fossil"),
                "{hidden}"
            );
        }
        assert_eq!(
            redact_recovery_phrase("nothing secret\n"),
            "nothing secret\n"
        );
    }

    #[test]
    fn runs_a_batch_and_splits_its_output() {
        let runs = vec![
            vec!["account".to_owned(), "new".to_owned(), "public".to_owned()],
            vec!["it's".to_owned()],
        ];
        // `echo` stands in for the wallet: each run prints its arguments.
        let script = batch_script("echo", &runs);
        let out = stdout_of(Command::new("sh").arg("-c").arg(&script)).unwrap();
        let outputs = split_batch(&out, 2).unwrap();
        assert_eq!(outputs, ["account new public\n", "it's\n"]);
    }

    #[test]
    fn a_run_without_a_final_newline_doesnt_hide_the_next_marker() {
        // `printf x` ends without a newline, then fails.
        let runs = [
            vec!["-c".to_owned(), "printf x; exit 1".to_owned()],
            vec!["-c".to_owned(), "printf y".to_owned()],
        ];
        let out = stdout_of(Command::new("sh").arg("-c").arg(batch_script("sh", &runs))).unwrap();
        let err = split_batch(&out, 2).unwrap_err().to_string();
        assert!(err.contains("command 1 of 2"), "{err}");
    }

    #[test]
    fn a_full_chunk_fits_one_linux_argument() {
        let run = vec!["account".to_owned(), "new".to_owned(), "public".to_owned()];
        let script = batch_script("/a/long/path/to/the/wallet/binary", &vec![run; BATCH_CHUNK]);
        assert!(script.len() < 128 * 1024 / 2, "{} bytes", script.len());
    }

    #[test]
    fn a_failed_batch_run_is_named_and_redacted() {
        // `sh -c` stands in for the wallet: the second run prints a phrase and fails.
        let runs = [
            vec!["-c".to_owned(), "true".to_owned()],
            vec![
                "-c".to_owned(),
                "echo 'Recovery phrase:'; echo secret words; exit 3".to_owned(),
            ],
        ];
        let script = batch_script("sh", &runs);
        let out = stdout_of(Command::new("sh").arg("-c").arg(&script)).unwrap();
        let err = split_batch(&out, runs.len()).unwrap_err().to_string();
        assert!(err.contains("command 2 of 2"), "{err}");
        assert!(!err.contains("secret words"), "{err}");
    }

    #[test]
    fn the_image_and_native_builds_agree() {
        let dockerfile = docker::DOCKERFILE;
        assert!(dockerfile.contains(LEZ_REPO));
        assert!(dockerfile.contains(&format!("FEATURES={SEQUENCER_FEATURES}")));
    }
}
