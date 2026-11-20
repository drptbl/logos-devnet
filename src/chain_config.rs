//! The sequencer config a devnet runs on: upstream's own local config for the pinned LEZ tag
//! (`configs/debug/sequencer_config.json` on v0.3, `networks/local.json` on v0.4), with our
//! chain home, block interval and faucet. Patching upstream's file rather than shipping our own
//! keeps every field a newer tag adds.

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::account::PublicId;
use crate::home::LezLine;

/// What a devnet changes in upstream's config.
pub struct ChainOptions<'a> {
    /// Where the sequencer keeps its database and keys, as the sequencer sees the path.
    pub home: &'a str,
    /// Block interval, in the sequencer's humantime format (`1s`, `500ms`).
    pub block_time: &'a str,
    /// The faucet, funded at genesis.
    pub faucet: &'a Funded,
    /// The most user transactions a block takes.
    pub max_txs_per_block: u32,
    /// More accounts to fund at genesis.
    pub genesis_accounts: &'a [Funded],
    /// Keep upstream's own genesis accounts, whose keys are published, funded too: tools such
    /// as LEZ's examples fund themselves from them.
    pub keep_upstream_accounts: bool,
}

/// An account funded at genesis. v0.3 names it by id, v0.4 by its public key.
pub struct Funded {
    pub id: PublicId,
    /// Its public key, as 64 hex digits: what a v0.4 genesis funds.
    pub key: Option<String>,
    /// Lepta.
    pub balance: u64,
}

/// The bounds `max_txs_per_block` must fall in: upstream's mempool holds 1000 transactions.
pub const MAX_TXS_PER_BLOCK: std::ops::RangeInclusive<u32> = 1..=1000;

pub fn check_max_txs_per_block(max: u32) -> Result<()> {
    if !MAX_TXS_PER_BLOCK.contains(&max) {
        bail!(
            "max_txs_per_block must be {} to {}",
            MAX_TXS_PER_BLOCK.start(),
            MAX_TXS_PER_BLOCK.end()
        );
    }
    Ok(())
}

/// Returns upstream's config with `opts` applied, in `line`'s schema. Unless the options keep
/// them, the faucet and the given accounts are the only ones funded: upstream's supply accounts
/// have published keys.
pub fn patch(line: LezLine, mut upstream: Value, opts: &ChainOptions) -> Result<Value> {
    let config = upstream
        .as_object_mut()
        .context("the upstream sequencer config is not a JSON object")?;
    config.insert("home".into(), json!(opts.home));
    config.insert("block_create_timeout".into(), json!(opts.block_time));
    check_max_txs_per_block(opts.max_txs_per_block)?;
    config.insert("max_num_tx_in_block".into(), json!(opts.max_txs_per_block));
    // Both would listen on every interface: metrics on 9000 and gossip on a UDP port. A single
    // devnet sequencer needs neither. v0.4 refuses fields it doesn't know, and renamed one.
    let metrics = match line {
        LezLine::V03 => "metrics_address",
        LezLine::V04 => "metrics_listen_address",
    };
    config.insert(metrics.into(), Value::Null);
    config.insert("gossip".into(), Value::Null);

    let funded = std::iter::once(opts.faucet).chain(opts.genesis_accounts);
    let posting_timeout = match line {
        LezLine::V03 => {
            let genesis = config
                .entry("genesis")
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .context("the upstream config's `genesis` is not a list")?;
            if !opts.keep_upstream_accounts {
                genesis.retain(|action| action.get("supply_account").is_none());
            }
            for account in funded {
                genesis.push(json!({
                    "supply_account": { "account_id": account.id.as_str(), "balance": account.balance }
                }));
            }
            config.get("bedrock_config").map(|b| &b["channel_params"])
        }
        LezLine::V04 => {
            let genesis = config
                .get_mut("genesis")
                .and_then(Value::as_object_mut)
                .context("the upstream config's `genesis` is not an object")?;
            let actions = actions(genesis)?;
            if !opts.keep_upstream_accounts {
                actions.retain(|action| {
                    action.get("supply_account").is_none()
                        && action.get("supply_private_account").is_none()
                });
            }
            for account in funded {
                let key = account.key.as_deref().with_context(|| {
                    format!(
                        "{} has no public key on record, and a LEZ v0.4 genesis funds by key",
                        account.id.wallet_form()
                    )
                })?;
                actions.push(json!({
                    "supply_account": {
                        "recipient": { "identified": { "key": key } },
                        "balance": account.balance
                    }
                }));
            }
            config.get("genesis").map(|g| &g["channel_params"])
        }
    }
    .and_then(|params| params["posting_timeout"].as_u64());

    let block_secs = block_time_secs(opts.block_time)?;
    if let Some(timeout) = posting_timeout
        && block_secs >= timeout as f64
    {
        bail!(
            "block time {} must be under the channel's posting_timeout of {timeout}s",
            opts.block_time
        );
    }
    Ok(upstream)
}

