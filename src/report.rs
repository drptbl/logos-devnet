//! What each command reports: printed for people, or as JSON with `--json` for scripts and
//! agents. Field names are part of the CLI's interface; the README lists them.

use std::path::PathBuf;

use serde::Serialize;

/// A command's result, in both forms.
pub trait Report: Serialize {
    /// The text a person reads.
    fn human(&self) -> String;
}

/// Prints `report` as JSON (only JSON goes to stdout) or as text.
pub fn emit<R: Report>(report: &R, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        print!("{}", report.human());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub lez: String,
    pub runtime: String,
    pub sequencer: Sequencer,
    pub faucet: Option<Balance>,
    pub programs: Vec<Program>,
    pub account_groups: Vec<GroupSummary>,
    pub sessions: Vec<String>,
    /// The env file to `source`, once the devnet has run.
    pub env_file: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct Sequencer {
    pub url: String,
    pub running: bool,
    /// The chain head, when it answers.
    pub block: Option<u64>,
    pub max_txs_per_block: u32,
    pub block_time: String,
}

#[derive(Debug, Serialize)]
pub struct Balance {
    /// `Public/<base58>`.
    pub account: String,
    /// In lepta, when the chain answers.
    pub balance: Option<u128>,
}

#[derive(Debug, Serialize)]
pub struct Program {
    pub name: String,
    pub id: String,
    pub hex: String,
    pub var: String,
    /// False when it was already on this chain.
    pub deployed_now: bool,
}

#[derive(Debug, Serialize)]
pub struct GroupSummary {
    pub name: String,
    pub accounts: usize,
    /// Lepta each account is funded with.
    pub amount: u64,
    /// `accounts.json`, listing every account.
    pub file: PathBuf,
    /// The LEZ wallet holding the keys.
    pub wallet: PathBuf,
    pub var: String,
}

#[derive(Debug, Serialize)]
pub struct SessionMade {
    pub name: String,
    pub modules: Vec<String>,
    /// What `session env` exports, by variable.
    pub vars: Vec<(String, String)>,
}

#[derive(Debug, Serialize)]
pub struct SessionRow {
    pub name: String,
    pub running: bool,
    pub account: Option<String>,
    pub balance: Option<u128>,
    /// Why the session can't be read, when it can't.
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Sessions(pub Vec<SessionRow>);

#[derive(Debug, Serialize)]
pub struct Groups(pub Vec<GroupSummary>);

/// The env file's variables, in its order.
#[derive(Debug, Serialize)]
pub struct EnvVars(pub serde_json::Map<String, serde_json::Value>);

impl EnvVars {
    pub fn new(vars: Vec<(String, String)>) -> Self {
        Self(
            vars.into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect(),
        )
    }
}

#[derive(Debug, Serialize)]
pub struct Done {
    pub message: String,
}

impl Report for Status {
    fn human(&self) -> String {
        let s = &self.sequencer;
        let mut out = format!("lez        {} ({})\n", self.lez, self.runtime);
        out += &match (s.running, s.block) {
            (true, Some(block)) => format!("sequencer  {}  running, block {block}\n", s.url),
            (true, None) => format!("sequencer  {}  starting\n", s.url),
            (false, _) => format!("sequencer  {}  stopped\n", s.url),
        };
        if let Some(faucet) = &self.faucet {
            out += &format!("faucet     {}{}\n", faucet.account, lepta(faucet.balance));
        }
        for p in &self.programs {
            out += &format!("program    {}  {}  ({})\n", p.name, p.id, p.var);
        }
        for g in &self.account_groups {
            out += &format!(
                "accounts   {}  {} x {} lepta  ({})\n",
                g.name, g.accounts, g.amount, g.var
            );
        }
        if !self.sessions.is_empty() {
            out += &format!("sessions   {}\n", self.sessions.join(", "));
        }
        if let Some(env) = &self.env_file {
            out += &format!("env        {}\n", env.display());
        }
        out
    }
}

impl Report for Balance {
    fn human(&self) -> String {
        format!("{}{}\n", self.account, lepta(self.balance))
    }
}

impl Report for Program {
    fn human(&self) -> String {
        format!("{}={}\n", self.var, self.id)
    }
}

impl Report for SessionMade {
    fn human(&self) -> String {
        let mut out = format!(
            "session {} ready, with {}\n",
            self.name,
            self.modules.join(", ")
        );
        for (var, value) in &self.vars {
            out += &format!("  {var}={value}\n");
        }
        out += &format!(
            "Load it with: source <(logos-devnet session env {})\n",
            self.name
        );
        out
    }
}

impl Report for Sessions {
    fn human(&self) -> String {
        if self.0.is_empty() {
            return "No sessions. `logos-devnet session new <name>` makes one.\n".into();
        }
        self.0
            .iter()
            .map(|s| {
                let state = if s.running { "running" } else { "stopped" };
                match (&s.account, &s.error) {
                    (Some(account), _) => {
                        format!("{:<16} {state:<8} {account}{}\n", s.name, lepta(s.balance))
                    }
                    (None, error) => format!(
                        "{:<16} {state:<8} {}\n",
                        s.name,
                        error.as_deref().unwrap_or("")
                    ),
                }
            })
            .collect()
    }
}

impl Report for GroupSummary {
    fn human(&self) -> String {
        format!(
            "{}  {} accounts x {} lepta\n  accounts {}\n  wallet   {}\n  env      {}\n",
            self.name,
            self.accounts,
            self.amount,
            self.file.display(),
            self.wallet.display(),
            self.var
        )
    }
}

impl Report for Groups {
    fn human(&self) -> String {
        if self.0.is_empty() {
            return "No account groups. `logos-devnet accounts new <name> --count <n>` makes one.\n"
                .into();
        }
        self.0.iter().map(Report::human).collect()
    }
}

impl Report for EnvVars {
    fn human(&self) -> String {
        let mut out = String::from("# Written by logos-devnet. Load it with: source <this file>\n");
        for (var, value) in &self.0 {
            let value = value.as_str().unwrap_or_default();
            out += &format!("export {var}={}\n", crate::runtime::sh_quote(value));
        }
        out
    }
}

impl Report for Done {
    fn human(&self) -> String {
        format!("{}\n", self.message)
    }
}

/// `  <n> lepta`, or nothing when unknown.
fn lepta(balance: Option<u128>) -> String {
    balance.map_or(String::new(), |b| format!("  {b} lepta"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_fields_are_stable() {
        let status = Status {
            lez: "v0.3.1".into(),
            runtime: "docker".into(),
            sequencer: Sequencer {
                url: "http://127.0.0.1:3040".into(),
                running: true,
                block: Some(7),
                max_txs_per_block: 20,
                block_time: "1s".into(),
            },
            faucet: None,
            programs: vec![],
            account_groups: vec![],
            sessions: vec![],
            env_file: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        for key in [
            "lez",
            "runtime",
            "sequencer",
            "faucet",
            "programs",
            "account_groups",
            "sessions",
            "env_file",
        ] {
            assert!(json.get(key).is_some(), "{key}");
        }
        assert_eq!(json["sequencer"]["block"], 7);
        assert!(status.human().contains("running, block 7"));
    }

    #[test]
    fn env_vars_keep_their_order_and_quote_for_the_shell() {
        let env = EnvVars::new(vec![("B".into(), "it's".into()), ("A".into(), "x".into())]);
        let keys: Vec<_> = env.0.keys().cloned().collect();
        assert_eq!(keys, ["B", "A"]);
        assert_eq!(
            env.human(),
            "# Written by logos-devnet. Load it with: source <this file>\nexport B='it'\\''s'\nexport A='x'\n"
        );
    }
}
