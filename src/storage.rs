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

/// The keys logos-devnet sets. The module may save others after it starts. `bootstrap-node`
/// is compared through `Config::bootstrap` instead: the hub's record changes on each of its
/// restarts, and the old one still reaches it.
const OWNED: [&str; 6] = [
    "no-bootstrap-node",
    "listen-ip",
    "listen-port",
    "nat",
    "mix-enabled",
    "data-dir",
];

/// A daemon's storage config, and which node it bootstraps from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    json: Value,
    /// `none`, or the peer id of the node it bootstraps from.
    bootstrap: String,
}

/// A storage node on 127.0.0.1 with no bootstrap node, so it joins no network: uploads and
/// downloads stay in its own store.
pub fn isolated() -> Config {
    Config {
        json: json!({
            "config-version": CONFIG_VERSION,
            "no-bootstrap-node": true,
            "listen-ip": "127.0.0.1",
            "listen-port": 0,
            "nat": "extip:127.0.0.1",
            "mix-enabled": false,
        }),
        bootstrap: "none".into(),
    }
}

/// The hub's storage node: on 127.0.0.1 at `port`, with no bootstrap node of its own.
pub fn hub(port: u16) -> Config {
    let mut config = isolated();
    config.json["listen-port"] = json!(port);
    config
}

/// A session's storage node, which joins the network of the hub's node, whose signed peer
/// record is `spr` and peer id `peer`, and no other.
pub fn client(spr: &str, peer: &str) -> Config {
    let mut config = isolated();
    let fields = config.json.as_object_mut().expect("a JSON object");
    fields.remove("no-bootstrap-node");
    fields.insert("bootstrap-node".into(), json!([spr]));
    config.bootstrap = peer.into();
    config
}

impl Config {
    /// The config file's content for a daemon whose `HOME` is `home`.
    fn content(&self, home: &Path) -> Value {
        let mut json = self.json.clone();
        json["data-dir"] = json!(data_dir(home));
        json
    }
}

/// Where the module reads its config in a daemon whose `HOME` is `home`.
pub fn config_file(home: &Path) -> PathBuf {
    home.join(".logos_storage").join("config.json")
}

/// The data directory the module uses in a daemon whose `HOME` is `home`.
pub fn data_dir(home: &Path) -> PathBuf {
    home.join(".logos_storage").join("data")
}

/// Which node the config in `home` bootstraps from, as `Config::bootstrap`.
fn bootstrap_file(home: &Path) -> PathBuf {
    home.join("storage-bootstrap")
}

/// Whether the config in `home` differs from `config`, so a daemon running on it has to
/// restart to take `config`.
pub fn differs(home: &Path, config: &Config) -> bool {
    let current: Value = std::fs::read_to_string(config_file(home))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    let wanted = config.content(home);
    let bootstrap = std::fs::read_to_string(bootstrap_file(home)).unwrap_or_default();
    OWNED
        .iter()
        .any(|key| current.get(*key) != wanted.get(*key))
        || bootstrap.trim() != config.bootstrap
}

/// Writes `config` into `home`, for a daemon that starts on it.
pub fn write(home: &Path, config: &Config) -> Result<()> {
    let path = config_file(home);
    let dir = path.parent().expect("a config file has a directory");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, config.content(home).to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    let marker = bootstrap_file(home);
    std::fs::write(&marker, &config.bootstrap)
        .with_context(|| format!("writing {}", marker.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_isolated_node_listens_on_loopback_and_joins_no_network() {
        let config = isolated().json;
        assert_eq!(config["no-bootstrap-node"], true);
        assert_eq!(config["listen-ip"], "127.0.0.1");
        assert_eq!(config["mix-enabled"], false);
        assert!(config.get("network").is_none(), "no preset network");
        assert_eq!(config["config-version"], 3);
    }

    #[test]
    fn sessions_bootstrap_from_the_hub_alone() {
        let hub = hub(4041).json;
        assert_eq!(hub["listen-port"], 4041);
        assert_eq!(hub["no-bootstrap-node"], true);
        let client = client("spr:hub", "16Uiu2HAmHub").json;
        assert_eq!(client["bootstrap-node"], json!(["spr:hub"]));
        assert!(client.get("no-bootstrap-node").is_none());
        assert!(client.get("network").is_none(), "no preset network");
        assert_eq!(client["listen-ip"], "127.0.0.1");
    }

    #[test]
    fn keeps_the_data_in_the_daemons_own_home() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), &isolated()).unwrap();
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(config_file(home.path())).unwrap())
                .unwrap();
        assert_eq!(saved["data-dir"], json!(data_dir(home.path())));
        // A home moved elsewhere: the module saved the old path, so the config differs.
        let moved = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(config_file(moved.path()).parent().unwrap()).unwrap();
        std::fs::copy(config_file(home.path()), config_file(moved.path())).unwrap();
        std::fs::copy(bootstrap_file(home.path()), bootstrap_file(moved.path())).unwrap();
        assert!(differs(moved.path(), &isolated()));
    }

    #[test]
    fn a_new_record_of_the_same_hub_is_the_same_config() {
        let home = tempfile::tempdir().unwrap();
        assert!(
            differs(home.path(), &client("spr:first", "peer-a")),
            "nothing yet"
        );
        write(home.path(), &client("spr:first", "peer-a")).unwrap();
        assert!(!differs(
            home.path(),
            &client("spr:after-a-restart", "peer-a")
        ));
        assert!(
            differs(home.path(), &client("spr:new-hub", "peer-b")),
            "another hub"
        );
        assert!(
            differs(home.path(), &isolated()),
            "leaving the hub's network"
        );
    }

    #[test]
    fn ignores_what_the_module_adds() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), &isolated()).unwrap();
        let mut saved = isolated().content(home.path());
        saved["log-level"] = json!("info");
        std::fs::write(config_file(home.path()), saved.to_string()).unwrap();
        assert!(!differs(home.path(), &isolated()));
        let mut other = isolated();
        other.json["listen-port"] = json!(4040);
        assert!(differs(home.path(), &other), "a key of ours changed");
    }
}
