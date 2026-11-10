//! `<home>/env`: what a project needs to point at the devnet, as `export` lines it can
//! `source`. `LOGOS_DEVNET_SEQUENCER` is the RPC URL, and each deployed program adds
//! `LOGOS_PROGRAM_<NAME>` (base58) and `LOGOS_PROGRAM_<NAME>_HEX`.

use std::collections::BTreeMap;

use anyhow::{Result, bail};

use crate::account::PublicId;

pub const SEQUENCER_VAR: &str = "LOGOS_DEVNET_SEQUENCER";
const PROGRAM_PREFIX: &str = "LOGOS_PROGRAM_";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Env {
    pub sequencer: String,
    /// Program name (as `deploy` took it) to its id.
    pub programs: BTreeMap<String, PublicId>,
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
    let ok = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.ends_with("-hex");
    if !ok {
        bail!(
            "program names are lower-case letters, digits and -, start with a letter and don't \
             end in -hex: {name}"
        );
    }
    Ok(())
}

impl Env {
    pub fn render(&self) -> String {
        let mut out = String::from("# Written by logos-devnet. Load it with: source <this file>\n");
        out += &format!("export {SEQUENCER_VAR}={}\n", self.sequencer);
        for (name, id) in &self.programs {
            let var = program_var(name);
            out += &format!("# program {name}\n");
            out += &format!("export {var}={}\n", id.as_str());
            out += &format!("export {var}_HEX={}\n", id.hex());
        }
        out
    }

    /// Reads back what `render` wrote. Program names come from the `# program` comments, so
    /// they survive the round trip in their original spelling.
    pub fn parse(text: &str) -> Result<Self> {
        let mut env = Env::default();
        let mut pending: Option<String> = None;
        for line in text.lines() {
            if let Some(name) = line.strip_prefix("# program ") {
                pending = Some(name.trim().to_owned());
            } else if let Some(assignment) = line.strip_prefix("export ") {
                let Some((var, value)) = assignment.split_once('=') else {
                    continue;
                };
                if var == SEQUENCER_VAR {
                    env.sequencer = value.to_owned();
                } else if let Some(name) = pending.take_if(|n| program_var(n) == var) {
                    env.programs.insert(name, PublicId::parse(value)?);
                }
            }
        }
        if env.sequencer.is_empty() {
            bail!("the env file has no {SEQUENCER_VAR}");
        }
        Ok(env)
    }
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
        assert!(text.contains("export LOGOS_DEVNET_SEQUENCER=http://127.0.0.1:3040\n"));
        assert!(text.contains(&format!("export LOGOS_PROGRAM_COUNTER={ID}\n")));
        assert!(text.contains(
            "export LOGOS_PROGRAM_COUNTER_HEX=f9eba477278c918fadafbcb4a6463e947d0fd8de344b5431ee0aa3cd41d7e3a4\n"
        ));
    }

    #[test]
    fn round_trips() {
        let mut env = Env {
            sequencer: "http://127.0.0.1:3041".into(),
            ..Env::default()
        };
        env.programs
            .insert("my-program".into(), PublicId::parse(ID).unwrap());
        assert_eq!(Env::parse(&env.render()).unwrap(), env);
    }

    #[test]
    fn refuses_a_file_without_the_sequencer() {
        assert!(Env::parse("# nothing\n").is_err());
    }
}
