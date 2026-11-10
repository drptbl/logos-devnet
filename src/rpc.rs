//! The few sequencer JSON-RPC calls the CLI needs.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::account::PublicId;

pub struct Rpc {
    url: String,
    agent: ureq::Agent,
}

impl Rpc {
    pub fn new(url: String) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Self { url, agent }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let reply: Value = self
            .agent
            .post(&self.url)
            .send_json(&body)
            .with_context(|| format!("no answer from the sequencer at {}", self.url))?
            .body_mut()
            .read_json()
            .context("the sequencer's answer is not JSON")?;
        result_of(reply, method)
    }

    pub fn last_block_id(&self) -> Result<u64> {
        let value = self.call("getLastBlockId", json!([]))?;
        value
            .as_u64()
            .context("getLastBlockId did not return a number")
    }

    pub fn balance(&self, account: &PublicId) -> Result<u128> {
        let value = self.call("getAccountBalance", json!([account.as_str()]))?;
        // Balances are u128 on chain; serde_json reads small ones as u64.
        value
            .as_u64()
            .map(u128::from)
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
            .with_context(|| format!("getAccountBalance returned {value}"))
    }

    /// Whether anything was written to the account: a deployed program header has a nonce and
    /// data, a fresh account has neither.
    pub fn account_in_use(&self, account: &PublicId) -> Result<bool> {
        let value = self.call("getAccount", json!([account.as_str()]))?;
        Ok(in_use(&value))
    }
}

fn result_of(reply: Value, method: &str) -> Result<Value> {
    if let Some(error) = reply.get("error") {
        bail!("{method} failed: {error}");
    }
    reply
        .get("result")
        .cloned()
        .with_context(|| format!("{method} returned no result: {reply}"))
}

fn in_use(account: &Value) -> bool {
    let nonce = account["nonce"].as_u64().unwrap_or(0);
    let has_data = account["data"]["shards"]
        .as_object()
        .is_some_and(|shards| !shards.is_empty());
    nonce > 0 || has_data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_results_and_errors() {
        assert_eq!(result_of(json!({ "result": 7 }), "m").unwrap(), json!(7));
        assert!(result_of(json!({ "error": { "message": "no" } }), "m").is_err());
        assert!(result_of(json!({}), "m").is_err());
    }

    #[test]
    fn tells_a_deployed_header_from_a_fresh_account() {
        // As the sequencer answered getAccount for a header and an unused account.
        let header = json!({ "nonce": 1, "data": { "shards": { "JAQ5": [29, 100] } } });
        let fresh = json!({ "nonce": 0, "data": { "shards": {} } });
        assert!(in_use(&header));
        assert!(!in_use(&fresh));
    }
}
