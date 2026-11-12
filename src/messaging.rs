//! The local messaging hub: a logosctl daemon of the devnet's own, running a delivery node
//! (Core mode, store on, a private cluster, no preset) and a libp2p discovery node, both on
//! `127.0.0.1` only. Apps point their own delivery node at the hub (`client_config`) and use its
//! libp2p node as their only discovery bootstrap, so messages and service lookups stay on this
//! machine.
//!
//! ```text
//! <home>/messaging/
//!   hub.json         ports, node keys and the addresses the nodes reported
//!   logosctl/        the hub's daemon
//!   store.sqlite3    the delivery node's message store
//! ```
//!
//! Ports and keys are picked once, so the hub keeps its addresses across restarts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::session::Logosctl;

const DELIVERY: &str = "delivery_module";
const DISCOVERY: &str = "libp2p_module";
const STORAGE: &str = "storage_module";
/// How long the hub's storage node gets to start with its daemon.
const STORAGE_START_TIMEOUT: Duration = Duration::from_secs(30);
/// A cluster no public network uses, so a node that ends up on the public fleet can't join it.
const CLUSTER: u32 = 77;
const SHARDS: u32 = 8;
/// How long `status` waits for each answer from the hub's daemon.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// libp2p's protobuf header for a secp256k1 private key: key type 2, then 32 bytes of data.
const SECP256K1_KEY_HEADER: &str = "08021220";

/// What `hub.json` records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    delivery_port: u16,
    discovery_port: u16,
    /// The delivery node's `nodekey`: a secp256k1 secret key, as hex.
    delivery_key: String,
    /// The discovery node's `privKey`: a libp2p protobuf private key, as hex.
    discovery_key: String,
    /// Whether the hub's modules were installed from the catalog.
    #[serde(default)]
    installed: bool,
    /// The addresses the running nodes reported, once they have.
    #[serde(default)]
    addresses: Option<Addresses>,
    /// Whether the hub runs the delivery and discovery nodes: hubs from before `[storage]`
    /// all did.
    #[serde(default = "runs_messaging")]
    messaging: bool,
    /// The storage node's port, once a project asked for local storage.
    #[serde(default)]
    storage_port: Option<u16>,
    /// The storage node's signed peer record, which sessions bootstrap from.
    #[serde(default)]
    storage_spr: Option<String>,
}

fn runs_messaging() -> bool {
    true
}

/// What a project asks the hub to run. What any project asked for stays on.
#[derive(Debug, Clone, Copy, Default)]
pub struct Wants {
    pub messaging: bool,
    pub storage: bool,
}

/// Where apps reach the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addresses {
    /// The delivery node's multiaddr, `/ip4/127.0.0.1/tcp/<port>/p2p/<id>`: an `entry-node`.
    pub delivery: String,
    /// The discovery node's multiaddr, `/ip4/127.0.0.1/tcp/<port>/p2p/<id>`: a bootstrap node.
    pub discovery: String,
}

impl Addresses {
    /// The `delivery_module.createNode` config of an app's node that uses the hub as its only
    /// entry node: Edge mode, the hub's cluster, no preset, loopback only.
    pub fn client_config(&self) -> String {
        let mut overrides = network_overrides();
        overrides.insert("entry-node".into(), json!([self.delivery]));
        json!({ "mode": "Edge", "messagingOverrides": overrides }).to_string()
    }
}

pub struct Hub {
    dir: PathBuf,
    ctl: Logosctl,
    status_timeout: Duration,
}

/// Which parts of the hub run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub daemon: bool,
    pub delivery: bool,
    pub discovery: bool,
    pub storage: bool,
}

