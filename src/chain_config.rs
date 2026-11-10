//! The sequencer config a devnet runs on: upstream's own debug config for the pinned LEZ tag,
//! with our chain home, block interval and faucet. Patching upstream's file rather than shipping
//! our own keeps every field a newer tag adds.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::account::PublicId;

/// What a devnet changes in upstream's debug config.
pub struct ChainOptions<'a> {
    /// Where the sequencer keeps its database and keys, as the sequencer sees the path.
    pub home: &'a str,
    /// Block interval, in the sequencer's humantime format (`1s`, `500ms`).
    pub block_time: &'a str,
    /// The faucet, funded at genesis.
    pub faucet: &'a PublicId,
    /// The faucet's genesis balance in lepta.
    pub faucet_balance: u64,
    /// The most user transactions a block takes.
    pub max_txs_per_block: u32,
    /// More accounts to fund at genesis, with their balances.
    pub genesis_accounts: &'a [(PublicId, u64)],
}

/// The bounds `max_txs_per_block` must fall in: upstream's mempool holds 1000 transactions.
pub const MAX_TXS_PER_BLOCK: std::ops::RangeInclusive<u32> = 1..=1000;

/// Returns `debug` with `opts` applied. The faucet is the only funded account: upstream's
/// supply accounts have published keys.
pub fn patch(mut debug: Value, opts: &ChainOptions) -> Result<Value> {
    let config = debug
        .as_object_mut()
        .context("the upstream sequencer config is not a JSON object")?;
    config.insert("home".into(), json!(opts.home));
    config.insert("block_create_timeout".into(), json!(opts.block_time));
    if !MAX_TXS_PER_BLOCK.contains(&opts.max_txs_per_block) {
        bail!(
            "max_txs_per_block must be {} to {}",
            MAX_TXS_PER_BLOCK.start(),
            MAX_TXS_PER_BLOCK.end()
        );
    }
    config.insert("max_num_tx_in_block".into(), json!(opts.max_txs_per_block));
    // Both would listen on every interface: metrics on 9000 and gossip on a UDP port. A single
    // devnet sequencer needs neither.
    config.insert("metrics_address".into(), Value::Null);
    config.insert("gossip".into(), Value::Null);

    let genesis = config
        .entry("genesis")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("the upstream config's `genesis` is not a list")?;
    genesis.retain(|action| action.get("supply_account").is_none());
    genesis.push(json!({
        "supply_account": { "account_id": opts.faucet.as_str(), "balance": opts.faucet_balance }
    }));
    for (account, balance) in opts.genesis_accounts {
        genesis.push(json!({
            "supply_account": { "account_id": account.as_str(), "balance": balance }
        }));
    }

    let block_secs = block_time_secs(opts.block_time)?;
    if let Some(timeout) = config
        .get("bedrock_config")
        .and_then(|b| b["channel_params"]["posting_timeout"].as_u64())
        && block_secs >= timeout as f64
    {
        bail!(
            "block time {} must be under the channel's posting_timeout of {timeout}s",
            opts.block_time
        );
    }
    Ok(debug)
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
        let faucet = PublicId::parse(FAUCET).unwrap();
        let users = [(PublicId::parse(USER).unwrap(), 7)];
        patch(
            upstream(),
            &ChainOptions {
                home: "/devnet/chain",
                block_time,
                faucet: &faucet,
                faucet_balance: 1_000,
                max_txs_per_block,
                genesis_accounts: &users,
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

    #[test]
    fn refuses_malformed_block_times() {
        for bad in ["1", "s", "1h", "0s", ""] {
            assert!(block_time_secs(bad).is_err(), "{bad:?}");
        }
        assert_eq!(block_time_secs("2m").unwrap(), 120.0);
    }
}
