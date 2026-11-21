//! A LEZ wallet's `storage.json`, as sessions and account groups keep it: what changes when
//! the chain it synced is replaced by a new one.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::value::RawValue;

/// The file a LEZ wallet keeps its keys and state in, inside its home directory.
pub const STORAGE_FILE: &str = "storage.json";

/// Marks wallets as on a new chain until they've forgotten the old one: in their owner's
/// directory, so the forgetting is retried on a later `up` when it fails.
const NEW_CHAIN_MARK: &str = "new-chain";

/// Marks the wallet kept under `owner` (a session's or a group's directory) as on a new chain.
pub fn mark_new_chain(owner: &Path) -> Result<()> {
    std::fs::create_dir_all(owner)?;
    let mark = owner.join(NEW_CHAIN_MARK);
    std::fs::write(&mark, "").with_context(|| format!("writing {}", mark.display()))
}

/// Whether `owner`'s wallet still has the old chain to forget.
pub fn marked_new_chain(owner: &Path) -> bool {
    owner.join(NEW_CHAIN_MARK).exists()
}

/// Clears `mark_new_chain`'s mark, once the wallet has forgotten the old chain.
pub fn clear_new_chain(owner: &Path) -> Result<()> {
    let mark = owner.join(NEW_CHAIN_MARK);
    std::fs::remove_file(&mark).with_context(|| format!("deleting {}", mark.display()))
}

/// A JSON value kept as written. The wallet stores `u128` nonces, which `Value` would turn into
/// floats the wallet can't read, so `storage.json` is only ever changed where it must be.
type Raw = Box<RawValue>;
type RawObject = BTreeMap<String, Raw>;

/// The LEZ wallet's state of a fresh account, what every account holds on a new chain, in the
/// shape of `was`: v0.4 keeps program data under `actor_states` and refuses v0.3's `shards`.
fn fresh_account(was: &Raw) -> Result<Raw> {
    let data: Option<RawObject> = serde_json::from_str::<RawObject>(was.get())?
        .get("data")
        .map(|data| serde_json::from_str(data.get()))
        .transpose()?;
    let key = match data {
        Some(data) if data.contains_key("actor_states") => "actor_states",
        _ => "shards",
    };
    Ok(RawValue::from_string(format!(
        r#"{{"nonce":0,"data":{{"{key}":{{}}}}}}"#
    ))?)
}

/// `raw`, an object, with `change` applied to its members.
fn edit_object(raw: &mut Raw, change: impl FnOnce(&mut RawObject) -> Result<()>) -> Result<()> {
    let mut object: RawObject = serde_json::from_str(raw.get())?;
    change(&mut object)?;
    *raw = serde_json::value::to_raw_value(&object)?;
    Ok(())
}

/// `change` applied to `object`'s member `key`, when it has one.
fn edit_member(
    object: &mut RawObject,
    key: &str,
    change: impl FnOnce(&mut Raw) -> Result<()>,
) -> Result<()> {
    object.get_mut(key).map_or(Ok(()), change)
}

