//! Mailboxes and messages on a key-value database (D892): each mailbox keeps its IMAP
//! `UIDVALIDITY` and next UID, and each message its UID, flags, internal date and raw bytes.

use eggdb_keyspace::{KeysRead, KeysWrite};

const VALIDITY_COUNTER: &[u8] = b"\x00validity";
const MAILBOX: u8 = b'B';
const MESSAGE: u8 = b'M';
const UPLOAD: u8 = b'U';
const SUBMISSION: u8 = b'S';
const COUNTER: u8 = b'N';
const PUSH: u8 = b'P';
const MAX_MAILBOX_NAME_BYTES: usize = 1024;

/// The mailbox every account has and new mail arrives in.
pub const INBOX: &str = "INBOX";

/// Why a mail store operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailError(pub String);

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MailError {}

/// The result of a mail store operation.
pub type MailResult<T> = Result<T, MailError>;

fn storage(message: String) -> MailError {
    MailError(message)
}

/// A mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// Its name, `INBOX` or a hierarchy separated by `/`.
    pub name: String,
    /// IMAP's `UIDVALIDITY`: a new mailbox of an old name gets a new one.
    pub uid_validity: u32,
    /// The UID the next message gets.
    pub uid_next: u32,
    /// Whether the user subscribed to it.
    pub subscribed: bool,
}

/// A stored message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    /// Its UID in its mailbox.
    pub uid: u32,
    /// Its flags, such as `\Seen`, in the order they were set.
    pub flags: Vec<String>,
    /// When it arrived, in seconds since 1970 (UTC).
    pub internal_date: u64,
    /// The message as received (RFC 5322).
    pub raw: Vec<u8>,
}

/// `INBOX` in any case is `INBOX`; other names stay as given.
pub fn canonical(name: &str) -> String {
    if name.eq_ignore_ascii_case(INBOX) {
        INBOX.to_owned()
    } else {
        name.to_owned()
    }
}

fn mailbox_key(name: &str) -> Vec<u8> {
    let mut key = vec![MAILBOX];
    key.extend_from_slice(name.as_bytes());
    key
}

fn message_prefix(name: &str) -> Vec<u8> {
    let mut key = vec![MESSAGE];
    key.extend_from_slice(name.as_bytes());
    key.push(0);
    key
}

fn message_key(name: &str, uid: u32) -> Vec<u8> {
    let mut key = message_prefix(name);
    key.extend_from_slice(&uid.to_be_bytes());
    key
}

fn mailbox_of(name: &str, bytes: &[u8]) -> Option<Mailbox> {
    Some(Mailbox {
        name: name.to_owned(),
        uid_validity: u32::from_be_bytes(bytes.get(0..4)?.try_into().ok()?),
        uid_next: u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?),
        subscribed: bytes.get(8).is_some_and(|flag| *flag != 0),
    })
}

fn mailbox_bytes(mailbox: &Mailbox) -> Vec<u8> {
    let mut bytes = mailbox.uid_validity.to_be_bytes().to_vec();
    bytes.extend_from_slice(&mailbox.uid_next.to_be_bytes());
    bytes.push(u8::from(mailbox.subscribed));
    bytes
}

fn message_of(uid: u32, bytes: &[u8]) -> Option<StoredMessage> {
    let internal_date = u64::from_be_bytes(bytes.get(0..8)?.try_into().ok()?);
    let flags_length = usize::from(u16::from_be_bytes(bytes.get(8..10)?.try_into().ok()?));
    let flags = std::str::from_utf8(bytes.get(10..10 + flags_length)?).ok()?;
    Some(StoredMessage {
        uid,
        flags: flags.split(' ').filter(|flag| !flag.is_empty()).map(str::to_owned).collect(),
        internal_date,
        raw: bytes.get(10 + flags_length..)?.to_vec(),
    })
}

fn message_bytes(message: &StoredMessage) -> MailResult<Vec<u8>> {
    let flags = message.flags.join(" ");
    let length = u16::try_from(flags.len()).map_err(|_| MailError("a message has too many flags".to_owned()))?;
    let mut bytes = message.internal_date.to_be_bytes().to_vec();
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(flags.as_bytes());
    bytes.extend_from_slice(&message.raw);
    Ok(bytes)
}

