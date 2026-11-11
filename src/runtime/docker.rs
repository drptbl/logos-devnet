//! The Docker runtime: our image (`ghcr.io/drptbl/logos-devnet-sequencer:<lez tag>`) run with
//! Docker Compose. The chain and the dev wallet stay in the home directory, bind-mounted and
//! owned by the host user, so `reset` and inspection work the same as natively.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{
    Runtime, WalletArg, all_output_of, batch_script, redact_recovery_phrase, redacted,
    run_attached, sequencer_env, split_batch, stdout_of,
};
use crate::home::{Home, Settings, write_json};

/// The image's build recipe. Built from stdin, it needs no context: it clones LEZ itself.
pub const DOCKERFILE: &str = include_str!("../../docker/Dockerfile");

/// The sequencer's RPC port inside the container.
const CONTAINER_PORT: u16 = 3040;
const CHAIN_MOUNT: &str = "/devnet/chain";
const WALLET_MOUNT: &str = "/devnet/wallet";
const FILES_MOUNT: &str = "/devnet/files";
/// Where the image keeps upstream's debug sequencer config.
const UPSTREAM_CONFIG: &str = "/usr/share/logos-devnet/sequencer_config.json";

pub struct Docker {
    home: Home,
    settings: Settings,
}

impl Docker {
    pub fn new(home: Home, settings: Settings) -> Self {
        Self { home, settings }
    }

    fn compose(&self) -> Command {
        let mut cmd = Command::new("docker");
        cmd.arg("compose")
            .arg("--project-name")
            .arg(compose_project(&self.home))
            .arg("--file")
            .arg(self.home.compose_file());
        cmd
    }

    fn write_compose_file(&self) -> Result<()> {
        // The chain directory must exist before Docker mounts it, or it creates it as root.
        std::fs::create_dir_all(self.home.chain_dir())?;
        let spec = compose_spec(&ComposeInputs {
            project: &compose_project(&self.home),
            image: &self.settings.image(),
            port: self.settings.port,
            user: &current_user()?,
            chain_dir: &self.home.chain_dir().to_string_lossy(),
            dev_mode: self.settings.dev_mode,
            log_filter: &self.settings.log_filter,
        });
        // JSON is YAML, and serde quotes every path correctly.
        write_json(&self.home.compose_file(), &spec)
    }

    /// `compose run` of the wallet service with the wallet in `home` mounted as its home.
    fn wallet_run(&self, home: &Path) -> Result<Command> {
        self.write_compose_file()?;
        std::fs::create_dir_all(home)?;
        let home = mountable(home)?;
        let mut cmd = self.compose();
        cmd.args(["run", "--rm", "--no-deps", "-T", "--volume"])
            .arg(format!("{}:{WALLET_MOUNT}", home.display()));
        Ok(cmd)
    }

    fn build_image(&self) -> Result<()> {
        eprintln!(
            "Building {} from LEZ {} (the first build takes a while)...",
            self.settings.image(),
            self.settings.lez
        );
        let mut child = Command::new("docker")
            .args(["build", "--build-arg"])
            .arg(format!("LEZ_TAG={}", self.settings.lez))
            .arg("--tag")
            .arg(self.settings.image())
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(std::io::stderr())
            .spawn()
            .context("could not run docker")?;
        child
            .stdin
            .take()
            .context("docker build has no stdin")?
            .write_all(DOCKERFILE.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            bail!("docker build failed ({status})");
        }
        Ok(())
    }
}

impl Runtime for Docker {
    fn describe(&self) -> String {
        format!("docker, {}", self.settings.image())
    }

    fn prepare(&self, rebuild: bool) -> Result<()> {
        // Without a daemon a failed pull says nothing about the image, and a build fails too.
        if !docker_reachable() {
            bail!("no Docker daemon answers: {START_DOCKER}");
        }
        if rebuild {
            return self.build_image();
        }
        if image_present(&self.settings.image()) {
            return Ok(());
        }
        eprintln!("Pulling {}...", self.settings.image());
        let pulled = Command::new("docker")
            .arg("pull")
            .arg(self.settings.image())
            // Progress goes to stderr: stdout is for results, JSON with --json.
            .stdout(std::io::stderr())
            .status()
            .is_ok_and(|s| s.success());
        if pulled {
            return Ok(());
        }
        eprintln!(
            "No published image for LEZ {}. Building it locally.",
            self.settings.lez
        );
        self.build_image()
    }

