//! Logos Storage in the daemons logos-devnet starts. Every logosctl daemon starts
//! `storage_module`, which reads `$HOME/.logos_storage/config.json` and keeps its data next to
//! it: one store per user, which a second daemon can't open while the first runs. So each daemon
//! gets a `HOME` of its own, with a config written there before it starts: a node on
//! 127.0.0.1 that joins no public network. With `[storage]`, the messaging hub's daemon runs a
//! storage node on a fixed port, and every session's node takes it as its only bootstrap node,
//! so the sessions form a storage network of their own.
//!
//! logos-devnet never calls `storage_module.init`: the module saves the config it's given into
//! the daemon's `HOME`, and outside a devnet daemon that's the user's own.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The `config-version` of `storage_module` 3.0.2, so it reads these configs as written.
const CONFIG_VERSION: u32 = 3;

/// The keys logos-devnet sets. The module adds others, e.g. `data-dir`, after it starts.
const OWNED: [&str; 6] = [
    "no-bootstrap-node",
    "bootstrap-node",
    "listen-ip",
    "listen-port",
    "nat",
    "mix-enabled",
];

/// A storage node on 127.0.0.1 with no bootstrap node, so it joins no network: uploads and
/// downloads stay in its own store.
pub fn isolated() -> Value {
    json!({
        "config-version": CONFIG_VERSION,
        "no-bootstrap-node": true,
        "listen-ip": "127.0.0.1",
        "listen-port": 0,
        "nat": "extip:127.0.0.1",
        "mix-enabled": false,
    })
}

/// The hub's storage node: on 127.0.0.1 at `port`, with no bootstrap node of its own.
pub fn hub(port: u16) -> Value {
    let mut config = isolated();
    config["listen-port"] = json!(port);
    config
}

/// A session's storage node, which joins the network of the node whose signed peer record is
/// `spr`, the hub's, and no other.
pub fn client(spr: &str) -> Value {
    let mut config = isolated();
    let fields = config.as_object_mut().expect("a JSON object");
    fields.remove("no-bootstrap-node");
    fields.insert("bootstrap-node".into(), json!([spr]));
    config
}

/// Where the module reads its config in a daemon whose `HOME` is `home`.
pub fn config_file(home: &Path) -> PathBuf {
    home.join(".logos_storage").join("config.json")
}

/// The data directory the module uses in a daemon whose `HOME` is `home`.
pub fn data_dir(home: &Path) -> PathBuf {
    home.join(".logos_storage").join("data")
}

/// Writes `config` into `home` unless the file there already holds its keys. True when a
/// running daemon has to restart to take it. A new record of the same bootstrap node needs
/// none: the hub's record changes on each of its restarts, and the old one still reaches it.
pub fn write_config(home: &Path, config: &Value) -> Result<bool> {
    let path = config_file(home);
    let current: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    let differs = |key: &&str| current.get(*key) != config.get(*key);
    if !OWNED.iter().any(differs) {
        return Ok(false);
    }
    let bootstraps = |config: &Value| {
        config
            .get("bootstrap-node")
            .map(|b| b.as_array().map(Vec::len))
    };
    let restart = OWNED
        .iter()
        .filter(|key| **key != "bootstrap-node")
        .any(differs)
        || bootstraps(&current) != bootstraps(config);
    let dir = path.parent().expect("a config file has a directory");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, config.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(restart)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_isolated_node_listens_on_loopback_and_joins_no_network() {
        let config = isolated();
        assert_eq!(config["no-bootstrap-node"], true);
        assert_eq!(config["listen-ip"], "127.0.0.1");
        assert_eq!(config["mix-enabled"], false);
        assert!(config.get("network").is_none(), "no preset network");
        assert_eq!(config["config-version"], 3);
    }

    #[test]
    fn sessions_bootstrap_from_the_hub_alone() {
        let hub = hub(4041);
        assert_eq!(hub["listen-port"], 4041);
        assert_eq!(hub["no-bootstrap-node"], true);
        let client = client("spr:hub");
        assert_eq!(client["bootstrap-node"], json!(["spr:hub"]));
        assert!(client.get("no-bootstrap-node").is_none());
        assert!(client.get("network").is_none(), "no preset network");
        assert_eq!(client["listen-ip"], "127.0.0.1");
    }

    #[test]
    fn a_new_record_of_the_same_hub_needs_no_restart() {
        let home = tempfile::tempdir().unwrap();
        assert!(write_config(home.path(), &client("spr:first")).unwrap());
        assert!(
            !write_config(home.path(), &client("spr:after-a-restart")).unwrap(),
            "same hub"
        );
        let saved = std::fs::read_to_string(config_file(home.path())).unwrap();
        assert!(
            saved.contains("spr:after-a-restart"),
            "the file has the new one"
        );
        assert!(
            write_config(home.path(), &isolated()).unwrap(),
            "leaving the hub's network"
        );
    }

    #[test]
    fn writes_a_config_once_and_ignores_what_the_module_adds() {
        let home = tempfile::tempdir().unwrap();
        assert!(
            write_config(home.path(), &isolated()).unwrap(),
            "first write"
        );
        assert!(
            !write_config(home.path(), &isolated()).unwrap(),
            "already there"
        );
        // The module saves the config back with its data-dir after it starts.
        let mut saved = isolated();
        saved["data-dir"] = json!(data_dir(home.path()));
        std::fs::write(config_file(home.path()), saved.to_string()).unwrap();
        assert!(!write_config(home.path(), &isolated()).unwrap());
        let mut other = isolated();
        other["listen-port"] = json!(4040);
        assert!(
            write_config(home.path(), &other).unwrap(),
            "a key of ours changed"
        );
    }
}