/// Every mailbox, by name.
pub fn mailboxes(keys: &dyn KeysRead) -> MailResult<Vec<Mailbox>> {
    let mut found = Vec::new();
    for (key, value) in keys.scan(&[MAILBOX]).map_err(storage)? {
        let Ok(name) = std::str::from_utf8(&key[1..]) else {
            continue;
        };
        if let Some(mailbox) = mailbox_of(name, &value) {
            found.push(mailbox);
        }
    }
    Ok(found)
}

/// The mailbox `name`, when it exists.
pub fn mailbox(keys: &dyn KeysRead, name: &str) -> MailResult<Option<Mailbox>> {
    let name = canonical(name);
    Ok(keys.get(&mailbox_key(&name)).map_err(storage)?.and_then(|bytes| mailbox_of(&name, &bytes)))
}

fn next_validity(keys: &mut dyn KeysWrite) -> MailResult<u32> {
    let current = keys
        .get(VALIDITY_COUNTER)
        .map_err(storage)?
        .and_then(|bytes| bytes.try_into().ok())
        .map_or(0, u32::from_be_bytes);
    let next = current.wrapping_add(1).max(1);
    keys.put(VALIDITY_COUNTER, &next.to_be_bytes()).map_err(storage)?;
    Ok(next)
}

fn put_mailbox(keys: &mut dyn KeysWrite, mailbox: &Mailbox) -> MailResult<()> {
    keys.put(&mailbox_key(&mailbox.name), &mailbox_bytes(mailbox)).map_err(storage)
}

/// Makes mailbox `name`; an error when it exists or the name is empty or too long.
pub fn create_mailbox(keys: &mut dyn KeysWrite, name: &str) -> MailResult<Mailbox> {
    let name = canonical(name.trim_end_matches('/'));
    if name.is_empty() || name.len() > MAX_MAILBOX_NAME_BYTES || name.contains(['\0', '\r', '\n']) {
        return Err(MailError("that mailbox name is not allowed".to_owned()));
    }
    if mailbox(&*keys, &name)?.is_some() {
        return Err(MailError(format!("mailbox {name} already exists")));
    }
    let created = Mailbox {
        name,
        uid_validity: next_validity(keys)?,
        uid_next: 1,
        subscribed: false,
    };
    put_mailbox(keys, &created)?;
    Ok(created)
}

/// The mailbox `name`, made first when it does not exist yet.
pub fn ensure_mailbox(keys: &mut dyn KeysWrite, name: &str) -> MailResult<Mailbox> {
    match mailbox(&*keys, name)? {
        Some(found) => Ok(found),
        None => create_mailbox(keys, name),
    }
}

/// Removes mailbox `name` and its messages; `INBOX` cannot go.
pub fn delete_mailbox(keys: &mut dyn KeysWrite, name: &str) -> MailResult<()> {
    let name = canonical(name);
    if name == INBOX {
        return Err(MailError("INBOX cannot be deleted".to_owned()));
    }
    if mailbox(&*keys, &name)?.is_none() {
        return Err(MailError(format!("mailbox {name} does not exist")));
    }
    for (key, _) in keys.scan(&message_prefix(&name)).map_err(storage)? {
        keys.delete(&key).map_err(storage)?;
    }
    keys.delete(&mailbox_key(&name)).map_err(storage)?;
    Ok(())
}

/// Renames mailbox `from` to `to`, its messages with it; renaming `INBOX` moves its messages
/// into the new mailbox and leaves it empty, as IMAP says.
pub fn rename_mailbox(keys: &mut dyn KeysWrite, from: &str, to: &str) -> MailResult<()> {
    let from = canonical(from);
    let Some(source) = mailbox(&*keys, &from)? else {
        return Err(MailError(format!("mailbox {from} does not exist")));
    };
    let mut target = create_mailbox(keys, to)?;
    for message in messages(&*keys, &from)? {
        keys.delete(&message_key(&from, message.uid)).map_err(storage)?;
        let moved = StoredMessage {
            uid: target.uid_next,
            ..message
        };
        target.uid_next += 1;
        keys.put(&message_key(&target.name, moved.uid), &message_bytes(&moved)?).map_err(storage)?;
    }
    target.subscribed = source.subscribed;
    put_mailbox(keys, &target)?;
    if from != INBOX {
        keys.delete(&mailbox_key(&from)).map_err(storage)?;
    }
    Ok(())
}

/// Subscribes to or unsubscribes from mailbox `name`.
pub fn subscribe(keys: &mut dyn KeysWrite, name: &str, subscribed: bool) -> MailResult<()> {
    let Some(mut found) = mailbox(&*keys, name)? else {
        return Err(MailError(format!("mailbox {name} does not exist")));
    };
    found.subscribed = subscribed;
    put_mailbox(keys, &found)
}

