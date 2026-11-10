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
}

/// Returns `debug` with `opts` applied. The faucet is the only funded account: upstream's
/// supply accounts have published keys.
pub fn patch(mut debug: Value, opts: &ChainOptions) -> Result<Value> {
    let config = debug
        .as_object_mut()
        .context("the upstream sequencer config is not a JSON object")?;
    config.insert("home".into(), json!(opts.home));
    config.insert("block_create_timeout".into(), json!(opts.block_time));
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

    fn patched(block_time: &str) -> Result<Value> {
        let faucet = PublicId::parse(FAUCET).unwrap();
        patch(
            upstream(),
            &ChainOptions {
                home: "/devnet/chain",
                block_time,
                faucet: &faucet,
                faucet_balance: 1_000,
            },
        )
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
    fn funds_only_the_faucet() {
        let out = patched("1s").unwrap();
        let genesis = out["genesis"].as_array().unwrap();
        assert_eq!(
            genesis,
            &vec![
                json!({ "stake_sequencer": { "sequencer_key": "k" } }),
                json!({ "supply_account": { "account_id": FAUCET, "balance": 1000 } }),
            ]
        );
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