impl Hub {
    pub fn new(dir: PathBuf) -> Self {
        let ctl = Logosctl::new(dir.join("logosctl"));
        Self {
            dir,
            ctl,
            status_timeout: STATUS_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_logosctl(dir: PathBuf, ctl: Logosctl) -> Self {
        Self {
            dir,
            ctl,
            status_timeout: Duration::from_secs(2),
        }
    }

    fn state_file(&self) -> PathBuf {
        self.dir.join("hub.json")
    }

    /// Whether the devnet has a hub, running or not.
    pub fn exists(&self) -> bool {
        self.state_file().exists()
    }

    fn state(&self) -> Result<State> {
        let text = std::fs::read_to_string(self.state_file())
            .with_context(|| format!("reading {}", self.state_file().display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("{} is damaged", self.state_file().display()))
    }

    fn save(&self, state: &State) -> Result<()> {
        crate::home::write_json(&self.state_file(), &serde_json::to_value(state)?)
    }

    /// The ports the hub listens on, once it has some.
    pub fn ports(&self) -> Vec<u16> {
        let Ok(state) = self.state() else {
            return Vec::new();
        };
        let mut ports = Vec::new();
        if state.messaging {
            ports.extend([state.delivery_port, state.discovery_port]);
        }
        ports.extend(state.storage_port);
        ports
    }

    /// Whether the hub runs the delivery and discovery nodes.
    pub fn runs_messaging(&self) -> bool {
        self.state().is_ok_and(|s| s.messaging)
    }

    /// Whether the hub runs a storage node for the sessions.
    pub fn runs_storage(&self) -> bool {
        self.state().is_ok_and(|s| s.storage_port.is_some())
    }

    /// The signed peer record of the hub's storage node, which sessions bootstrap from.
    pub fn storage_spr(&self) -> Option<String> {
        self.state().ok().and_then(|s| s.storage_spr)
    }

    /// The addresses the hub's nodes last reported.
    pub fn addresses(&self) -> Option<Addresses> {
        self.state()
            .ok()
            .filter(|s| s.messaging)
            .and_then(|s| s.addresses)
    }

    pub fn running(&self) -> bool {
        self.ctl.running()
    }

    pub fn stop(&self) -> Result<()> {
        self.ctl.stop()
    }

    /// Asks the hub's daemon whether each node runs. Each question gets `STATUS_TIMEOUT`, and
    /// one left unanswered counts as no.
    pub fn health(&self) -> Health {
        let daemon = self.ctl.answers_within(self.status_timeout);
        let state = self.state().ok().filter(|_| daemon);
        let ask = |module, method, args: &[&str]| {
            self.ctl
                .call_within(module, method, args, self.status_timeout)
                .ok()
                .and_then(module_value)
        };
        let messaging = state.as_ref().is_some_and(|s| s.messaging);
        let delivery = messaging
            && ask(DELIVERY, "getNodeInfo", &["IsRunning"])
                .is_some_and(|running| running == "true");
        let discovery = messaging
            && state.as_ref().is_some_and(|state| {
                ask(DISCOVERY, "peerInfo", &[])
                    .is_some_and(|peer| listens_on(&peer, state.discovery_port))
            });
        // `isRunning` answers a bare boolean, not the `{"success", "value"}` of other calls.
        let storage = state.as_ref().is_some_and(|s| s.storage_port.is_some())
            && self
                .ctl
                .call_within(STORAGE, "isRunning", &[], self.status_timeout)
                .is_ok_and(|running| running == true);
        Health {
            daemon,
            delivery,
            discovery,
            storage,
        }
    }

    /// Stops the hub and deletes it: its ports, keys and message store. A daemon that still
    /// holds the hub's ports after `stop` keeps it, since nothing could stop it once deleted.
    pub fn remove(&self) -> Result<()> {
        self.stop()?;
        for port in self.ports() {
            if crate::devnet::port_free(port).is_err() {
                bail!(
                    "port {port} on 127.0.0.1, which the messaging hub listens on, is still \
                     taken, so its daemon may still run. Stop it with \
                     `LOGOSCTL_CONFIG_DIR={} logosctl daemon stop`, then try again",
                    self.dir.join("logosctl").display()
                );
            }
        }
        std::fs::remove_dir_all(&self.dir)
            .with_context(|| format!("deleting {}", self.dir.display()))
    }

    /// Makes the hub run what `wants` asks for, and what it ran before: picks its ports and
    /// keys the first time, starts its daemon, and creates each node that isn't there yet, e.g.
    /// after the daemon restarted.
    pub fn ensure(&self, dev_mode: bool, wants: Wants) -> Result<()> {
        let mut state = if self.exists() {
            self.state()?
        } else {
            std::fs::create_dir_all(&self.dir)?;
            State::new()?
        };
        state.messaging |= wants.messaging;
        if wants.storage && state.storage_port.is_none() {
            let taken = [state.delivery_port, state.discovery_port];
            let mut port = free_port()?;
            while taken.contains(&port) {
                port = free_port()?;
            }
            state.storage_port = Some(port);
        }
        self.save(&state)?;
        let modules = [DELIVERY.to_owned(), DISCOVERY.to_owned()];
        if state.messaging && !state.installed {
            eprintln!("Starting the messaging hub and installing {DELIVERY}, {DISCOVERY}...");
        }
        let storage = state
            .storage_port
            .map_or_else(crate::storage::isolated, crate::storage::hub);
        self.ctl.start_with_storage(dev_mode, &storage)?;
        if state.messaging {
            if !state.installed {
                self.ctl.install(&modules)?;
                state.installed = true;
                self.save(&state)?;
            }
            self.ctl.load(&modules)?;
            let addresses = Addresses {
                delivery: self.ensure_delivery(&state)?,
                discovery: self.ensure_discovery(&state)?,
            };
            if state.addresses.as_ref() != Some(&addresses) {
                state.addresses = Some(addresses);
                self.save(&state)?;
            }
        }
        if state.storage_port.is_some() {
            let spr = self.started_storage_spr()?;
            if state.storage_spr.as_ref() != Some(&spr) {
                state.storage_spr = Some(spr);
                self.save(&state)?;
            }
        }
        Ok(())
    }

    /// The storage node's signed peer record, once the node runs: the daemon starts it on its
    /// own, a moment after it starts.
    fn started_storage_spr(&self) -> Result<String> {
        let deadline = std::time::Instant::now() + STORAGE_START_TIMEOUT;
        loop {
            if let Some(spr) = self
                .node_info(STORAGE, "spr", &[])?
                .and_then(|spr| spr.as_str().map(str::to_owned))
                .filter(|spr| spr.starts_with("spr:"))
            {
                return Ok(spr);
            }
            if std::time::Instant::now() > deadline {
                bail!(
                    "the hub's storage node didn't start within {} s. See {}",
                    STORAGE_START_TIMEOUT.as_secs(),
                    self.dir.join("logosctl").join("logs").display()
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Creates the delivery node unless it exists, and starts it: starting a started node
    /// does nothing, and starts one that an interrupted `up` created.
    fn ensure_delivery(&self, state: &State) -> Result<String> {
        let info = || self.node_info(DELIVERY, "getNodeInfo", &["MyMultiaddresses"]);
        if info()?.is_none() {
            let config = delivery_config(state, &self.dir.join("store.sqlite3"));
            self.module_call(DELIVERY, "createNode", &[&config])?;
        }
        self.module_call(DELIVERY, "start", &[])?;
        let addrs = info()?.unwrap_or_default();
        let addrs = addrs.as_str().unwrap_or_default();
        let wanted = format!("{}/p2p/", local_address(state.delivery_port));
        addrs
            .split(',')
            .map(str::trim)
            .find(|a| a.len() > wanted.len() && a.starts_with(&wanted))
            .map(str::to_owned)
            .with_context(|| other_settings("delivery", addrs))
    }

    /// The discovery node, as `ensure_delivery` does it, with its discovery service started.
    fn ensure_discovery(&self, state: &State) -> Result<String> {
        let info = || self.node_info(DISCOVERY, "peerInfo", &[]);
        if info()?.is_none() {
            self.module_call(DISCOVERY, "createNode", &[&discovery_config(state)])?;
        }
        self.module_call(DISCOVERY, "start", &[])?;
        self.module_call(DISCOVERY, "discoStart", &[])?;
        let peer = info()?.unwrap_or_default();
        let id = peer["peerId"].as_str().unwrap_or_default();
        if !listens_on(&peer, state.discovery_port) || id.is_empty() {
            return Err(other_settings("discovery", &peer.to_string()));
        }
        Ok(format!("{}/p2p/{id}", local_address(state.discovery_port)))
    }

    /// A module's info about its node, or `None` when the module says it has none yet. A
    /// failing `logosctl` is an error, not a missing node.
    fn node_info(&self, module: &str, method: &str, args: &[&str]) -> Result<Option<Value>> {
        Ok(module_value(self.ctl.call(module, method, args)?))
    }

    /// Calls a module that answers `{"success", "value", "error"}`, and returns its value.
    fn module_call(&self, module: &str, method: &str, args: &[&str]) -> Result<Value> {
        let reply = self.ctl.call(module, method, args)?;
        if reply["success"] != true {
            let error = reply["error"].as_str().unwrap_or("no error message");
            bail!("{module}.{method} failed: {error}");
        }
        Ok(reply["value"].clone())
    }
}

impl State {
    fn new() -> Result<Self> {
        let delivery_port = free_port()?;
        let mut discovery_port = free_port()?;
        while discovery_port == delivery_port {
            discovery_port = free_port()?;
        }
        Ok(Self {
            delivery_port,
            discovery_port,
            delivery_key: crate::devnet::random_key()?,
            discovery_key: format!("{SECP256K1_KEY_HEADER}{}", crate::devnet::random_key()?),
            installed: false,
            addresses: None,
            messaging: false,
            storage_port: None,
            storage_spr: None,
        })
    }
}

/// The hub's delivery node: Core, so it relays, serves filter and lightpush, and stores
/// messages, which Edge nodes need before they count as connected.
fn delivery_config(state: &State, store: &Path) -> String {
    let mut overrides = network_overrides();
    overrides.insert("tcp-port".into(), json!(state.delivery_port));
    overrides.insert("store".into(), json!(true));
    overrides.insert(
        "store-message-db-url".into(),
        json!(format!("sqlite://{}", store.display())),
    );
    overrides.insert("nodekey".into(), json!(state.delivery_key));
    json!({ "mode": "Core", "messagingOverrides": overrides }).to_string()
}

/// The delivery overrides the hub and its clients share: the hub's cluster without a preset,
/// on loopback only, with no public-address or peer discovery.
fn network_overrides() -> serde_json::Map<String, Value> {
    let Value::Object(overrides) = json!({
        "cluster-id": CLUSTER,
        "num-shards-in-network": SHARDS,
        "listen-address": "127.0.0.1",
        "nat": "none",
        "discv5-discovery": false,
        "quic-support": false,
    }) else {
        unreachable!("a JSON object literal")
    };
    overrides
}

fn discovery_config(state: &State) -> String {
    json!({
        "addrs": [local_address(state.discovery_port)],
        "mountKad": true,
        "mountServiceDiscovery": true,
        "privKey": state.discovery_key,
    })
    .to_string()
}

/// The value of a module's `{"success", "value", "error"}` reply, when it succeeded.
fn module_value(reply: Value) -> Option<Value> {
    (reply["success"] == true).then(|| reply["value"].clone())
}

fn local_address(port: u16) -> String {
    format!("/ip4/127.0.0.1/tcp/{port}")
}

/// Whether a libp2p node's `peerInfo` lists `port` on 127.0.0.1: a node that isn't started
/// lists no address.
fn listens_on(peer: &Value, port: u16) -> bool {
    let wanted = local_address(port);
    peer["addrs"]
        .as_array()
        .is_some_and(|addrs| addrs.iter().any(|a| a.as_str() == Some(wanted.as_str())))
}

fn other_settings(node: &str, reported: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the hub's {node} node runs with other settings (it reports {reported}). \
         `logos-devnet down`, then `up`, restarts it"
    )
}

/// A port on 127.0.0.1 that nothing listens on now.
fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).context("finding a free port")?;
    Ok(listener.local_addr()?.port())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::{calls, fake_logosctl};

    const MESSAGING: Wants = Wants {
        messaging: true,
        storage: false,
    };

    const DELIVERY_ADDR: &str = "/ip4/127.0.0.1/tcp/61310/p2p/16Uiu2HAmDelivery";
    const PEER: &str = "16Uiu2HAmDiscovery";

    fn state() -> State {
        State {
            delivery_port: 61310,
            discovery_port: 61320,
            delivery_key: "aa".repeat(32),
            discovery_key: format!("{SECP256K1_KEY_HEADER}{}", "bb".repeat(32)),
            installed: true,
            addresses: None,
            messaging: true,
            storage_port: None,
            storage_spr: None,
        }
    }

    /// A hub whose logosctl answers as the modules do: a node exists once `createNode` made it,
    /// or from the start when `nodes` is set. Everything else just succeeds.
    fn hub(dir: &Path, nodes: bool) -> Hub {
        let made = dir.join("made");
        std::fs::create_dir_all(&made).unwrap();
        if nodes {
            for module in [DELIVERY, DISCOVERY] {
                std::fs::write(made.join(module), "").unwrap();
            }
        }
        let made = made.display();
        let ctl = fake_logosctl(
            dir,
            &format!(
                r#"ok() {{ echo "{{\"status\":\"ok\",\"result\":$1}}"; }}
case "$2 $3 $4" in
  "daemon status "*) [ ! -e '{made}/stopped' ] ;;
  "daemon stop "*) touch '{made}/stopped' ;;
  "daemon start "*) rm -f '{made}/stopped'; echo "$HOME" > '{made}/home' ;;
  "call storage_module dataDir") ok "{{\"success\":true,\"value\":\"$(cat '{made}/home')/.logos_storage/data\"}}" ;;
  "call storage_module spr") ok '{{"success":true,"value":"spr:hub"}}' ;;
  "call storage_module isRunning") ok true ;;
  "module ls "*) echo '[{{"name":"{DELIVERY}","status":"loaded"}},{{"name":"{DISCOVERY}","status":"loaded"}}]' ;;
  *" createNode") touch '{made}'/"$3"; ok '{{"success":true,"value":null}}' ;;
  *" getNodeInfo") [ -e '{made}/{DELIVERY}' ] \
      && {{ [ "$5" = str:IsRunning ] && ok '{{"success":true,"value":"true"}}' \
        || ok '{{"success":true,"value":"{DELIVERY_ADDR}"}}'; }} \
      || ok '{{"success":false,"error":"Context not initialized"}}' ;;
  *" peerInfo") [ -e '{made}/{DISCOVERY}' ] \
      && ok '{{"success":true,"value":{{"addrs":["/ip4/127.0.0.1/tcp/61320"],"peerId":"{PEER}"}}}}' \
      || ok '{{"success":false,"error":"No libp2p context"}}' ;;
  *) ok '{{"success":true,"value":null}}' ;;
