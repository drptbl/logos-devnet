//! Public account ids as the LEZ wallet prints and takes them: 32 bytes in base58, optionally
//! prefixed `Public/`.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A public account id, validated as 32 bytes of base58. Stored as bare base58.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublicId(String);

impl TryFrom<String> for PublicId {
    type Error = anyhow::Error;
    fn try_from(text: String) -> Result<Self> {
        Self::parse(&text)
    }
}

impl From<PublicId> for String {
    fn from(id: PublicId) -> Self {
        id.0
    }
}

impl PublicId {
    /// Accepts `Public/<base58>` or bare `<base58>`. Refuses private ids: the faucet pays in
    /// public, and a public payment to a private id would be lost.
    pub fn parse(text: &str) -> Result<Self> {
        if text.starts_with("Private/") {
            bail!(
                "{text} is a private account; fund a public one and move it privately from there"
            );
        }
        let id = text.strip_prefix("Public/").unwrap_or(text);
        match bs58::decode(id).into_vec() {
            Ok(bytes) if bytes.len() == 32 => Ok(Self(id.to_owned())),
            _ => bail!("{text} is not an account id (32 bytes of base58)"),
        }
    }

    /// The bare base58 id.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `Public/<base58>`, the form wallet commands take.
    pub fn wallet_form(&self) -> String {
        format!("Public/{}", self.0)
    }

    /// The id as 64 hex characters, the form `lez_core` takes (`program_id_hex`).
    pub fn hex(&self) -> String {
        let bytes = bs58::decode(&self.0)
            .into_vec()
            .expect("validated in parse");
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The first `Public/<id>` in wallet output, e.g. from
/// `Generated new account with account_id Public/<id> at path /0`.
pub fn first_public_id(output: &str) -> Option<PublicId> {
    output
        .split(|c: char| c.is_whitespace())
        .filter_map(|word| word.strip_prefix("Public/"))
        .find_map(|id| PublicId::parse(id).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real account id from the LEZ testnet, in base58 and in hex.
    const SAMPLE_ID: &str = "HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d";
    const SAMPLE_HEX: &str = "f9eba477278c918fadafbcb4a6463e947d0fd8de344b5431ee0aa3cd41d7e3a4";

    #[test]
    fn parses_bare_and_prefixed_ids() {
        assert_eq!(PublicId::parse(SAMPLE_ID).unwrap().as_str(), SAMPLE_ID);
        let prefixed = PublicId::parse(&format!("Public/{SAMPLE_ID}")).unwrap();
        assert_eq!(prefixed.as_str(), SAMPLE_ID);
        assert_eq!(prefixed.wallet_form(), format!("Public/{SAMPLE_ID}"));
    }

    #[test]
    fn converts_to_hex() {
        assert_eq!(PublicId::parse(SAMPLE_ID).unwrap().hex(), SAMPLE_HEX);
    }

    #[test]
    fn refuses_private_and_malformed_ids() {
        assert!(PublicId::parse(&format!("Private/{SAMPLE_ID}")).is_err());
        assert!(PublicId::parse("not-base58-0OIl").is_err());
        assert!(PublicId::parse("abc").is_err(), "too short");
    }

    #[test]
    fn finds_the_new_account_in_wallet_output() {
        let output = format!(
            "Stored persistent accounts at /w/storage.json\n\
             Generated new account with account_id Public/{SAMPLE_ID} at path /0\n\
             With pk deb1830a"
        );
        assert_eq!(first_public_id(&output).unwrap().as_str(), SAMPLE_ID);
        assert!(first_public_id("no account here").is_none());
    }
}