/// A LEZ wallet's `storage.json` (`text`) without what it learned from a chain: how far it
/// synced, and the state of its private and shared private accounts. Keys, labels and
/// everything else stay as written, so the wallet's accounts are the same on the next chain.
/// Accounts imported from genesis data keep their state. A layout it doesn't know is an error,
/// not a guess.
fn forget_chain(text: &str) -> Result<String> {
    let mut storage: RawObject = serde_json::from_str(text)?;
    if storage.contains_key("ciphertext") {
        anyhow::bail!(
            "the wallet's storage is encrypted with a password, so its state from the old chain \
             can't be cleared: make the wallet again without one"
        );
    }
    storage.insert(
        "last_synced_block".into(),
        RawValue::from_string("0".into())?,
    );
    // v0.4 also records the block its sync started from.
    if storage.contains_key("synced_from_block") {
        storage.insert(
            "synced_from_block".into(),
            RawValue::from_string("0".into())?,
        );
    }
    edit_member(&mut storage, "key_chain", |key_chain| {
        edit_object(key_chain, |key_chain| {
            edit_member(key_chain, "accounts", |accounts| {
                let mut accounts_list: Vec<RawObject> = serde_json::from_str(accounts.get())?;
                for account in &mut accounts_list {
                    edit_member(account, "Private", |private| {
                        edit_object(private, |private| {
                            edit_member(private, "data", |data| {
                                edit_object(data, |data| {
                                    edit_member(data, "value", forget_private_states)
                                })
                            })
                        })
                    })?;
                }
                *accounts = serde_json::value::to_raw_value(&accounts_list)?;
                Ok(())
            })?;
            edit_member(key_chain, "shared_private_accounts", |shared| {
                let mut entries: RawObject = serde_json::from_str(shared.get())?;
                for entry in entries.values_mut() {
                    edit_object(entry, |entry| {
                        if let Some(account) = entry.get("account") {
                            let fresh = fresh_account(account)?;
                            entry.insert("account".into(), fresh);
                        }
                        Ok(())
                    })?;
                }
                *shared = serde_json::value::to_raw_value(&entries)?;
                Ok(())
            })
        })
    })?;
    Ok(serde_json::to_string_pretty(&storage)? + "\n")
}

/// A private account's `value`: its keys, then its state per kind of private account, each
/// set back to a fresh account.
fn forget_private_states(value: &mut Raw) -> Result<()> {
    let (keys, states): (Raw, Vec<(Raw, Raw)>) = serde_json::from_str(value.get())?;
    let states = states
        .into_iter()
        .map(|(kind, account)| Ok((kind, fresh_account(&account)?)))
        .collect::<Result<Vec<_>>>()?;
    *value = serde_json::value::to_raw_value(&(keys, states))?;
    Ok(())
}

/// `forget_chain` on the file, when the wallet has written one. The new file replaces the old
/// one whole: it holds the wallet's keys.
pub fn forget_chain_in(storage: &Path) -> Result<()> {
    let text = match std::fs::read_to_string(storage) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", storage.display())),
    };
    let forgotten = forget_chain(&text).with_context(|| {
        format!(
            "{}: not a LEZ wallet storage this version knows",
            storage.display()
        )
    })?;
    replace_file(storage, forgotten.as_bytes())
        .with_context(|| format!("replacing {}", storage.display()))?;
    // v0.4 caches the nullifiers it saw spent next to the storage: the old chain's.
    let nullifiers = storage.with_extension("nullifiers");
    match std::fs::remove_file(&nullifiers) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("deleting {}", nullifiers.display()))
        }
        _ => Ok(()),
    }
}

/// The public key, in hex, of the public account `id` in the wallet whose storage is at
/// `storage`: as written for an account the wallet made, or derived from an imported one's
/// private key. Only a key that names `id` counts. `None` when the wallet holds no such account.
pub fn public_key(storage: &Path, id: &crate::account::PublicId) -> Result<Option<String>> {
    use crate::account::{PublicId, public_key_of};
    let text = match std::fs::read_to_string(storage) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", storage.display())),
    };
    let wallet: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a LEZ wallet storage", storage.display()))?;
    let accounts = wallet["key_chain"]["accounts"]
        .as_array()
        .into_iter()
        .flatten();
    for account in accounts {
        let key = if account["Public"]["account_id"] == id.as_str() {
            account["Public"]["data"]["pk"].as_str().map(str::to_owned)
        } else if account["ImportedPublic"]["account_id"] == id.as_str() {
            account["ImportedPublic"]["pub_sign_key"]
                .as_str()
                .map(public_key_of)
                .transpose()?
        } else {
            continue;
        };
        return Ok(key.filter(|key| PublicId::of_key(key).is_ok_and(|of| &of == id)));
    }
    Ok(None)
}