esac"#
            ),
        );
        // The daemon's directory, as `start` makes it.
        std::fs::create_dir_all(dir.join("session")).unwrap();
        let hub = Hub::with_logosctl(dir.join("messaging"), ctl);
        std::fs::create_dir_all(&hub.dir).unwrap();
        hub.save(&state()).unwrap();
        hub
    }

    #[test]
    fn a_storage_hub_records_its_bootstrap_and_runs_no_messaging_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path(), false);
        hub.save(&State {
            messaging: false,
            installed: false,
            ..state()
        })
        .unwrap();
        hub.ensure(
            false,
            Wants {
                messaging: false,
                storage: true,
            },
        )
        .unwrap();
        assert_eq!(hub.storage_spr().as_deref(), Some("spr:hub"));
        assert!(hub.runs_storage() && !hub.runs_messaging());
        let log = calls(dir.path());
        assert!(
            !log.contains("createNode") && !log.contains("package install"),
            "{log}"
        );
        let port = hub.state().unwrap().storage_port.unwrap();
        assert_eq!(hub.ports(), [port], "only the storage port is the hub's");
        let config: Value = serde_json::from_str(
            &std::fs::read_to_string(crate::storage::config_file(&hub.ctl.home())).unwrap(),
        )
        .unwrap();
        assert_eq!(config, crate::storage::hub(port));
        assert!(hub.health().storage);
        // The port stays across ensures, and storage stays on for a project without it.
        hub.ensure(false, Wants::default()).unwrap();
        assert_eq!(hub.state().unwrap().storage_port, Some(port));
    }

    #[test]
    fn a_hub_from_before_storage_still_runs_messaging() {
        let old = r#"{"delivery_port":1,"discovery_port":2,"delivery_key":"","discovery_key":""}"#;
        let state: State = serde_json::from_str(old).unwrap();
        assert!(state.messaging);
        assert_eq!(state.storage_port, None);
    }

    #[test]
    fn creates_and_starts_both_nodes_on_a_fresh_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path(), false);
        hub.ensure(false, MESSAGING).unwrap();
        let addresses = hub.addresses().unwrap();
        assert_eq!(addresses.delivery, DELIVERY_ADDR);
        let log = calls(dir.path());
        let order: Vec<&str> = log
            .lines()
            .filter_map(|l| l.strip_prefix("--json call "))
            .map(|l| l.split(" str:").next().unwrap())
            .collect();
        assert_eq!(
            order,
            [
                "delivery_module getNodeInfo",
                "delivery_module createNode",
                "delivery_module start",
                "delivery_module getNodeInfo",
                "libp2p_module peerInfo",
                "libp2p_module createNode",
                "libp2p_module start",
                "libp2p_module discoStart",
                "libp2p_module peerInfo",
            ]
        );
    }

    #[test]
    fn starts_nodes_that_exist_but_may_not_run() {
        let dir = tempfile::tempdir().unwrap();
        hub(dir.path(), true).ensure(false, MESSAGING).unwrap();
        let log = calls(dir.path());
        assert!(!log.contains("createNode"), "{log}");
        for start in [
            "delivery_module start",
            "libp2p_module start",
            "libp2p_module discoStart",
        ] {
            assert!(log.contains(start), "{start}: {log}");
        }
    }

    #[test]
    fn reports_which_nodes_run() {
        let dir = tempfile::tempdir().unwrap();
        let all = Health {
            daemon: true,
            delivery: true,
            discovery: true,
            storage: false,
        };
        assert_eq!(hub(dir.path(), true).health(), all);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            hub(dir.path(), false).health(),
            Health {
                delivery: false,
                discovery: false,
                ..all
            }
        );
    }

    #[test]
    fn reports_a_node_that_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path(), true);
        // The discovery node isn't there; the delivery node is.
        std::fs::remove_file(dir.path().join("made").join(DISCOVERY)).unwrap();
        let health = hub.health();
        assert!(
            health.daemon && health.delivery && !health.discovery,
            "{health:?}"
        );
        hub.stop().unwrap();
        assert_eq!(
            hub.health(),
            Health {
                daemon: false,
                delivery: false,
                discovery: false,
                storage: false,
            }
        );
    }

    #[test]
    fn a_stuck_daemon_counts_as_a_node_that_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = fake_logosctl(
            dir.path(),
            r#"case "$2 $3" in
  "daemon status") exit 0 ;;
  *) exec sleep 30 ;;