    fn upstream_config(&self) -> Result<Value> {
        let text = stdout_of(
            Command::new("docker")
                .args(["run", "--rm", "--entrypoint", "cat"])
                .arg(self.settings.image())
                .arg(UPSTREAM_CONFIG),
        )?;
        serde_json::from_str(&text).context("the image's sequencer config is not JSON")
    }

    fn chain_home(&self) -> String {
        CHAIN_MOUNT.into()
    }

    /// Deletes through a container rather than on the host. OrbStack shows a host-side delete
    /// inside its VM late, and a sequencer started within that window found the old chain's
    /// key still listed but unreadable ("No such file or directory"): one reset in five.
    fn wipe_chain(&self) -> Result<()> {
        let chain = self.home.chain_dir();
        if !chain.exists() {
            return Ok(());
        }
        stdout_of(
            Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--user",
                    &current_user()?,
                    "--entrypoint",
                    "find",
                ])
                .arg("--volume")
                .arg(format!("{}:{CHAIN_MOUNT}", chain.display()))
                .arg(self.settings.image())
                .args([CHAIN_MOUNT, "-mindepth", "1", "-delete"]),
        )
        .map(drop)
        .context("deleting the chain")
    }

    fn wallet_sequencer_url(&self) -> String {
        format!("http://sequencer:{CONTAINER_PORT}")
    }

    fn start(&self) -> Result<()> {
        self.write_compose_file()?;
        stdout_of(self.compose().args(["up", "--detach", "sequencer"])).map(drop)
    }

    /// Takes the project down whatever state its containers are in (running, paused,
    /// restarting). Only an unreachable Docker counts as nothing to stop.
    fn stop(&self) -> Result<()> {
        if !self.home.compose_file().exists() {
            return Ok(());
        }
        match stdout_of(self.compose().args(["down", "--remove-orphans"])) {
            Err(_) if !docker_reachable() => Ok(()),
            result => result.map(drop),
        }
    }

    fn running(&self) -> bool {
        self.home.compose_file().exists()
            && stdout_of(
                self.compose()
                    .args(["ps", "--status", "running", "--quiet", "sequencer"]),
            )
            .is_ok_and(|ids| !ids.trim().is_empty())
    }

    fn wallet(&self, home: &Path, args: Vec<WalletArg>) -> Result<String> {
        let mut cmd = self.wallet_run(home)?;
        let mut wallet_args = Vec::new();
        for (i, arg) in args.into_iter().enumerate() {
            match arg {
                WalletArg::Text(text) => wallet_args.push(text),
                WalletArg::File(path) => {
                    // Mount just the file, read-only.
                    let path = mountable(&path)?;
                    let mount = format!("{FILES_MOUNT}/{i}");
                    cmd.arg("--volume")
                        .arg(format!("{}:{mount}:ro", path.display()));
                    wallet_args.push(mount);
                }
            }
        }
        cmd.arg("wallet").args(wallet_args);
        all_output_of(&mut cmd)
            .map(|out| redact_recovery_phrase(&out))
            .map_err(redacted)
    }

    fn check_wallet_file(&self, path: &Path) -> Result<()> {
        mountable(path).map(drop)
    }

    fn wallet_batch(&self, home: &Path, runs: &[Vec<String>]) -> Result<Vec<String>> {
        let mut cmd = self.wallet_run(home)?;
        cmd.args(["--entrypoint", "sh", "wallet", "-c"])
            .arg(batch_script("wallet", runs));
        let out = all_output_of(&mut cmd).map_err(redacted)?;
        split_batch(&out, runs.len())
    }

    fn logs(&self, follow: bool) -> Result<()> {
        let mut cmd = self.compose();
        cmd.args(["logs", "--tail", "200"]);
        if follow {
            cmd.arg("--follow");
        }
        run_attached(cmd.arg("sequencer"))
    }
}