/// Replaces `path` with `contents` in one step, keeping its permissions: it never holds half of
/// either, and the new file is on disk before it takes the old one's place.
fn replace_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
    let next = path.with_extension("json.new");
    let written = (|| {
        // One left by an earlier failure would keep its own permissions.
        match std::fs::remove_file(&next) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&next)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&next, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&next);
    }
    written
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;

    /// A session's `storage.json` after a chain ran, as the LEZ wallet writes it: synced to
    /// block 42, a private account and a shared one holding notes, and an account imported from
    /// genesis data whose nonce only fits a u128.
    pub(crate) const SYNCED_STORAGE: &str = r#"{
  "key_chain": {
    "accounts": [
      { "Public": { "account_id": "P", "chain_index": "/0", "data": { "k": [1, 2] } } },
      { "Private": { "account_id": "Q", "chain_index": "/0", "data": {
        "value": [ { "keys": 2 }, [
          [ { "Regular": "111" }, { "nonce": 7, "data": { "shards": { "T": [1, 2] } } } ],
          [ { "Regular": "222" }, { "nonce": 3, "data": { "shards": {} } } ]
        ] ],
        "ccc": [0], "cci": 0 } } },
      { "ImportedPrivate": { "account": { "nonce": 340282366920938463463374607431768211455 } } }
    ],
    "group_key_holders": { "g": { "k": 3 } },
    "shared_private_accounts": {
      "S": { "group_label": "g", "identifier": "1", "pda_seed": null,
             "authority_program_id": null,
             "account": { "nonce": 5, "data": { "shards": { "T": [9] } } } }
    }
  },
  "last_synced_block": 42,
  "labels": { "main": { "Public": "P" } }
}"#;

    #[test]
    fn forgetting_a_chain_keeps_keys_and_drops_its_state() {
        let text = forget_chain(SYNCED_STORAGE).unwrap();
        // Numbers past u64 come back as written, not as floats.
        assert!(
            text.contains("340282366920938463463374607431768211455"),
            "{text}"
        );
        let storage: Value = serde_json::from_str(&text).unwrap();
        let fresh = serde_json::json!({ "nonce": 0, "data": { "shards": {} } });
        assert_eq!(storage["last_synced_block"], 0);
        let private = &storage["key_chain"]["accounts"][1]["Private"]["data"]["value"];
        assert_eq!(private[0], serde_json::json!({ "keys": 2 }), "keys stay");
        assert_eq!(private[1][0][0], serde_json::json!({ "Regular": "111" }));
        assert_eq!(private[1][0][1], fresh);
        assert_eq!(private[1][1][1], fresh);
        let shared = &storage["key_chain"]["shared_private_accounts"]["S"];
        assert_eq!(shared["account"], fresh);
        assert_eq!(shared["group_label"], "g");
        // Everything else is as it was.
        let mut expected: Value = serde_json::from_str(SYNCED_STORAGE).unwrap();
        expected["last_synced_block"] = 0.into();
        expected["key_chain"]["accounts"][1] = storage["key_chain"]["accounts"][1].clone();
        expected["key_chain"]["shared_private_accounts"]["S"]["account"] = fresh;
        assert_eq!(storage, expected);
    }

    /// A v0.4 wallet's `storage.json` after a chain ran, in the layout LEZ v0.4.0-rc3's wallet
    /// writes: the private state keeps program data under `actor_states`, and the wallet
    /// records where its sync started.
    const SYNCED_STORAGE_V04: &str = r#"{
  "key_chain": {
    "accounts": [
      { "Private": { "account_id": "Q", "chain_index": "/0", "data": {
        "value": [ { "keys": 2 }, [
          [ "Regular", { "nonce": 7, "data": { "actor_states": { "T": [1, 2] } } } ]
        ] ] } } }
    ],
    "shared_private_accounts": {}
  },
  "last_synced_block": 42,
  "synced_from_block": 40
}"#;

    #[test]
    fn forgets_a_v0_4_chain_in_its_own_shape() {
        let storage: Value =
            serde_json::from_str(&forget_chain(SYNCED_STORAGE_V04).unwrap()).unwrap();
        assert_eq!(storage["last_synced_block"], 0);
        assert_eq!(storage["synced_from_block"], 0);
        let state = &storage["key_chain"]["accounts"][0]["Private"]["data"]["value"][1][0];
        // v0.4 refuses `shards`: a fresh account has to be in the shape it read.
        assert_eq!(
            state,
            &serde_json::json!(["Regular", { "nonce": 0, "data": { "actor_states": {} } }])
        );
    }

    #[test]
    fn drops_the_spent_nullifiers_a_v0_4_wallet_cached() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join(STORAGE_FILE);
        std::fs::write(&storage, SYNCED_STORAGE_V04).unwrap();
        let nullifiers = dir.path().join("storage.nullifiers");
        std::fs::write(&nullifiers, "old chain").unwrap();
        forget_chain_in(&storage).unwrap();
        assert!(!nullifiers.exists());
    }

    #[test]
    fn reads_an_accounts_public_key_from_the_wallet() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join(STORAGE_FILE);
        // As LEZ's wallet writes them: a generated account keeps its key as `pk`, an imported
        // one its private key as `pub_sign_key`.
        let made = "4afc5da3b6acabc4892dddb1e49226599326ae3a6a700828e395b5bc9a4b30ba";
        let text = format!(
            r#"{{"key_chain":{{"accounts":[
              {{"Public":{{"account_id":"23cbN9uZbLU1zhcuiZ1qu1eneL8ed8HeHzDEF1br6rEd","chain_index":[0],"data":{{"sk":"x","pk":"{made}"}}}}}},
              {{"ImportedPublic":{{"account_id":"B1UN3hPgxacgHKBRoThcAmsPajGcUf6YXUhgB36x4DAd","pub_sign_key":"{}"}}}}
            ]}},"last_synced_block":1}}"#,
            "01".repeat(32)
        );
        std::fs::write(&storage, text).unwrap();
        let key = |id: &str| public_key(&storage, &crate::account::PublicId::parse(id).unwrap());
        assert_eq!(
            key("23cbN9uZbLU1zhcuiZ1qu1eneL8ed8HeHzDEF1br6rEd")
                .unwrap()
                .as_deref(),
            Some(made)
        );
        let imported = key("B1UN3hPgxacgHKBRoThcAmsPajGcUf6YXUhgB36x4DAd").unwrap();
        assert_eq!(
            imported,
            Some(crate::account::public_key_of(&"01".repeat(32)).unwrap())
        );
        assert_eq!(
            key("HpavNGFYxjyhLsy171aXsvv146yUeXmRWiho22uvT43d").unwrap(),
            None
        );
    }

    #[test]
    fn refuses_an_encrypted_storage() {
        // A wallet with a password keeps its state inside the ciphertext, out of reach.
        let sealed = r#"{"version":1,"kdf":{},"nonce":"aa","ciphertext":"bb"}"#;
        let err = forget_chain(sealed).unwrap_err();
        assert!(format!("{err:#}").contains("password"), "{err:#}");
    }

    #[test]
    fn forgetting_a_chain_adds_nothing_and_refuses_a_layout_it_doesnt_know() {
        let bare = r#"{"key_chain":{"accounts":[]},"last_synced_block":9}"#;
        let storage: Value = serde_json::from_str(&forget_chain(bare).unwrap()).unwrap();
        assert_eq!(
            storage,
            serde_json::json!({ "key_chain": { "accounts": [] }, "last_synced_block": 0 })
        );
        let odd = r#"{"key_chain":{"accounts":[{"Private":{"data":{"value":[1,2,3]}}}]}}"#;
        assert!(forget_chain(odd).is_err());
    }
}