esac"#,
        );
        std::fs::create_dir_all(dir.path().join("session")).unwrap();
        let hub = Hub::with_logosctl(dir.path().join("messaging"), ctl);
        std::fs::create_dir_all(&hub.dir).unwrap();
        hub.save(&state()).unwrap();
        let started = std::time::Instant::now();
        let health = hub.health();
        assert!(
            health.daemon && !health.delivery && !health.discovery,
            "{health:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "gave up on each question"
        );
    }

    #[test]
    fn keeps_a_hub_whose_ports_are_still_taken() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path(), true);
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        hub.save(&State {
            delivery_port: held.local_addr().unwrap().port(),
            ..state()
        })
        .unwrap();
        let err = hub.remove().unwrap_err();
        assert!(err.to_string().contains("still taken"), "{err}");
        assert!(hub.exists());
    }

    #[test]
    fn removing_it_deletes_its_keys_and_store() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path(), true);
        // Ports 0, which nothing holds: the fixed ones may be another program's here.
        hub.save(&State {
            delivery_port: 0,
            discovery_port: 0,
            ..state()
        })
        .unwrap();
        std::fs::write(hub.dir.join("store.sqlite3"), "").unwrap();
        hub.remove().unwrap();
        assert!(!hub.dir.exists());
        assert!(!hub.exists());
        assert!(calls(dir.path()).contains("daemon stop"), "stopped first");
    }

    #[test]
    fn a_failing_logosctl_is_not_a_missing_node() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = fake_logosctl(
            dir.path(),
            r#"case "$2 $3" in
  "daemon status") exit 0 ;;
  "module ls") echo '[{"name":"delivery_module","status":"loaded"},{"name":"libp2p_module","status":"loaded"}]' ;;
  *) echo '{"status":"error","message":"no listener"}' ;;
