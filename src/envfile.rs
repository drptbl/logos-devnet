//! `<home>/env`: what a project needs to point at the devnet, as `export` lines it can
//! `source`. `LOGOS_DEVNET_SEQUENCER` is the RPC URL, each deployed program adds
//! `LOGOS_PROGRAM_<NAME>` (base58) and `LOGOS_PROGRAM_<NAME>_HEX`, and each account group
//! `LOGOS_DEVNET_ACCOUNTS_<NAME>`, the path of its `accounts.json`. A messaging hub adds
//! `LOGOS_DEVNET_DELIVERY_CONFIG`, `LOGOS_DEVNET_DELIVERY_ENTRY_NODE` and
//! `LOGOS_DEVNET_DISCOVERY_BOOTSTRAP`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::account::PublicId;
use crate::messaging::Addresses;
use crate::runtime::sh_quote;

pub const SEQUENCER_VAR: &str = "LOGOS_DEVNET_SEQUENCER";
pub const HEADER: &str = "# Written by logos-devnet. Load it with: source <this file>\n";
const PROGRAM_PREFIX: &str = "LOGOS_PROGRAM_";
const GROUP_PREFIX: &str = "LOGOS_DEVNET_ACCOUNTS_";
const DELIVERY_CONFIG_VAR: &str = "LOGOS_DEVNET_DELIVERY_CONFIG";
const DELIVERY_ENTRY_NODE_VAR: &str = "LOGOS_DEVNET_DELIVERY_ENTRY_NODE";
const DISCOVERY_BOOTSTRAP_VAR: &str = "LOGOS_DEVNET_DISCOVERY_BOOTSTRAP";
/// RISC Zero's switch for fake proofs, read by every prover and verifier of the devnet.
pub const DEV_MODE_VAR: &str = "RISC0_DEV_MODE";

/// `DEV_MODE_VAR`'s value: set either way, so one left in a shell or a parent process can't
/// differ from the chain's.
pub fn dev_mode_value(dev_mode: bool) -> &'static str {
    if dev_mode { "1" } else { "0" }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Env {
    pub sequencer: String,
    /// Program name (as `deploy` took it) to its id.
    pub programs: BTreeMap<String, PublicId>,
    /// Account group name to its `accounts.json`.
    pub groups: BTreeMap<String, PathBuf>,
    /// Whether the chain runs in dev mode, so provers make fake proofs too.
    pub dev_mode: bool,
    /// Where apps reach the messaging hub, when the devnet has one.
    pub messaging: Option<Addresses>,
}

/// The messaging hub's variables: an app's delivery node config, the hub's delivery node and
/// its discovery bootstrap node.
fn messaging_vars(hub: &Addresses) -> [(&'static str, String); 3] {
    [
        (DELIVERY_CONFIG_VAR, hub.client_config()),
        (DELIVERY_ENTRY_NODE_VAR, hub.delivery.clone()),
        (DISCOVERY_BOOTSTRAP_VAR, hub.discovery.clone()),
    ]
}

/// The wallet directory of the group whose `accounts.json` is `manifest`, for
/// `LEE_WALLET_HOME_DIR`.
fn group_wallet(manifest: &std::path::Path) -> String {
    manifest
        .with_file_name(crate::groups::WALLET_DIR)
        .to_string_lossy()
        .into_owned()
}

/// `LOGOS_DEVNET_ACCOUNTS_<NAME>` for an account group name.
pub fn group_var(name: &str) -> String {
    format!(
        "{GROUP_PREFIX}{}",
        name.to_ascii_uppercase().replace('-', "_")
    )
}

/// A name that becomes part of a variable, a directory or a wallet label: lower-case letters,
/// digits and `-`, starting with a letter, so names map one to one onto variable names.
pub fn check_name(kind: &str, name: &str) -> Result<()> {
    let ok = name.len() <= 48
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!(
            "{kind} names are up to 48 lower-case letters, digits and -, starting with a letter: {name}"
        );
    }
    Ok(())
}

/// `LOGOS_PROGRAM_<NAME>` for a program name: upper case, `-` as `_`.
pub fn program_var(name: &str) -> String {
    format!(
        "{PROGRAM_PREFIX}{}",
        name.to_ascii_uppercase().replace('-', "_")
    )
}

/// A program name `deploy` accepts: lower-case letters, digits and `-`, starting with a
/// letter. Those map one to one onto env var names, and a name can't end in `-hex`, which
/// would collide with another program's `_HEX` variable.
pub fn check_program_name(name: &str) -> Result<()> {
    check_name("program", name)?;
    if name.ends_with("-hex") {
        bail!("program names can't end in -hex (it would clash with another's _HEX): {name}");
    }
    Ok(())
}