/// A v0.4 genesis's `actions`.
fn actions(genesis: &mut Map<String, Value>) -> Result<&mut Vec<Value>> {
    genesis
        .entry("actions")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("the upstream config's `genesis.actions` is not a list")
}

/// Seconds in a `1s` / `500ms` / `2m` block time: the forms the sequencer's humantime config
/// takes that a block interval needs. Refuses anything else, and zero.
pub fn block_time_secs(text: &str) -> Result<f64> {
    let (number, unit) = text
        .find(|c: char| !c.is_ascii_digit())
        .map(|i| text.split_at(i))
        .context("a block time needs a unit, such as 1s or 500ms")?;
    let number: f64 = number
        .parse()
        .with_context(|| format!("not a block time: {text}"))?;
    let secs = match unit {
        "ms" => number / 1000.0,
        "s" => number,
        "m" => number * 60.0,
        _ => bail!("not a block time: {text} (use ms, s or m)"),
    };
    if secs == 0.0 {
        bail!("the block time can't be zero");
    }
    Ok(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::home::LezLine;

    fn upstream() -> Value {
        json!({
            "home": ".",
            "block_create_timeout": "15s",
            "bedrock_config": { "channel_params": { "posting_timeout": 25 } },
            "genesis": [
                { "supply_account": { "account_id": "Stock", "balance": 10 } },
                { "stake_sequencer": { "sequencer_key": "k" } }
            ],
            "future_field": 7
        })
    }

    const FAUCET: &str = "HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d";

    const USER: &str = "6YvesSEDWW25kpi8SSki2c8o5m3uACsigvGnNQ6wMruh";

    fn patched_with(block_time: &str, max_txs_per_block: u32) -> Result<Value> {
        let faucet = Funded {
            id: PublicId::parse(FAUCET).unwrap(),
            key: None,
            balance: 1_000,
        };
        let users = [Funded {
            id: PublicId::parse(USER).unwrap(),
            key: None,
            balance: 7,
        }];
        patch(
            LezLine::V03,
            upstream(),
            &ChainOptions {
                home: "/devnet/chain",
                block_time,
                faucet: &faucet,
                max_txs_per_block,
                genesis_accounts: &users,
                keep_upstream_accounts: false,
            },
        )
    }

    fn patched(block_time: &str) -> Result<Value> {
        patched_with(block_time, 20)
    }

    #[test]
    fn sets_home_and_block_time_and_turns_listeners_off() {
        let out = patched("1s").unwrap();
        assert_eq!(out["home"], "/devnet/chain");
        assert_eq!(out["block_create_timeout"], "1s");
        assert!(out["metrics_address"].is_null());
        assert!(out["gossip"].is_null());
        assert_eq!(out["future_field"], 7, "fields we don't know about stay");
    }

    #[test]
    fn funds_only_the_faucet_and_the_named_accounts() {
        let out = patched("1s").unwrap();
        let genesis = out["genesis"].as_array().unwrap();
        assert_eq!(
            genesis,
            &vec![
                json!({ "stake_sequencer": { "sequencer_key": "k" } }),
                json!({ "supply_account": { "account_id": FAUCET, "balance": 1000 } }),
                json!({ "supply_account": { "account_id": USER, "balance": 7 } }),
            ]
        );
    }

    #[test]
    fn sets_and_bounds_the_transactions_per_block() {
        assert_eq!(patched_with("1s", 200).unwrap()["max_num_tx_in_block"], 200);
        assert!(patched_with("1s", 0).is_err());
        assert!(patched_with("1s", 1001).is_err());
    }

    #[test]
    fn refuses_a_block_time_at_or_over_the_posting_timeout() {
        assert!(patched("25s").is_err());
        assert!(patched("500ms").is_ok());
    }

    /// LEZ v0.4.0-rc3's `networks/local.json`, cut down.
    fn upstream_v04() -> Value {
        json!({
            "home": ".",
            "max_num_tx_in_block": 20,
            "block_create_timeout": "15s",
            "bedrock_config": { "channel_id": "0101", "node_url": "http://localhost:18080" },
            "genesis": {
                "channel_params": { "posting_timeout": 25, "exit_delay": 100 },
                "actions": [
                    { "supply_account": { "recipient": { "identified": { "key": "aa" } }, "balance": 1 } },
                    { "supply_private_account": { "address": "Priv", "balance": 2 } },
                    { "stake_sequencer": { "sequencer_key": "k" } }
                ]
            },
            "gossip": { "listen_addr": "/ip4/0.0.0.0/udp/0/quic-v1", "bootstrap_peers": [] }
        })
    }

    const FAUCET_KEY: &str = "6aa4c365699b89d963ef85a59868b9612b6001f90750b4970662a62776332e81";

    fn patched_v04(user_key: Option<&str>, keep_upstream_accounts: bool) -> Result<Value> {
        let faucet = Funded {
            id: PublicId::parse(FAUCET).unwrap(),
            key: Some(FAUCET_KEY.into()),
            balance: 1_000,
        };
        let users = [Funded {
            id: PublicId::parse(USER).unwrap(),
            key: user_key.map(str::to_owned),
            balance: 7,
        }];
        patch(
            LezLine::V04,
            upstream_v04(),
            &ChainOptions {
                home: "/devnet/chain",
                block_time: "1s",
                faucet: &faucet,
                max_txs_per_block: 50,
                genesis_accounts: &users,
                keep_upstream_accounts,
            },
        )
    }

    #[test]
    fn a_v0_4_config_runs_alone_and_keeps_the_tags_channel() {
        let out = patched_v04(Some("bb"), false).unwrap();
        assert_eq!(out["home"], "/devnet/chain");
        assert_eq!(out["block_create_timeout"], "1s");
        assert_eq!(out["max_num_tx_in_block"], 50);
        // v0.4 refuses unknown fields: `metrics_address` would fail the whole config.
        assert!(out["metrics_listen_address"].is_null());
        assert!(out.get("metrics_address").is_none());
        assert!(out["gossip"].is_null());
        // rc3 needs `exit_delay`, later tags `finality_depth`: the tag's own params stay.
        assert_eq!(out["genesis"]["channel_params"]["exit_delay"], 100);
    }

    #[test]
    fn a_v0_4_genesis_funds_by_public_key() {
        let out = patched_v04(Some("bb"), false).unwrap();
        let funded = |key: &str, balance: u64| json!({ "supply_account": { "recipient": { "identified": { "key": key } }, "balance": balance } });
        assert_eq!(
            out["genesis"]["actions"].as_array().unwrap(),
            &vec![
                json!({ "stake_sequencer": { "sequencer_key": "k" } }),
                funded(FAUCET_KEY, 1000),
                funded("bb", 7),
            ]
        );
        let err = patched_v04(None, false).unwrap_err();
        assert!(format!("{err:#}").contains(USER), "{err:#}");
    }

    #[test]
    fn a_v0_4_genesis_can_keep_upstreams_published_accounts() {
        let out = patched_v04(Some("bb"), true).unwrap();
        let actions = out["genesis"]["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 5, "{actions:?}");
        assert!(actions[1].get("supply_private_account").is_some());
    }

    #[test]
    fn a_v0_4_block_time_stays_under_the_posting_timeout() {
        let mut upstream = upstream_v04();
        upstream["genesis"]["channel_params"]["posting_timeout"] = json!(1);
        let faucet = Funded {
            id: PublicId::parse(FAUCET).unwrap(),
            key: Some(FAUCET_KEY.into()),
            balance: 1,
        };
        let opts = ChainOptions {
            home: "/devnet/chain",
            block_time: "1s",
            faucet: &faucet,
            max_txs_per_block: 20,
            genesis_accounts: &[],
            keep_upstream_accounts: false,
        };
        assert!(patch(LezLine::V04, upstream, &opts).is_err());
    }

    #[test]
    fn refuses_malformed_block_times() {
        for bad in ["1", "s", "1h", "0s", ""] {
            assert!(block_time_secs(bad).is_err(), "{bad:?}");
        }
        assert_eq!(block_time_secs("2m").unwrap(), 120.0);
    }
}