esac"#,
        );
        let hub = Hub::with_logosctl(dir.path().join("messaging"), ctl);
        std::fs::create_dir_all(&hub.dir).unwrap();
        hub.save(&state()).unwrap();
        let err = hub.ensure(false, MESSAGING).unwrap_err();
        assert!(format!("{err:#}").contains("no listener"), "{err:#}");
        assert!(!calls(dir.path()).contains("createNode"));
    }

    #[test]
    fn leaves_running_nodes_alone_and_records_their_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path(), true);
        hub.ensure(false, MESSAGING).unwrap();
        let addresses = hub.addresses().unwrap();
        assert_eq!(addresses.delivery, DELIVERY_ADDR);
        assert_eq!(
            addresses.discovery,
            format!("/ip4/127.0.0.1/tcp/61320/p2p/{PEER}")
        );
        assert!(!calls(dir.path()).contains("createNode"));
        assert_eq!(hub.addresses(), Some(addresses));
    }

    #[test]
    fn refuses_nodes_on_other_ports() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path(), true);
        hub.save(&State {
            delivery_port: 1,
            ..state()
        })
        .unwrap();
        let err = hub.ensure(false, MESSAGING).unwrap_err();
        assert!(err.to_string().contains("other settings"), "{err}");
    }

    #[test]
    fn installs_its_modules_once() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub(dir.path(), true);
        hub.save(&State {
            installed: false,
            ..state()
        })
        .unwrap();
        hub.ensure(false, MESSAGING).unwrap();
        hub.ensure(false, MESSAGING).unwrap();
        let log = calls(dir.path());
        assert_eq!(log.matches("package install").count(), 2, "{log}");
        assert!(
            log.contains("package install delivery_module --yes"),
            "{log}"
        );
        assert!(log.contains("package install libp2p_module --yes"), "{log}");
    }

    #[test]
    fn the_hubs_delivery_node_stores_messages_in_the_home() {
        let config: Value =
            serde_json::from_str(&delivery_config(&state(), Path::new("/h/messaging/s.db")))
                .unwrap();
        let o = &config["messagingOverrides"];
        assert_eq!(config["mode"], "Core");
        assert!(config.get("preset").is_none(), "no public network");
        assert_eq!(o["store"], true);
        assert_eq!(o["store-message-db-url"], "sqlite:///h/messaging/s.db");
        assert_eq!(o["tcp-port"], 61310);
        assert_eq!(o["listen-address"], "127.0.0.1");
        assert_eq!(o["nodekey"], "aa".repeat(32));
    }

    #[test]
    fn the_discovery_node_keeps_its_key_and_port() {
        let config: Value = serde_json::from_str(&discovery_config(&state())).unwrap();
        assert_eq!(config["addrs"], json!(["/ip4/127.0.0.1/tcp/61320"]));
        assert_eq!(config["privKey"], state().discovery_key);
        assert_eq!(config["mountServiceDiscovery"], true);
    }

    #[test]
    fn apps_join_through_the_hub_only() {
        let config: Value = serde_json::from_str(
            &Addresses {
                delivery: DELIVERY_ADDR.into(),
                discovery: String::new(),
            }
            .client_config(),
        )
        .unwrap();
        let o = &config["messagingOverrides"];
        assert_eq!(config["mode"], "Edge");
        assert!(config.get("preset").is_none());
        assert_eq!(o["entry-node"], json!([DELIVERY_ADDR]));
        assert_eq!(o["cluster-id"], CLUSTER);
        assert!(o.get("tcp-port").is_none(), "any free port");
    }

    #[test]
    fn a_new_hub_gets_two_ports_and_keys_of_its_own() {
        let a = State::new().unwrap();
        let b = State::new().unwrap();
        assert_ne!(a.delivery_port, a.discovery_port);
        assert_eq!(a.delivery_key.len(), 64);
        assert!(a.discovery_key.starts_with(SECP256K1_KEY_HEADER));
        assert_eq!(a.discovery_key.len(), 8 + 64);
        assert_ne!(a.delivery_key, b.delivery_key);
    }
}