/// Stores `raw` in mailbox `name` (made when missing) with `flags`; returns its UID.
pub fn append(keys: &mut dyn KeysWrite, name: &str, flags: &[String], internal_date: u64, raw: &[u8]) -> MailResult<u32> {
    let mut target = ensure_mailbox(keys, name)?;
    let uid = target.uid_next;
    target.uid_next = uid.checked_add(1).ok_or_else(|| MailError("the mailbox's UIDs are used up".to_owned()))?;
    let message = StoredMessage {
        uid,
        flags: flags.to_vec(),
        internal_date,
        raw: raw.to_vec(),
    };
    keys.put(&message_key(&target.name, uid), &message_bytes(&message)?).map_err(storage)?;
    put_mailbox(keys, &target)?;
    Ok(uid)
}

/// The messages of mailbox `name`, by UID.
pub fn messages(keys: &dyn KeysRead, name: &str) -> MailResult<Vec<StoredMessage>> {
    let name = canonical(name);
    let prefix = message_prefix(&name);
    let mut found = Vec::new();
    for (key, value) in keys.scan(&prefix).map_err(storage)? {
        let Some(uid) = key.get(prefix.len()..).and_then(|bytes| bytes.try_into().ok()).map(u32::from_be_bytes) else {
            continue;
        };
        if let Some(message) = message_of(uid, &value) {
            found.push(message);
        }
    }
    Ok(found)
}

/// Replaces the flags of message `uid` in mailbox `name`.
pub fn set_flags(keys: &mut dyn KeysWrite, name: &str, uid: u32, flags: &[String]) -> MailResult<()> {
    let name = canonical(name);
    let key = message_key(&name, uid);
    let Some(bytes) = keys.get(&key).map_err(storage)? else {
        return Err(MailError(format!("message {uid} does not exist")));
    };
    let Some(mut message) = message_of(uid, &bytes) else {
        return Err(MailError(format!("message {uid} is damaged")));
    };
    message.flags = flags.to_vec();
    keys.put(&key, &message_bytes(&message)?).map_err(storage)
}

/// Removes message `uid` from mailbox `name`.
pub fn remove(keys: &mut dyn KeysWrite, name: &str, uid: u32) -> MailResult<bool> {
    keys.delete(&message_key(&canonical(name), uid)).map_err(storage)
}

fn upload_key(id: &str) -> Vec<u8> {
    let mut key = vec![UPLOAD];
    key.extend_from_slice(id.as_bytes());
    key
}

/// An uploaded blob (JMAP's upload, RFC 8620 section 6.1): its media type, when it came and
/// its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    /// Its media type as the client gave it.
    pub content_type: String,
    /// When it was uploaded, in seconds since 1970.
    pub uploaded_at: u64,
    /// Its bytes.
    pub bytes: Vec<u8>,
}

/// Keeps an uploaded blob under `id`.
pub fn put_upload(keys: &mut dyn KeysWrite, id: &str, upload: &Upload) -> MailResult<()> {
    let content_type = upload.content_type.as_bytes();
    let length = u16::try_from(content_type.len()).map_err(|_| MailError("the media type is too long".to_owned()))?;
    let mut value = upload.uploaded_at.to_be_bytes().to_vec();
    value.extend_from_slice(&length.to_be_bytes());
    value.extend_from_slice(content_type);
    value.extend_from_slice(&upload.bytes);
    keys.put(&upload_key(id), &value).map_err(storage)
}

/// The uploaded blob `id`, if it is still kept.
pub fn upload(keys: &dyn KeysRead, id: &str) -> MailResult<Option<Upload>> {
    let Some(value) = keys.get(&upload_key(id)).map_err(storage)? else {
        return Ok(None);
    };
    let read = || -> Option<Upload> {
        let uploaded_at = u64::from_be_bytes(value.get(..8)?.try_into().ok()?);
        let length = usize::from(u16::from_be_bytes(value.get(8..10)?.try_into().ok()?));
        let content_type = String::from_utf8(value.get(10..10 + length)?.to_vec()).ok()?;
        Some(Upload {
            content_type,
            uploaded_at,
            bytes: value.get(10 + length..)?.to_vec(),
        })
    };
    Ok(read())
}