impl Env {
    /// Every variable and its value, in the order `render` writes them.
    pub fn vars(&self) -> Vec<(String, String)> {
        let mut vars = vec![
            (SEQUENCER_VAR.to_owned(), self.sequencer.clone()),
            (
                DEV_MODE_VAR.to_owned(),
                dev_mode_value(self.dev_mode).to_owned(),
            ),
        ];
        for (name, id) in &self.programs {
            let var = program_var(name);
            vars.push((var.clone(), id.as_str().to_owned()));
            vars.push((format!("{var}_HEX"), id.hex()));
        }
        for (name, path) in &self.groups {
            let var = group_var(name);
            vars.push((var.clone(), path.to_string_lossy().into_owned()));
            vars.push((format!("{var}_WALLET"), group_wallet(path)));
        }
        for (var, value) in self.messaging.iter().flat_map(messaging_vars) {
            vars.push((var.to_owned(), value));
        }
        vars
    }

    /// The env file: `vars`' export lines, with a comment naming each program and group so
    /// `parse` reads their names back.
    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        out += &export_line(SEQUENCER_VAR, &self.sequencer);
        out += &export_line(DEV_MODE_VAR, dev_mode_value(self.dev_mode));
        for (name, id) in &self.programs {
            let var = program_var(name);
            out += &format!("# program {name}\n");
            out += &export_line(&var, id.as_str());
            out += &export_line(&format!("{var}_HEX"), &id.hex());
        }
        for (name, path) in &self.groups {
            out += &format!("# accounts {name}\n");
            let var = group_var(name);
            out += &export_line(&var, &path.to_string_lossy());
            out += &export_line(&format!("{var}_WALLET"), &group_wallet(path));
        }
        if let Some(hub) = &self.messaging {
            out += "# messaging hub\n";
            for (var, value) in messaging_vars(hub) {
                out += &export_line(var, &value);
            }
        }
        out
    }

    /// Reads back what `render` wrote. Program names come from the `# program` comments, so
    /// they survive the round trip in their original spelling.
    pub fn parse(text: &str) -> Result<Self> {
        let mut env = Env::default();
        let mut pending: Option<String> = None;
        let mut pending_group: Option<String> = None;
        let (mut entry_node, mut bootstrap) = (None, None);
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("# program ") {
                pending = Some(name.trim().to_owned());
            } else if let Some(name) = line.strip_prefix("# accounts ") {
                pending_group = Some(name.trim().to_owned());
            } else if let Some(assignment) = line.strip_prefix("export ") {
                let Some((var, value)) = assignment.split_once('=') else {
                    continue;
                };
                // v0.1.0 wrote values unquoted.
                let value = if value.starts_with('\'') {
                    sh_unquote(value)?
                } else {
                    value.to_owned()
                };
                if var == SEQUENCER_VAR {
                    env.sequencer = value;
                } else if var == DEV_MODE_VAR {
                    env.dev_mode = value == dev_mode_value(true);
                } else if let Some(name) = pending.take_if(|n| program_var(n) == var) {
                    env.programs.insert(name, PublicId::parse(&value)?);
                } else if let Some(name) = pending_group.take_if(|n| group_var(n) == var) {
                    env.groups.insert(name, PathBuf::from(value));
                } else if var == DELIVERY_ENTRY_NODE_VAR {
                    entry_node = Some(value);
                } else if var == DISCOVERY_BOOTSTRAP_VAR {
                    bootstrap = Some(value);
                }
            }
        }
        if env.sequencer.is_empty() {
            bail!("the env file has no {SEQUENCER_VAR}");
        }
        if let (Some(delivery), Some(discovery)) = (entry_node, bootstrap) {
            env.messaging = Some(Addresses {
                delivery,
                discovery,
            });
        }
        Ok(env)
    }
}

/// `export VAR='value'`: every value single-quoted, so any path survives `source`.
pub fn export_line(var: &str, value: &str) -> String {
    format!("export {var}={}\n", sh_quote(value))
}

