use eggdb_bson::{Bson, Document, read_document};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const OP_MSG: i32 = 2013;
const HEADER_BYTES: usize = 16;
const HEADER_AFTER_LENGTH_BYTES: usize = 12;
const FLAG_BYTES: usize = 4;
const MAX_MESSAGE_BYTES: usize = 48 << 20;
const SCRAM_MECHANISM: &str = "SCRAM-SHA-256";

#[derive(Debug)]
pub(crate) struct MongoClient {
    stream: TcpStream,
    next_request: i32,
}

fn field<'t>(message: &'t str, name: char) -> Option<&'t str> {
    message
        .split(',')
        .find_map(|part| part.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')))
}

fn client_nonce() -> String {
    let seed = format!(
        "{:?}{}",
        std::time::SystemTime::now(),
        std::process::id()
    );
    eggdb_crypto::base64_encode(&eggdb_crypto::sha256(seed.as_bytes())[..18])
}

fn payload_of(reply: &Document) -> Result<Vec<u8>, String> {
    match reply.get("payload") {
        Some(Bson::Binary { bytes, .. }) => Ok(bytes.clone()),
        _ => Err("the SASL reply has no payload".to_owned()),
    }
}

impl MongoClient {
    pub(crate) fn connect(address: &str) -> Result<Self, String> {
        let stream = TcpStream::connect(address).map_err(|error| format!("{address}: {error}"))?;
        stream.set_nodelay(true).map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .map_err(|error| error.to_string())?;
        Ok(Self { stream, next_request: 1 })
    }

    fn exchange(&mut self, body: &Document) -> Result<Document, String> {
        let bson = body.to_bytes();
        let length = HEADER_BYTES + 4 + 1 + bson.len();
        let mut message = Vec::with_capacity(length);
        message.extend_from_slice(&i32::try_from(length).map_err(|_| "the command is too large")?.to_le_bytes());
        message.extend_from_slice(&self.next_request.to_le_bytes());
        message.extend_from_slice(&0i32.to_le_bytes());
        message.extend_from_slice(&OP_MSG.to_le_bytes());
        message.extend_from_slice(&[0u8; FLAG_BYTES]);
        message.push(0);
        message.extend_from_slice(&bson);
        self.next_request = self.next_request.wrapping_add(1);
        self.stream.write_all(&message).map_err(|error| format!("sending: {error}"))?;
        let mut length = [0u8; 4];
        self.stream.read_exact(&mut length).map_err(|error| format!("receiving: {error}"))?;
        let length = usize::try_from(i32::from_le_bytes(length)).map_err(|_| "a negative reply length")?;
        if !(HEADER_BYTES + 5..=MAX_MESSAGE_BYTES).contains(&length) {
            return Err(format!("a reply of {length} bytes"));
        }
        let mut rest = vec![0u8; length - 4];
        self.stream.read_exact(&mut rest).map_err(|error| format!("receiving: {error}"))?;
        let op_code = rest
            .get(8..12)
            .and_then(|bytes| bytes.try_into().ok())
            .map(i32::from_le_bytes);
        if op_code != Some(OP_MSG) {
            return Err(format!("a reply with op code {op_code:?}"));
        }
        let sections = rest
            .get(HEADER_AFTER_LENGTH_BYTES + FLAG_BYTES..)
            .ok_or("a reply without sections")?;
        let (&kind, document) = sections.split_first().ok_or("a reply without sections")?;
        if kind != 0 {
            return Err(format!("a reply starting with section kind {kind}"));
        }
        let (reply, _) = read_document(document).map_err(|error| format!("reading the reply: {error:?}"))?;
        Ok(reply)
    }

    pub(crate) fn command(&mut self, database: &str, mut body: Document) -> Result<Document, String> {
        body.insert("$db", Bson::String(database.to_owned()));
        let reply = self.exchange(&body)?;
        let ok = reply.get("ok").and_then(Bson::as_f64).unwrap_or(0.0);
        if ok != 1.0 {
            let message = reply.get_str("errmsg").unwrap_or("the command failed");
            return Err(message.to_owned());
        }
        if let Some(Bson::Array(errors)) = reply.get("writeErrors")
            && !errors.is_empty()
        {
            return Err(format!("write errors: {errors:?}"));
        }
        Ok(reply)
    }

    pub(crate) fn authenticate(&mut self, user: &str, password: &str) -> Result<(), String> {
        let nonce = client_nonce();
        let first_bare = format!("n={},r={nonce}", user.replace('=', "=3D").replace(',', "=2C"));
        let start = Document::from_entries(vec![
            ("saslStart".to_owned(), Bson::Int32(1)),
            ("mechanism".to_owned(), Bson::String(SCRAM_MECHANISM.to_owned())),
            (
                "payload".to_owned(),
                Bson::Binary {
                    subtype: 0,
                    bytes: format!("n,,{first_bare}").into_bytes(),
                },
            ),
            ("autoAuthorize".to_owned(), Bson::Int32(1)),
        ]);
        let reply = self.command("admin", start)?;
        let conversation = reply.get("conversationId").cloned().unwrap_or(Bson::Int32(1));
        let server_first = String::from_utf8(payload_of(&reply)?).map_err(|_| "a server-first message that is not UTF-8")?;
        let combined_nonce = field(&server_first, 'r').ok_or("no nonce in the server-first message")?;
        if !combined_nonce.starts_with(&nonce) {
            return Err("the server's nonce does not extend ours".to_owned());
        }
        let salt = field(&server_first, 's')
            .and_then(|salt| eggdb_crypto::base64_decode(salt.as_bytes()).ok())
            .ok_or("no salt in the server-first message")?;
        let iterations: u32 = field(&server_first, 'i')
            .and_then(|count| count.parse().ok())
            .ok_or("no iteration count in the server-first message")?;
        let mut salted = [0u8; 32];
        eggdb_crypto::pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations, &mut salted);
        let client_key = eggdb_crypto::hmac_sha256(&salted, b"Client Key");
        let stored_key = eggdb_crypto::sha256(&client_key);
        let without_proof = format!("c=biws,r={combined_nonce}");
        let auth_message = format!("{first_bare},{server_first},{without_proof}");
        let signature = eggdb_crypto::hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(signature).map(|(key, signed)| key ^ signed).collect();
        let client_final = format!("{without_proof},p={}", eggdb_crypto::base64_encode(&proof));
        let mut payload = client_final.into_bytes();
        loop {
            let next = Document::from_entries(vec![
                ("saslContinue".to_owned(), Bson::Int32(1)),
                ("conversationId".to_owned(), conversation.clone()),
                ("payload".to_owned(), Bson::Binary { subtype: 0, bytes: payload }),
            ]);
            let reply = self.command("admin", next)?;
            if reply.get_bool("done") == Some(true) {
                return Ok(());
            }
            payload = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scram_fields_are_read_by_their_letter() {
        let message = "r=abc123,s=c2FsdA==,i=4096";
        assert_eq!(field(message, 'r'), Some("abc123"));
        assert_eq!(field(message, 's'), Some("c2FsdA=="));
        assert_eq!(field(message, 'i'), Some("4096"));
        assert_eq!(field(message, 'x'), None);
        assert_eq!(client_nonce().len(), 24);
    }
}
