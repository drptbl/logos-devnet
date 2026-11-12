//! What each command reports: printed for people, or as JSON with `--json` for scripts and
//! agents. Field names are part of the CLI's interface; the README lists them.

use std::collections::BTreeMap;
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
    /// The local messaging hub, when the devnet has one.
    pub messaging: Option<Messaging>,
    /// The hub's storage node, when a project asked for local storage.
    pub storage: Option<Storage>,
    /// The env file to `source`, once the devnet has run.
    pub env_file: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct Storage {
    /// Whether the hub's storage node, the sessions' bootstrap node, runs.
    pub running: bool,
}

#[derive(Debug, Serialize)]
pub struct Messaging {
    /// Whether its daemon runs.
    pub running: bool,
    /// Whether its delivery node runs.
    pub delivery_node: bool,
    /// Whether its discovery node runs.
    pub discovery_node: bool,
    /// Its nodes' multiaddrs, `delivery` and `discovery`, once they reported them.
    #[serde(flatten)]
    pub addresses: Option<crate::messaging::Addresses>,
}

#[derive(Debug, Serialize)]
pub struct Sequencer {
    pub url: String,
    pub running: bool,
    /// False when Docker didn't answer in time: `running` is then false, but means unknown.
    pub runtime_answered: bool,
    /// The chain head, when it answers.
    pub block: Option<u64>,
    pub max_txs_per_block: u32,
    pub block_time: String,
    /// Whether it skips proof checks (`RISC0_DEV_MODE`).
    pub dev_mode: bool,
}