/// `logos-devnet` for the default home, and a name derived from the path for any other, so
/// two homes never share containers.
fn compose_project(home: &Home) -> String {
    if home.is_default() {
        "logos-devnet".into()
    } else {
        let path = home.root().to_string_lossy();
        format!("logos-devnet-{:016x}", fnv1a(path.as_bytes()))
    }
}

/// Whether the Docker daemon answers. With it down, no container of ours runs.
/// `path`, absolute, as `--volume` can mount it: its short syntax splits on `:`.
fn mountable(path: &Path) -> Result<std::path::PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("{} does not exist", path.display()))?;
    if path.to_string_lossy().contains(':') {
        bail!(
            "Docker can't mount {}: the path holds a `:`. Use one without, or `up --native`",
            path.display()
        );
    }
    Ok(path)
}

/// Whether `image` is in the local Docker image store.
pub(crate) fn image_present(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// What to do when no Docker daemon answers.
pub(crate) const START_DOCKER: &str =
    "install or start Docker (OrbStack, Docker Desktop or Colima), or use `up --native`";

pub(crate) fn docker_reachable() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A string Compose reads literally: it interpolates `$VAR` in every value, so a `$` in a
/// home path would otherwise point a mount somewhere else.
fn compose_literal(text: &str) -> String {
    text.replace('$', "$$")
}

/// FNV-1a: a hash that stays the same across Rust releases, unlike `DefaultHasher`.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, b| {
        (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// `uid:gid` of whoever runs the CLI, so files the containers write in the home directory are
/// theirs.
fn current_user() -> Result<String> {
    let id = |flag| -> Result<String> {
        let out = stdout_of(Command::new("id").arg(flag))?;
        let id = out.trim();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            bail!("`id {flag}` printed {id:?}");
        }
        Ok(id.to_owned())
    };
    Ok(format!("{}:{}", id("-u")?, id("-g")?))
}

struct ComposeInputs<'a> {
    project: &'a str,
    image: &'a str,
    port: u16,
    user: &'a str,
    chain_dir: &'a str,
    dev_mode: bool,
    log_filter: &'a str,
}