/// The inverse of `runtime::sh_quote`.
fn sh_unquote(value: &str) -> Result<String> {
    let inner = value
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .context("expected a single-quoted value")?;
    Ok(inner.replace(r"'\''", "'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d";

    #[test]
    fn names_program_vars() {
        assert_eq!(program_var("counter"), "LOGOS_PROGRAM_COUNTER");
        assert_eq!(program_var("my-program-v2"), "LOGOS_PROGRAM_MY_PROGRAM_V2");
    }

    #[test]
    fn checks_program_names() {
        assert!(check_program_name("counter").is_ok());
        assert!(check_program_name("my-program-2").is_ok());
        assert!(check_program_name("hexagon").is_ok());
        // Each of these would collide with another name's variables, or isn't one.
        for bad in ["", "2fast", "a b", "a/b", "a_b", "Counter", "counter-hex"] {
            assert!(check_program_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn renders_sourceable_exports() {
        let mut env = Env {
            sequencer: "http://127.0.0.1:3040".into(),
            ..Env::default()
        };
        env.programs
            .insert("counter".into(), PublicId::parse(ID).unwrap());
        let text = env.render();
        assert!(text.contains("export LOGOS_DEVNET_SEQUENCER='http://127.0.0.1:3040'\n"));
        assert!(text.contains(&format!("export LOGOS_PROGRAM_COUNTER='{ID}'\n")));
        assert!(text.contains(
            "export LOGOS_PROGRAM_COUNTER_HEX='f9eba477278c918fadafbcb4a6463e947d0fd8de344b5431ee0aa3cd41d7e3a4'\n"
        ));
    }

    #[test]
    fn always_says_whether_proofs_are_skipped() {
        let mut env = Env {
            sequencer: "http://127.0.0.1:3040".into(),
            ..Env::default()
        };
        // Off is written too: a RISC0_DEV_MODE=1 left in the shell would send fake proofs.
        assert!(env.render().contains("export RISC0_DEV_MODE='0'\n"));
        assert!(env.vars().contains(&("RISC0_DEV_MODE".into(), "0".into())));
        env.dev_mode = true;
        assert!(env.render().contains("export RISC0_DEV_MODE='1'\n"));
        assert_eq!(Env::parse(&env.render()).unwrap(), env);
    }

    #[test]
    fn exports_each_groups_wallet_directory() {
        let mut env = Env {
            sequencer: "http://127.0.0.1:3040".into(),
            ..Env::default()
        };
        env.groups
            .insert("users".into(), "/h/accounts/users/accounts.json".into());
        let vars = env.vars();
        assert!(vars.contains(&(
            "LOGOS_DEVNET_ACCOUNTS_USERS_WALLET".into(),
            "/h/accounts/users/wallet".into()
        )));
        assert!(
            env.render()
                .contains("export LOGOS_DEVNET_ACCOUNTS_USERS_WALLET='/h/accounts/users/wallet'\n")
        );
        assert_eq!(Env::parse(&env.render()).unwrap(), env);
    }

    #[test]
    fn round_trips() {
        let mut env = Env {
            sequencer: "http://127.0.0.1:3041".into(),
            ..Env::default()
        };
        env.programs
            .insert("my-program".into(), PublicId::parse(ID).unwrap());
        env.groups.insert(
            "users".into(),
            "/h/it's here/accounts/users/accounts.json".into(),
        );
        assert_eq!(Env::parse(&env.render()).unwrap(), env);
    }

    #[test]
    fn exports_the_messaging_hub_so_a_shell_reads_its_json_intact() {
        let hub = Addresses {
            delivery: "/ip4/127.0.0.1/tcp/61310/p2p/16Uiu2HAmA".into(),
            discovery: "/ip4/127.0.0.1/tcp/61320/p2p/16Uiu2HAmB".into(),
        };
        let env = Env {
            sequencer: "http://127.0.0.1:3041".into(),
            messaging: Some(hub.clone()),
            ..Env::default()
        };
        assert_eq!(Env::parse(&env.render()).unwrap(), env);
        let vars = env.vars();
        assert!(vars.contains(&(DELIVERY_ENTRY_NODE_VAR.into(), hub.delivery.clone())));
        assert!(vars.contains(&(DISCOVERY_BOOTSTRAP_VAR.into(), hub.discovery.clone())));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("env");
        std::fs::write(&file, env.render()).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                ". '{}' && printf %s \"${DELIVERY_CONFIG_VAR}\"",
                file.display()
            ))
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), hub.client_config());
    }

    #[test]
    fn reads_a_v0_1_0_env_file() {
        // As v0.1.0 wrote it: unquoted values.
        let text = format!(
            "# Written by logos-devnet. Load it with: source <this file>\n\
             export LOGOS_DEVNET_SEQUENCER=http://127.0.0.1:3040\n\
             # program escrow\n\
             export LOGOS_PROGRAM_ESCROW={ID}\n"
        );
        let env = Env::parse(&text).unwrap();
        assert_eq!(env.sequencer, "http://127.0.0.1:3040");
        assert_eq!(env.programs["escrow"].as_str(), ID);
    }

    #[test]
    fn refuses_a_file_without_the_sequencer() {
        assert!(Env::parse("# nothing\n").is_err());
    }
}
