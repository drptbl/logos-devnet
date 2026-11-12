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
        Ok(in_use(&self.account(account)?))
    }

    /// The block that holds the transaction whose hash is `hash` (64 hex digits), or `None`
    /// while none does: not yet, or never.
    pub fn transaction_block(&self, hash: &str) -> Result<Option<u64>> {
        let value = self.call("getTransaction", json!([hash]))?;
        if value.is_null() {
            return Ok(None);
        }
        value
            .get(1)
            .and_then(Value::as_u64)
            .map(Some)
            .with_context(|| format!("getTransaction returned {value}"))
    }

    /// An account's state: its `nonce` and its `data` shards, by program id.
    pub fn account(&self, account: &PublicId) -> Result<Value> {
        self.call("getAccount", json!([account.as_str()]))
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

pub(crate) fn in_use(account: &Value) -> bool {
    let nonce = account["nonce"].as_u64().unwrap_or(0);
    let has_data = account["data"]["shards"]
        .as_object()
        .is_some_and(|shards| !shards.is_empty());
    nonce > 0 || has_data
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An Rpc against a one-request server that answers `body`.
    fn answering(body: &'static str) -> Rpc {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body.as_bytes()).unwrap();
        });
        Rpc::new(url)
    }

    #[test]
    fn reads_a_transactions_block() {
        let in_block = answering(r#"{"jsonrpc":"2.0","id":1,"result":["EgAAAA",83]}"#);
        assert_eq!(
            in_block.transaction_block(&"ab".repeat(32)).unwrap(),
            Some(83)
        );
        let pending = answering(r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
        assert_eq!(pending.transaction_block(&"ab".repeat(32)).unwrap(), None);
    }

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
