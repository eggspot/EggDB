use eggdb_crypto::{AEAD_NONCE_BYTES, AEAD_TAG_BYTES, AesGcm};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

const KEYRING_FILE: &str = "tenant-keys.eggkeys";
const MAGIC: &[u8; 8] = b"EGGTKEY1";
const TENANT_KEY_BYTES: usize = 32;
const MOST_NAME_BYTES: usize = u16::MAX as usize;

/// Each tenant database's own key (D1366), kept in one file beside the tenant files, sealed
/// with AES-256-GCM under the server's key: dropping a tenant forgets its key, so its file and
/// every copy of it can no longer be read (crypto-shredding).
pub(super) struct TenantKeyring {
    path: PathBuf,
    sealing: AesGcm,
    keys: Mutex<BTreeMap<String, [u8; TENANT_KEY_BYTES]>>,
}

impl TenantKeyring {
    /// The keyring under `root`, sealed with `server_key`, read when it exists.
    pub(super) fn open(root: &Path, server_key: &[u8; 32]) -> Result<Self, String> {
        let path = root.join(KEYRING_FILE);
        let sealing = AesGcm::new(server_key).map_err(|_| "the server key is not 32 bytes".to_owned())?;
        let keys = match std::fs::read(&path) {
            Ok(bytes) => unseal(&sealing, &bytes).ok_or_else(|| {
                format!("{} cannot be read with the server's key: a wrong key, or the file was changed", path.display())
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        Ok(Self { path, sealing, keys: Mutex::new(keys) })
    }

    /// Tenant `name`'s key, when it has one.
    pub(super) fn key_of(&self, name: &str) -> Option<[u8; TENANT_KEY_BYTES]> {
        self.keys.lock().unwrap_or_else(PoisonError::into_inner).get(name).copied()
    }

    /// Makes tenant `name` a new random key, kept before it is returned.
    pub(super) fn add(&self, name: &str) -> Result<[u8; TENANT_KEY_BYTES], String> {
        if name.len() > MOST_NAME_BYTES {
            return Err("the tenant name is too long for the keyring".to_owned());
        }
        let mut key = [0u8; TENANT_KEY_BYTES];
        eggdb_platform::secure_random::fill(&mut key).map_err(|error| format!("cannot make a tenant key: {error}"))?;
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = keys.clone();
        next.insert(name.to_owned(), key);
        self.save(&next)?;
        *keys = next;
        Ok(key)
    }

    /// Forgets tenant `name`'s key for good; whether it had one.
    pub(super) fn shred(&self, name: &str) -> Result<bool, String> {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        if !keys.contains_key(name) {
            return Ok(false);
        }
        let mut next = keys.clone();
        next.remove(name);
        self.save(&next)?;
        *keys = next;
        Ok(true)
    }

    fn save(&self, keys: &BTreeMap<String, [u8; TENANT_KEY_BYTES]>) -> Result<(), String> {
        let mut nonce = [0u8; AEAD_NONCE_BYTES];
        eggdb_platform::secure_random::fill(&mut nonce).map_err(|error| format!("cannot seal the tenant keys: {error}"))?;
        let mut body = Vec::new();
        for (name, key) in keys {
            body.extend_from_slice(&(name.len() as u16).to_be_bytes());
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(key);
        }
        let tag = self.sealing.seal_in_place(&nonce, MAGIC, &mut body);
        let mut sealed = MAGIC.to_vec();
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&tag);
        sealed.extend_from_slice(&body);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let staging = self.path.with_extension("eggkeys-writing");
        let written = std::fs::File::create(&staging).and_then(|mut file| {
            std::io::Write::write_all(&mut file, &sealed)?;
            file.sync_all()
        });
        written.map_err(|error| format!("cannot write {}: {error}", staging.display()))?;
        std::fs::rename(&staging, &self.path).map_err(|error| format!("cannot replace {}: {error}", self.path.display()))
    }
}

fn unseal(sealing: &AesGcm, bytes: &[u8]) -> Option<BTreeMap<String, [u8; TENANT_KEY_BYTES]>> {
    let rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let nonce: [u8; AEAD_NONCE_BYTES] = rest.get(..AEAD_NONCE_BYTES)?.try_into().ok()?;
    let rest = &rest[AEAD_NONCE_BYTES..];
    let tag: [u8; AEAD_TAG_BYTES] = rest.get(..AEAD_TAG_BYTES)?.try_into().ok()?;
    let mut body = rest[AEAD_TAG_BYTES..].to_vec();
    sealing.open_in_place(&nonce, MAGIC, &mut body, &tag).ok()?;
    let mut keys = BTreeMap::new();
    let mut at = 0;
    while at < body.len() {
        let length = usize::from(u16::from_be_bytes(body.get(at..at + 2)?.try_into().ok()?));
        at += 2;
        let name = std::str::from_utf8(body.get(at..at + length)?).ok()?.to_owned();
        at += length;
        let key: [u8; TENANT_KEY_BYTES] = body.get(at..at + TENANT_KEY_BYTES)?.try_into().ok()?;
        at += TENANT_KEY_BYTES;
        keys.insert(name, key);
    }
    Some(keys)
}

#[cfg(test)]
mod tests {
    use super::TenantKeyring;

    #[test]
    fn keys_are_kept_sealed_read_back_and_shredded() {
        let folder = std::env::temp_dir().join(format!("eggdb-tenant-keys-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let ring = TenantKeyring::open(&folder, &[7; 32]).unwrap();
        let acme = ring.add("acme").unwrap();
        let globex = ring.add("globex").unwrap();
        assert_ne!(acme, globex);
        let raw = std::fs::read(folder.join("tenant-keys.eggkeys")).unwrap();
        assert!(!raw.windows(4).any(|window| window == b"acme"));
        let again = TenantKeyring::open(&folder, &[7; 32]).unwrap();
        assert_eq!(again.key_of("acme"), Some(acme));
        assert!(TenantKeyring::open(&folder, &[8; 32]).is_err());
        assert!(again.shred("acme").unwrap());
        assert!(!again.shred("acme").unwrap());
        let after = TenantKeyring::open(&folder, &[7; 32]).unwrap();
        assert_eq!(after.key_of("acme"), None);
        assert_eq!(after.key_of("globex"), Some(globex));
        std::fs::remove_file(folder.join("tenant-keys.eggkeys")).unwrap();
        std::fs::remove_dir(&folder).unwrap();
    }
}