/// The compose file. The RPC is published on loopback only: it has no caller auth.
fn compose_spec(i: &ComposeInputs) -> Value {
    json!({
        "name": i.project,
        "services": {
            "sequencer": {
                "image": i.image,
                "user": i.user,
                "command": [
                    "sequencer_service",
                    format!("{CHAIN_MOUNT}/sequencer_config.json"),
                    "--port", CONTAINER_PORT.to_string()
                ],
                "ports": [format!("127.0.0.1:{}:{CONTAINER_PORT}", i.port)],
                // The long form takes any path, `:` included.
                "volumes": [{
                    "type": "bind",
                    "source": compose_literal(i.chain_dir),
                    "target": CHAIN_MOUNT
                }],
                "environment": sequencer_env(i.dev_mode, i.log_filter)
                    .into_iter()
                    .map(|(var, value)| (var.to_owned(), Value::from(compose_literal(&value))))
                    .collect::<serde_json::Map<_, _>>(),
                "stop_grace_period": "30s"
            },
            "wallet": {
                "image": i.image,
                "user": i.user,
                "profiles": ["tools"],
                "entrypoint": ["wallet"],
                "environment": { "LEE_WALLET_HOME_DIR": WALLET_MOUNT }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> ComposeInputs<'static> {
        ComposeInputs {
            project: "logos-devnet",
            image: "ghcr.io/drptbl/logos-devnet-sequencer:v0.3.1",
            port: 3040,
            user: "501:20",
            chain_dir: "/Users/me/My Devnet/chain",
            dev_mode: false,
            log_filter: "info,kameo=warn",
        }
    }

    #[test]
    fn the_sequencer_skips_proof_checks_only_in_dev_mode() {
        let spec = compose_spec(&inputs());
        let env = &spec["services"]["sequencer"]["environment"];
        assert_eq!(env["RISC0_DEV_MODE"], "0");
        assert_eq!(env["RUST_LOG"], "info,kameo=warn");
        let spec = compose_spec(&ComposeInputs {
            log_filter: "info,my_fn$x",
            ..inputs()
        });
        // Compose would read `$x` as a variable and drop it.
        assert_eq!(
            spec["services"]["sequencer"]["environment"]["RUST_LOG"],
            "info,my_fn$$x"
        );
        let spec = compose_spec(&ComposeInputs {
            dev_mode: true,
            ..inputs()
        });
        assert_eq!(
            spec["services"]["sequencer"]["environment"]["RISC0_DEV_MODE"],
            "1"
        );
    }

    #[test]
    fn publishes_the_rpc_on_loopback_only() {
        let spec = compose_spec(&ComposeInputs {
            port: 3999,
            ..inputs()
        });
        assert_eq!(
            spec["services"]["sequencer"]["ports"],
            json!(["127.0.0.1:3999:3040"])
        );
    }

    #[test]
    fn mounts_host_directories_as_the_host_user() {
        let spec = compose_spec(&inputs());
        let seq = &spec["services"]["sequencer"];
        assert_eq!(seq["user"], "501:20");
        assert_eq!(
            seq["volumes"],
            json!([{
                "type": "bind",
                "source": "/Users/me/My Devnet/chain",
                "target": "/devnet/chain"
            }])
        );
        assert_eq!(seq["command"][1], "/devnet/chain/sequencer_config.json");
        let wallet = &spec["services"]["wallet"];
        assert_eq!(wallet["user"], "501:20");
        assert_eq!(
            wallet["environment"]["LEE_WALLET_HOME_DIR"],
            "/devnet/wallet"
        );
        assert!(
            wallet.get("volumes").is_none(),
            "each run mounts the wallet it works on"
        );
    }

    #[test]
    fn project_names_keep_the_whole_hash() {
        // These two collide in the low 32 bits of FNV-1a.
        let a = fnv1a(b"/tmp/devnet-157538");
        let b = fnv1a(b"/tmp/devnet-296006");
        assert_eq!(a as u32, b as u32);
        assert_ne!(format!("{a:016x}"), format!("{b:016x}"));
    }

    #[test]
    fn a_dollar_in_a_home_path_stays_literal() {
        let spec = compose_spec(&ComposeInputs {
            chain_dir: "/tmp/dev$HOME/chain",
            ..inputs()
        });
        assert_eq!(
            spec["services"]["sequencer"]["volumes"][0]["source"],
            "/tmp/dev$$HOME/chain"
        );
    }

    #[test]
    fn refuses_to_mount_a_path_with_a_colon() {
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join("a:b");
        std::fs::create_dir(&odd).unwrap();
        assert!(mountable(&odd).is_err());
        assert!(mountable(dir.path()).is_ok());
    }

    #[test]
    fn keeps_the_wallet_out_of_up() {
        let spec = compose_spec(&inputs());
        assert_eq!(spec["services"]["wallet"]["profiles"], json!(["tools"]));
        assert!(spec["services"]["wallet"].get("ports").is_none());
    }

    #[test]
    fn the_dockerfile_keeps_the_config_where_the_cli_reads_it() {
        assert!(DOCKERFILE.contains("ARG LEZ_TAG"));
        assert!(DOCKERFILE.contains(UPSTREAM_CONFIG));
    }

    #[test]
    fn other_homes_get_their_own_compose_project() {
        let dir = tempfile::tempdir().unwrap();
        let a = Home::locate(Some(dir.path().join("a"))).unwrap();
        let b = Home::locate(Some(dir.path().join("b"))).unwrap();
        assert_ne!(compose_project(&a), compose_project(&b));
        assert!(compose_project(&a).starts_with("logos-devnet-"));
        // One directory spelled two ways is one project.
        let a2 = Home::locate(Some(dir.path().join("b/../a"))).unwrap();
        assert_eq!(compose_project(&a), compose_project(&a2));
    }

    #[test]
    fn the_default_home_is_plain_logos_devnet() {
        assert_eq!(
            compose_project(&Home::locate(None).unwrap()),
            "logos-devnet"
        );
    }
}