#[derive(Debug, Serialize)]
pub struct Transaction {
    pub hash: String,
    /// The block that holds it, once one does.
    pub block: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Account {
    /// `Public/<base58>`.
    pub account: String,
    /// The same id as 64 hex digits, as `lez_core` takes it.
    pub hex: String,
    pub balance: u128,
    pub nonce: u128,
    /// Whether it signed a transaction or holds data: a fresh account does neither.
    pub in_use: bool,
    /// The bytes each program keeps in it, by program id. The native token's is the balance.
    pub data: BTreeMap<String, usize>,
    /// The same bytes, as hex.
    pub data_hex: BTreeMap<String, String>,
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
    /// True when this call put it on the chain, false when it was there already.
    pub deployed_now: bool,
    /// True when a new build replaced it in place: it's mutable.
    pub updated: bool,
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

/// The env file's variables, in its order. Serialized as one JSON object.
#[derive(Debug)]
pub struct EnvVars(Vec<(String, String)>);

impl EnvVars {
    pub fn new(vars: Vec<(String, String)>) -> Self {
        Self(vars)
    }

    pub fn vars(&self) -> &[(String, String)] {
        &self.0
    }
}

impl Serialize for EnvVars {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (var, value) in &self.0 {
            map.serialize_entry(var, value)?;
        }
        map.end()
    }
}

#[derive(Debug, Serialize)]
pub struct Done {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct WalletConfig {
    /// The `wallet_config.json` written.
    pub path: PathBuf,
}

impl Report for WalletConfig {
    fn human(&self) -> String {
        format!("{}\n", self.path.display())
    }
}

impl Report for Status {
    fn human(&self) -> String {
        let s = &self.sequencer;
        let mut out = format!("lez        {} ({})\n", self.lez, self.runtime);
        out += &match (s.running, s.block) {
            _ if !s.runtime_answered => {
                format!("sequencer  {}  unknown: Docker didn't answer", s.url)
            }
            (true, Some(block)) => format!("sequencer  {}  running, block {block}", s.url),
            (true, None) => format!("sequencer  {}  starting", s.url),
            (false, _) => format!("sequencer  {}  stopped", s.url),
        };
        out += if s.dev_mode {
            ", dev mode (fake proofs)\n"
        } else {
            "\n"
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
        if let Some(m) = &self.messaging {
            let down: Vec<&str> = [
                (m.delivery_node, "delivery"),
                (m.discovery_node, "discovery"),
            ]
            .into_iter()
            .filter_map(|(up, node)| (!up).then_some(node))
            .collect();
            out += &match (m.running, down.as_slice()) {
                (false, _) => "messaging  stopped\n".to_owned(),
                (true, []) => "messaging  running\n".to_owned(),
                (true, down) => format!(
                    "messaging  running, but its {} node{} down. `logos-devnet up` starts {}\n",
                    down.join(" and "),
                    if down.len() == 1 { " is" } else { "s are" },
                    if down.len() == 1 { "it" } else { "them" },
                ),
            };
            if let Some(a) = &m.addresses {
                out += &format!("           delivery  {}\n", a.delivery);
                out += &format!("           discovery  {}\n", a.discovery);
            }
        }
        if let Some(s) = &self.storage {
            out += if s.running {
                "storage    running, the sessions' bootstrap node\n"
            } else {
                "storage    stopped. `logos-devnet up` starts it\n"
            };
        }
        if let Some(env) = &self.env_file {
            out += &format!("env        {}\n", env.display());
        }
        out
    }
}

impl Report for Transaction {
    fn human(&self) -> String {
        match self.block {
            Some(block) => format!("{}  in block {block}\n", self.hash),
            None => format!("{}  in no block yet\n", self.hash),
        }
    }
}

impl Report for Account {
    fn human(&self) -> String {
        let mut out = format!(
            "{}\n  hex      {}\n  balance  {} lepta\n  nonce    {}\n",
            self.account, self.hex, self.balance, self.nonce
        );
        for (program, bytes) in &self.data {
            out += &format!("  data     {program}: {bytes} bytes\n");
        }
        if !self.in_use {
            out += "  unused: no transaction signed, no data held\n";
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
        let mut out = String::from(crate::envfile::HEADER);
        for (var, value) in &self.0 {
            out += &crate::envfile::export_line(var, value);
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
                runtime_answered: true,
                block: Some(7),
                max_txs_per_block: 20,
                block_time: "1s".into(),
                dev_mode: true,
            },
            faucet: None,
            programs: vec![],
            account_groups: vec![],
            sessions: vec![],
            messaging: Some(Messaging {
                running: true,
                delivery_node: true,
                discovery_node: false,
                addresses: Some(crate::messaging::Addresses {
                    delivery: "/ip4/127.0.0.1/tcp/61310/p2p/16Uiu2HAmA".into(),
                    discovery: "/ip4/127.0.0.1/tcp/61320/p2p/16Uiu2HAmB".into(),
                }),
            }),
            storage: None,
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
            "messaging",
            "env_file",
        ] {
            assert!(json.get(key).is_some(), "{key}");
        }
        assert_eq!(json["sequencer"]["block"], 7);
        assert_eq!(json["sequencer"]["dev_mode"], true);
        assert!(status.human().contains("running, block 7, dev mode"));
        assert_eq!(json["sequencer"]["runtime_answered"], true);
        assert_eq!(json["messaging"]["running"], true);
        assert_eq!(
            json["messaging"]["delivery"],
            "/ip4/127.0.0.1/tcp/61310/p2p/16Uiu2HAmA"
        );
        assert!(
            status.human().contains(
                "messaging  running, but its discovery node is down. `logos-devnet up` starts it\n           delivery  /ip4/127.0.0.1/tcp/61310"
            ),
            "{}",
            status.human()
        );
        assert_eq!(json["messaging"]["discovery_node"], false);
    }

    #[test]
    fn a_runtime_that_didnt_answer_leaves_the_sequencer_unknown() {
        let status = Status {
            lez: "v0.3.1".into(),
            runtime: "docker".into(),
            sequencer: Sequencer {
                url: "http://127.0.0.1:3040".into(),
                running: false,
                runtime_answered: false,
                block: None,
                max_txs_per_block: 20,
                block_time: "1s".into(),
                dev_mode: false,
            },
            faucet: None,
            programs: vec![],
            account_groups: vec![],
            sessions: vec![],
            messaging: None,
            storage: None,
            env_file: None,
        };
        assert!(
            status
                .human()
                .contains("sequencer  http://127.0.0.1:3040  unknown: Docker didn't answer"),
            "{}",
            status.human()
        );
        assert_eq!(
            serde_json::to_value(&status).unwrap()["sequencer"]["runtime_answered"],
            false
        );
    }

    #[test]
    fn env_vars_keep_their_order_and_quote_for_the_shell() {
        let env = EnvVars::new(vec![("B".into(), "it's".into()), ("A".into(), "x".into())]);
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"B":"it's","A":"x"}"#, "in order, not sorted");
        assert_eq!(
            env.human(),
            "# Written by logos-devnet. Load it with: source <this file>\nexport B='it'\\''s'\nexport A='x'\n"
        );
    }
}
