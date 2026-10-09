//! The key vault in Windows Credential Manager: one generic entry per key,
//! `UsageMonitor/Vault/<id>`, holding the whole key as JSON. Like the
//! accounts' own keys, they are readable only by this Windows user.

use usage_monitor_core::vault::{self, VaultKey};

use super::{credential_names, load_named, remove_named, save_named};

const PREFIX: &str = "UsageMonitor/Vault/";
/// A key at its longest, with its labels and JSON quoting.
const MAX_ENTRY_BYTES: usize = 3 * vault::MAX_SECRET_BYTES;

fn entry_name(id: &str) -> Result<String, String> {
    // Ids are made by `VaultKey::new`; anything else is refused, so an entry
    // outside the vault can never be named.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("The key's id is invalid.".to_owned());
    }
    Ok(format!("{PREFIX}{id}"))
}

/// Every key in the vault, in list order. A damaged entry is skipped rather
/// than hiding the others.
pub fn list() -> Result<Vec<VaultKey>, String> {
    let mut keys = entry_ids()?
        .into_iter()
        .filter_map(|id| get(&id).ok().flatten())
        .collect::<Vec<_>>();
    vault::sort(&mut keys);
    Ok(keys)
}

pub fn get(id: &str) -> Result<Option<VaultKey>, String> {
    let Some(blob) = load_named(&entry_name(id)?, MAX_ENTRY_BYTES).map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let key: VaultKey = serde_json::from_slice(&blob)
        .map_err(|error| format!("The saved key is damaged: {error}"))?;
    Ok((key.id == id).then_some(key))
}

/// Adds `key`, or replaces the key with its id.
pub fn save(key: &VaultKey) -> Result<(), String> {
    let blob = serde_json::to_vec(key).map_err(|error| error.to_string())?;
    save_named(&entry_name(&key.id)?, &blob, MAX_ENTRY_BYTES).map_err(|e| e.to_string())
}

pub fn remove(id: &str) -> Result<(), String> {
    remove_named(&entry_name(id)?).map_err(|e| e.to_string())
}

/// The ids of the vault's entries, leaving out the parts of split ones.
fn entry_ids() -> Result<Vec<String>, String> {
    Ok(credential_names(PREFIX)
        .map_err(|error| error.to_string())?
        .iter()
        .filter_map(|name| name.strip_prefix(PREFIX))
        .filter(|id| !id.contains('#'))
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_vault_ids_name_an_entry() {
        assert_eq!(entry_name("abc123").unwrap(), "UsageMonitor/Vault/abc123");
        assert!(entry_name("").is_err());
        assert!(entry_name("../OAuth/1").is_err());
        assert!(entry_name("a#part0").is_err());
    }

    /// Writes to the real Credential Manager, so it runs only when asked:
    /// `cargo test -p usage-monitor-windows vault -- --ignored`.
    #[test]
    #[ignore]
    fn keys_round_trip_through_credential_manager() {
        let mut key = VaultKey::new("Test key", "Test", &"k".repeat(4000)).unwrap();
        save(&key).unwrap();
        assert_eq!(get(&key.id).unwrap().as_ref(), Some(&key));
        assert!(list().unwrap().iter().any(|listed| listed.id == key.id));
        key.edit("Renamed", "Test", "short").unwrap();
        save(&key).unwrap();
        assert_eq!(get(&key.id).unwrap().unwrap().secret, "short");
        remove(&key.id).unwrap();
        assert_eq!(get(&key.id).unwrap(), None);
    }
}
