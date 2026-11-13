use eggdb_crypto::{AEAD_NONCE_BYTES, AEAD_TAG_BYTES, AesGcm, Sha256, hkdf_expand, hkdf_extract};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"EGGENC01";
const SALT_BYTES: usize = 16;
const PREFIX_BYTES: usize = 7;
const CHUNK_BYTES: usize = 1 << 20;
const KEY_INFO: &[u8] = b"eggdb backup archive v1";

fn cipher(key: &[u8; 32], salt: &[u8]) -> Result<AesGcm, String> {
    let prk = hkdf_extract::<Sha256>(salt, key);
    let mut derived = [0u8; 32];
    hkdf_expand::<Sha256>(prk.as_ref(), KEY_INFO, &mut derived).map_err(|_| "cannot derive the archive key".to_owned())?;
    AesGcm::new(&derived).map_err(|_| "cannot use the archive key".to_owned())
}

fn nonce(prefix: &[u8; PREFIX_BYTES], counter: u32, last: bool) -> [u8; AEAD_NONCE_BYTES] {
    let mut nonce = [0u8; AEAD_NONCE_BYTES];
    nonce[..PREFIX_BYTES].copy_from_slice(prefix);
    nonce[PREFIX_BYTES..PREFIX_BYTES + 4].copy_from_slice(&counter.to_be_bytes());
    nonce[AEAD_NONCE_BYTES - 1] = u8::from(last);
    nonce
}

fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// Encrypts the file at `source` into the new file `target` (D1020): a header with a random
/// salt and nonce prefix, then 1 MiB chunks sealed with AES-256-GCM under a key derived from
/// `key` with HKDF-SHA256. Each chunk's nonce carries its number and whether it is the last,
/// so chunks can be neither reordered, dropped nor cut off at the end without failing.
pub(super) fn seal_file(source: &Path, target: &Path, key: &[u8; 32]) -> Result<(), String> {
    let mut input = File::open(source).map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
    let mut header = [0u8; SALT_BYTES + PREFIX_BYTES];
    eggdb_platform::secure_random::fill(&mut header).map_err(|error| format!("cannot draw a random salt: {error}"))?;
    let (salt, prefix) = header.split_at(SALT_BYTES);
    let prefix: [u8; PREFIX_BYTES] = prefix.try_into().map_err(|_| "the nonce prefix has the wrong length".to_owned())?;
    let cipher = cipher(key, salt)?;
    let write_failed = |error: std::io::Error| format!("cannot write {}: {error}", target.display());
    output.write_all(MAGIC).and_then(|()| output.write_all(&header)).map_err(write_failed)?;
    let mut current = vec![0u8; CHUNK_BYTES];
    let mut next = vec![0u8; CHUNK_BYTES];
    let read_failed = |error: std::io::Error| format!("cannot read {}: {error}", source.display());
    let mut current_length = fill(&mut input, &mut current).map_err(read_failed)?;
    let mut counter = 0u32;
    loop {
        let next_length = fill(&mut input, &mut next).map_err(read_failed)?;
        let last = next_length == 0;
        let chunk = &mut current[..current_length];
        let tag = cipher.seal_in_place(&nonce(&prefix, counter, last), MAGIC, chunk);
        output.write_all(chunk).and_then(|()| output.write_all(&tag)).map_err(write_failed)?;
        if last {
            break;
        }
        counter = counter.checked_add(1).ok_or("the file is too large to seal")?;
        std::mem::swap(&mut current, &mut next);
        current_length = next_length;
    }
    output.sync_all().map_err(write_failed)
}

/// Decrypts a file [`seal_file`] wrote into the new file `target`, failing on a wrong key or
/// any change to the sealed file.
pub(super) fn open_file(source: &Path, target: &Path, key: &[u8; 32]) -> Result<(), String> {
    let mut input = File::open(source).map_err(|error| format!("cannot read {}: {error}", source.display()))?;
    let read_failed = |error: std::io::Error| format!("cannot read {}: {error}", source.display());
    let mut magic = [0u8; 8];
    let mut header = [0u8; SALT_BYTES + PREFIX_BYTES];
    if fill(&mut input, &mut magic).map_err(read_failed)? != magic.len() || &magic != MAGIC {
        return Err(format!("{} is not an encrypted EggDB archive", source.display()));
    }
    if fill(&mut input, &mut header).map_err(read_failed)? != header.len() {
        return Err(format!("{} is cut short", source.display()));
    }
    let (salt, prefix) = header.split_at(SALT_BYTES);
    let prefix: [u8; PREFIX_BYTES] = prefix.try_into().map_err(|_| "the nonce prefix has the wrong length".to_owned())?;
    let cipher = cipher(key, salt)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|error| format!("cannot create {}: {error}", target.display()))?;
    let sealed_bytes = CHUNK_BYTES + AEAD_TAG_BYTES;
    let mut current = vec![0u8; sealed_bytes];
    let mut next = vec![0u8; sealed_bytes];
    let mut current_length = fill(&mut input, &mut current).map_err(read_failed)?;
    let mut counter = 0u32;
    loop {
        let next_length = fill(&mut input, &mut next).map_err(read_failed)?;
        let last = next_length == 0;
        if current_length < AEAD_TAG_BYTES {
            return Err(format!("{} is cut short", source.display()));
        }
        let (chunk, tag) = current[..current_length].split_at_mut(current_length - AEAD_TAG_BYTES);
        let tag: [u8; AEAD_TAG_BYTES] = (&*tag).try_into().map_err(|_| "a chunk's tag is cut short".to_owned())?;
        cipher
            .open_in_place(&nonce(&prefix, counter, last), MAGIC, chunk, &tag)
            .map_err(|_| format!("{} does not open with this key, or it was changed", source.display()))?;
        output.write_all(chunk).map_err(|error| format!("cannot write {}: {error}", target.display()))?;
        if last {
            break;
        }
        counter = counter.checked_add(1).ok_or("the archive is too large")?;
        std::mem::swap(&mut current, &mut next);
        current_length = next_length;
    }
    output.sync_all().map_err(|error| format!("cannot write {}: {error}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_file_opens_with_its_key_and_fails_when_changed() {
        let directory = std::env::temp_dir().join(format!("eggdb-sealed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let plain = directory.join("plain");
        let content: Vec<u8> = (0..(CHUNK_BYTES * 2 + 17)).map(|index| (index % 251) as u8).collect();
        std::fs::write(&plain, &content).unwrap();
        let key = [7u8; 32];
        seal_file(&plain, &directory.join("sealed"), &key).unwrap();
        open_file(&directory.join("sealed"), &directory.join("opened"), &key).unwrap();
        assert_eq!(std::fs::read(directory.join("opened")).unwrap(), content);
        assert!(open_file(&directory.join("sealed"), &directory.join("wrong"), &[8u8; 32]).is_err());
        let mut sealed = std::fs::read(directory.join("sealed")).unwrap();
        sealed.truncate(sealed.len() - AEAD_TAG_BYTES - 1);
        std::fs::write(directory.join("cut"), &sealed).unwrap();
        assert!(open_file(&directory.join("cut"), &directory.join("cut-opened"), &key).is_err());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
