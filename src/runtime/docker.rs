//! The Docker runtime: our image (`ghcr.io/drptbl/logos-devnet-sequencer:<lez tag>`) run with
//! Docker Compose. The chain and the dev wallet stay in the home directory, bind-mounted and
//! owned by the host user, so `reset` and inspection work the same as natively.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Runtime, SEQUENCER_LOG, WalletArg, all_output_of, run_attached, stdout_of};
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
        // The chain and wallet directories must exist before Docker mounts them, or it creates
        // them as root.
        std::fs::create_dir_all(self.home.chain_dir())?;
        std::fs::create_dir_all(self.home.wallet_dir())?;
        let spec = compose_spec(&ComposeInputs {
            project: &compose_project(&self.home),
            image: &self.settings.image(),
            port: self.settings.port,
            user: &current_user()?,
            chain_dir: &self.home.chain_dir().to_string_lossy(),
            wallet_dir: &self.home.wallet_dir().to_string_lossy(),
        });
        // JSON is YAML, and serde quotes every path correctly.
        write_json(&self.home.compose_file(), &spec)
    }

    fn image_exists(&self) -> bool {
        Command::new("docker")
            .args(["image", "inspect", "--format", "{{.Id}}"])
            .arg(self.settings.image())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
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
        if rebuild {
            return self.build_image();
        }
        if self.image_exists() {
            return Ok(());
        }
        eprintln!("Pulling {}...", self.settings.image());
        let pulled = Command::new("docker")
            .arg("pull")
            .arg(self.settings.image())
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

    fn wallet(&self, args: Vec<WalletArg>) -> Result<String> {
        self.write_compose_file()?;
        let mut cmd = self.compose();
        cmd.args(["run", "--rm", "--no-deps", "-T"]);
        let mut wallet_args = Vec::new();
        for (i, arg) in args.into_iter().enumerate() {
            match arg {
                WalletArg::Text(text) => wallet_args.push(text),
                WalletArg::File(path) => {
                    // Mount just the file, read-only.
                    let path = path
                        .canonicalize()
                        .with_context(|| format!("{} does not exist", path.display()))?;
                    let mount = format!("{FILES_MOUNT}/{i}");
                    cmd.arg("--volume")
                        .arg(format!("{}:{mount}:ro", path.display()));
                    wallet_args.push(mount);
                }
            }
        }
        cmd.arg("wallet").args(wallet_args);
        all_output_of(&mut cmd)
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
fn docker_reachable() -> bool {
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
    wallet_dir: &'a str,
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
                "volumes": [format!("{}:{CHAIN_MOUNT}", compose_literal(i.chain_dir))],
                "environment": { "RUST_LOG": SEQUENCER_LOG },
                "stop_grace_period": "30s"
            },
            "wallet": {
                "image": i.image,
                "user": i.user,
                "profiles": ["tools"],
                "entrypoint": ["wallet"],
                "volumes": [format!("{}:{WALLET_MOUNT}", compose_literal(i.wallet_dir))],
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
            wallet_dir: "/Users/me/My Devnet/wallet",
        }
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
            json!(["/Users/me/My Devnet/chain:/devnet/chain"])
        );
        assert_eq!(seq["command"][1], "/devnet/chain/sequencer_config.json");
        let wallet = &spec["services"]["wallet"];
        assert_eq!(wallet["user"], "501:20");
        assert_eq!(
            wallet["environment"]["LEE_WALLET_HOME_DIR"],
            "/devnet/wallet"
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
            wallet_dir: "/tmp/dev$HOME/wallet",
            ..inputs()
        });
        assert_eq!(
            spec["services"]["sequencer"]["volumes"][0],
            "/tmp/dev$$HOME/chain:/devnet/chain"
        );
        assert_eq!(
            spec["services"]["wallet"]["volumes"][0],
            "/tmp/dev$$HOME/wallet:/devnet/wallet"
        );
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