/// The next number of the counter `name`, starting at 1.
pub fn next_number(keys: &mut dyn KeysWrite, name: &str) -> MailResult<u64> {
    let mut key = vec![COUNTER];
    key.extend_from_slice(name.as_bytes());
    let next = keys
        .get(&key)
        .map_err(storage)?
        .and_then(|value| value.as_slice().try_into().ok())
        .map_or(0, u64::from_be_bytes)
        + 1;
    keys.put(&key, &next.to_be_bytes()).map_err(storage)?;
    Ok(next)
}

/// Keeps a JMAP EmailSubmission, as the JSON object `EmailSubmission/get` returns.
pub fn put_submission(keys: &mut dyn KeysWrite, id: &str, json: &str) -> MailResult<()> {
    let mut key = vec![SUBMISSION];
    key.extend_from_slice(id.as_bytes());
    keys.put(&key, json.as_bytes()).map_err(storage)
}

/// Every kept EmailSubmission, as (id, JSON), in id order.
pub fn submissions(keys: &dyn KeysRead) -> MailResult<Vec<(String, String)>> {
    Ok(keys
        .scan(&[SUBMISSION])
        .map_err(storage)?
        .into_iter()
        .map(|(key, value)| (String::from_utf8_lossy(&key[1..]).into_owned(), String::from_utf8_lossy(&value).into_owned()))
        .collect())
}

/// Keeps a JMAP PushSubscription as JSON.
pub fn put_push(keys: &mut dyn KeysWrite, id: &str, json: &str) -> MailResult<()> {
    let mut key = vec![PUSH];
    key.extend_from_slice(id.as_bytes());
    keys.put(&key, json.as_bytes()).map_err(storage)
}

/// Removes the PushSubscription `id`; whether it was kept.
pub fn remove_push(keys: &mut dyn KeysWrite, id: &str) -> MailResult<bool> {
    let mut key = vec![PUSH];
    key.extend_from_slice(id.as_bytes());
    keys.delete(&key).map_err(storage)
}

/// Every kept PushSubscription, as (id, JSON).
pub fn pushes(keys: &dyn KeysRead) -> MailResult<Vec<(String, String)>> {
    Ok(keys
        .scan(&[PUSH])
        .map_err(storage)?
        .into_iter()
        .map(|(key, value)| (String::from_utf8_lossy(&key[1..]).into_owned(), String::from_utf8_lossy(&value).into_owned()))
        .collect())
}

/// Removes the uploads made before `before`; returns how many.
pub fn remove_uploads_before(keys: &mut dyn KeysWrite, before: u64) -> MailResult<usize> {
    let mut stale = Vec::new();
    for (key, value) in keys.scan(&[UPLOAD]).map_err(storage)? {
        let uploaded_at = value.get(..8).and_then(|bytes| bytes.try_into().ok()).map_or(0, u64::from_be_bytes);
        if uploaded_at < before {
            stale.push(key);
        }
    }
    for key in &stale {
        keys.delete(key).map_err(storage)?;
    }
    Ok(stale.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::{Keyspace, MemoryKeyspace};

    #[test]
    fn mailboxes_keep_messages_flags_and_uids() {
        let keyspace = MemoryKeyspace::default();
        keyspace
            .write(0, &mut |keys| {
                let fail = |error: MailError| error.0;
                assert_eq!(append(keys, "inbox", &[], 10, b"Subject: a\r\n\r\nhi").map_err(fail)?, 1);
                assert_eq!(append(keys, "INBOX", &["\\Seen".to_owned()], 11, b"Subject: b\r\n\r\nyo").map_err(fail)?, 2);
                set_flags(keys, INBOX, 1, &["\\Flagged".to_owned()]).map_err(fail)?;
                let stored = messages(&*keys, INBOX).map_err(fail)?;
                assert_eq!(stored.len(), 2);
                assert_eq!(stored[0].flags, vec!["\\Flagged".to_owned()]);
                assert_eq!(stored[1].raw, b"Subject: b\r\n\r\nyo");
                rename_mailbox(keys, INBOX, "Archive/2026").map_err(fail)?;
                assert!(messages(&*keys, INBOX).map_err(fail)?.is_empty());
                assert_eq!(messages(&*keys, "Archive/2026").map_err(fail)?.len(), 2);
                assert!(delete_mailbox(keys, INBOX).is_err());
                assert!(create_mailbox(keys, "Archive/2026").is_err());
                delete_mailbox(keys, "Archive/2026").map_err(fail)?;
                assert_eq!(mailboxes(&*keys).map_err(fail)?.len(), 1);
                Ok(())
            })
            .unwrap();
    }
}
