//! TLS 1.3 session resumption with tickets (RFC 8446 sections 4.2.11, 4.6.1 and 7.1): the server
//! seals what a resumed handshake needs into a ticket only it can open, sends it in a
//! NewSessionTicket after the handshake, and accepts it back as a pre-shared key whose binder
//! proves the client knows the key.

use crate::cipher_suite::HashKind;
use crate::codec::{CodecError, Reader, Writer};
use crate::handshake::{handshake_message, handshake_type};
use crate::key_schedule::{KeySchedule, derive_secret, finished_verify_data, hkdf_expand_label};
use eggdb_crypto::{AEAD_NONCE_BYTES, AEAD_TAG_BYTES, AesGcm, constant_time_eq};

/// How long a ticket may be used: seven days, the most RFC 8446 allows.
pub const TICKET_LIFETIME_SECONDS: u32 = 604_800;
/// The `psk_dhe_ke` mode: resumption with a fresh (EC)DHE exchange, the only mode EggDB accepts.
pub const PSK_DHE_KE: u8 = 1;
const TICKET_KEY_BYTES: usize = 32;
const TICKET_NONCE: [u8; 1] = [0];
const TICKET_AGE_TOLERANCE_MILLISECONDS: u64 = 10_000;

/// The key a server seals its tickets with, made once per process: tickets stop opening when the
/// server restarts, and every client then makes a full handshake again.
pub struct SessionTickets {
    cipher: AesGcm,
}

impl std::fmt::Debug for SessionTickets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionTickets { .. }")
    }
}

/// What a ticket carries: the cipher suite's hash, when it was issued, the age obfuscation and the
/// resumption key, and the host name the client asked for.
#[derive(Clone, PartialEq, Eq)]
pub struct TicketState {
    /// The cipher suite of the handshake that issued the ticket.
    pub cipher_suite: u16,
    /// When the ticket was issued, in milliseconds since the Unix epoch.
    pub issued_unix_milliseconds: u64,
    /// The value the client adds to the ticket's age before sending it.
    pub age_add: u32,
    /// The host name of the issuing handshake's `server_name`, empty when there was none.
    pub server_name: String,
    /// The pre-shared key a resumed handshake starts from.
    pub psk: Vec<u8>,
}

impl std::fmt::Debug for TicketState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TicketState")
            .field("cipher_suite", &self.cipher_suite)
            .field("issued_unix_milliseconds", &self.issued_unix_milliseconds)
            .finish_non_exhaustive()
    }
}

impl TicketState {
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_u16(self.cipher_suite);
        writer.write_u32((self.issued_unix_milliseconds >> 32) as u32);
        writer.write_u32(self.issued_unix_milliseconds as u32);
        writer.write_u32(self.age_add);
        writer.write_vector_u8(|name| name.write_bytes(self.server_name.as_bytes()));
        writer.write_vector_u8(|psk| psk.write_bytes(&self.psk));
        writer.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(bytes);
        let cipher_suite = reader.read_u16()?;
        let high = u64::from(reader.read_u32()?);
        let low = u64::from(reader.read_u32()?);
        let age_add = reader.read_u32()?;
        let server_name = String::from_utf8(reader.read_vector_u8(0..=255)?.to_vec())
            .map_err(|_| CodecError::LengthOutOfRange)?;
        let psk = reader.read_vector_u8(1..=255)?.to_vec();
        reader.finish()?;
        Ok(Self {
            cipher_suite,
            issued_unix_milliseconds: (high << 32) | low,
            age_add,
            server_name,
            psk,
        })
    }
}

impl SessionTickets {
    /// A fresh ticket key from `fill_random`.
    pub fn new(fill_random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>) -> std::io::Result<Self> {
        let mut key = [0u8; TICKET_KEY_BYTES];
        fill_random(&mut key)?;
        let cipher = AesGcm::new(&key).map_err(|_| std::io::Error::other("the ticket key has the wrong length"))?;
        Ok(Self { cipher })
    }

    /// `state` sealed into an opaque ticket: a random nonce, the encrypted state and its tag.
    pub fn seal(
        &self,
        state: &TicketState,
        fill_random: &mut dyn FnMut(&mut [u8]) -> std::io::Result<()>,
    ) -> std::io::Result<Vec<u8>> {
        let mut nonce = [0u8; AEAD_NONCE_BYTES];
        fill_random(&mut nonce)?;
        let mut body = state.encode();
        let tag = self.cipher.seal_in_place(&nonce, b"eggdb ticket", &mut body);
        let mut ticket = Vec::with_capacity(AEAD_NONCE_BYTES + body.len() + AEAD_TAG_BYTES);
        ticket.extend_from_slice(&nonce);
        ticket.extend_from_slice(&body);
        ticket.extend_from_slice(&tag);
        Ok(ticket)
    }

    /// The state in `ticket`, or `None` when this key did not seal it or it was changed.
    pub fn open(&self, ticket: &[u8]) -> Option<TicketState> {
        if ticket.len() < AEAD_NONCE_BYTES + AEAD_TAG_BYTES {
            return None;
        }
        let (nonce, rest) = ticket.split_at(AEAD_NONCE_BYTES);
        let (body, tag) = rest.split_at(rest.len() - AEAD_TAG_BYTES);
        let nonce: [u8; AEAD_NONCE_BYTES] = nonce.try_into().ok()?;
        let tag: [u8; AEAD_TAG_BYTES] = tag.try_into().ok()?;
        let mut body = body.to_vec();
        self.cipher
            .open_in_place(&nonce, b"eggdb ticket", &mut body, &tag)
            .ok()?;
        TicketState::decode(&body).ok()
    }
}

/// One identity the client offers in `pre_shared_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedIdentity {
    /// The ticket, as the server sent it.
    pub identity: Vec<u8>,
    /// The ticket's age in milliseconds plus its `age_add`, modulo 2^32.
    pub obfuscated_ticket_age: u32,
}

/// A client's `pre_shared_key` extension: its identities and one binder for each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedPsks {
    /// The identities, in the client's order.
    pub identities: Vec<OfferedIdentity>,
    /// The binders, one per identity, in the same order.
    pub binders: Vec<Vec<u8>>,
}

impl OfferedPsks {
    /// Parses the data of a ClientHello's `pre_shared_key` extension.
    pub fn decode(data: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(data);
        let identity_list = reader.read_vector_u16(7..=65_535)?;
        let mut identities = Vec::new();
        let mut list = Reader::new(identity_list);
        while !list.is_empty() {
            let identity = list.read_vector_u16(1..=65_535)?.to_vec();
            let obfuscated_ticket_age = list.read_u32()?;
            identities.push(OfferedIdentity {
                identity,
                obfuscated_ticket_age,
            });
        }
        let binder_list = reader.read_vector_u16(33..=65_535)?;
        let mut binders = Vec::new();
        let mut list = Reader::new(binder_list);
        while !list.is_empty() {
            binders.push(list.read_vector_u8(32..=255)?.to_vec());
        }
        reader.finish()?;
        if binders.len() != identities.len() {
            return Err(CodecError::LengthOutOfRange);
        }
        Ok(Self { identities, binders })
    }

    /// How many bytes the binders take at the end of the ClientHello, their list's length
    /// included: what is cut off to make the partial ClientHello the binders are computed over.
    pub fn binders_bytes(&self) -> usize {
        self.binders
            .iter()
            .fold(2usize, |total, binder| total.saturating_add(1 + binder.len()))
    }
}

/// The ClientHello message `message` (header included) without its binders, which RFC 8446
/// requires to be the last bytes of the message.
pub fn partial_client_hello<'m>(message: &'m [u8], offered: &OfferedPsks) -> Option<&'m [u8]> {
    message.get(..message.len().checked_sub(offered.binders_bytes())?)
}

/// The binder of `psk` over the transcript hash of the partial ClientHello (and, after a
/// HelloRetryRequest, the messages before it): the Finished value under the `res binder` key.
pub fn binder(hash: HashKind, psk: &[u8], partial_transcript_hash: &[u8]) -> Vec<u8> {
    let early = KeySchedule::new(hash, Some(psk));
    let binder_key = derive_secret(hash, early.stage_secret(), b"res binder", &hash.digest(&[]));
    finished_verify_data(hash, &binder_key, partial_transcript_hash)
}

/// Whether `received` is the binder `psk` gives over `partial_transcript_hash`, in constant time.
pub fn binder_matches(hash: HashKind, psk: &[u8], partial_transcript_hash: &[u8], received: &[u8]) -> bool {
    constant_time_eq(&binder(hash, psk, partial_transcript_hash), received)
}

/// Whether a ticket issued at `issued_unix_milliseconds` is still within its lifetime at
/// `now_unix_milliseconds`, and the age the client reports agrees with it within ten seconds.
pub fn ticket_is_fresh(state: &TicketState, obfuscated_ticket_age: u32, now_unix_milliseconds: u64) -> bool {
    let Some(age) = now_unix_milliseconds.checked_sub(state.issued_unix_milliseconds) else {
        return false;
    };
    if age > u64::from(TICKET_LIFETIME_SECONDS) * 1000 {
        return false;
    }
    let reported = u64::from(obfuscated_ticket_age.wrapping_sub(state.age_add));
    reported.abs_diff(age) <= TICKET_AGE_TOLERANCE_MILLISECONDS
}

/// The resumption master secret, from the master secret and the transcript hash through the
/// client's Finished.
pub fn resumption_master_secret(hash: HashKind, master_secret: &[u8], client_finished_hash: &[u8]) -> Vec<u8> {
    derive_secret(hash, master_secret, b"res master", client_finished_hash)
}

/// The pre-shared key of the ticket issued with `TICKET_NONCE` under `resumption_master_secret`.
pub fn ticket_psk(hash: HashKind, resumption_master_secret: &[u8]) -> Vec<u8> {
    hkdf_expand_label(hash, resumption_master_secret, b"resumption", &TICKET_NONCE, hash.digest_bytes())
}

/// The NewSessionTicket message for `ticket`, with the server's lifetime, `age_add` and nonce.
pub fn new_session_ticket_message(age_add: u32, ticket: &[u8]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.write_u32(TICKET_LIFETIME_SECONDS);
    writer.write_u32(age_add);
    writer.write_vector_u8(|nonce| nonce.write_bytes(&TICKET_NONCE));
    writer.write_vector_u16(|field| field.write_bytes(ticket));
    writer.write_vector_u16(|_| {});
    handshake_message(handshake_type::NEW_SESSION_TICKET, &writer.into_bytes())
}

/// The server's `pre_shared_key` extension data: the index of the identity it accepted.
pub fn selected_identity(index: u16) -> Vec<u8> {
    index.to_be_bytes().to_vec()
}

/// Milliseconds since the Unix epoch now, or 0 on a clock before it.
pub fn now_unix_milliseconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter_random() -> impl FnMut(&mut [u8]) -> std::io::Result<()> {
        let mut next = 1u8;
        move |bytes: &mut [u8]| {
            for byte in bytes {
                *byte = next;
                next = next.wrapping_add(7);
            }
            Ok(())
        }
    }

    fn state() -> TicketState {
        TicketState {
            cipher_suite: 0x1301,
            issued_unix_milliseconds: 1_700_000_000_000,
            age_add: 0x1234_5678,
            server_name: "db.example".to_owned(),
            psk: vec![9; 32],
        }
    }

    #[test]
    fn a_sealed_ticket_opens_only_under_its_key_and_unchanged() {
        let mut random = counter_random();
        let tickets = SessionTickets::new(&mut random).unwrap();
        let other = SessionTickets::new(&mut random).unwrap();
        let mut ticket = tickets.seal(&state(), &mut random).unwrap();
        assert_eq!(tickets.open(&ticket), Some(state()));
        assert_eq!(other.open(&ticket), None);
        let last = ticket.len() - 1;
        ticket[last] ^= 1;
        assert_eq!(tickets.open(&ticket), None);
    }

    #[test]
    fn a_ticket_s_age_must_agree_and_stay_within_its_lifetime() {
        let ticket = state();
        let issued = ticket.issued_unix_milliseconds;
        let reported = |age: u32| age.wrapping_add(ticket.age_add);
        assert!(ticket_is_fresh(&ticket, reported(5_000), issued + 5_000));
        assert!(!ticket_is_fresh(&ticket, reported(5_000), issued + 60_000));
        let past_lifetime = u64::from(TICKET_LIFETIME_SECONDS) * 1000 + 1;
        assert!(!ticket_is_fresh(&ticket, reported(past_lifetime as u32), issued + past_lifetime));
        assert!(!ticket_is_fresh(&ticket, reported(0), issued - 1));
    }

    #[test]
    fn offered_psks_parse_and_the_partial_hello_ends_before_the_binders() {
        let mut extension = Writer::new();
        extension.write_vector_u16(|identities| {
            identities.write_vector_u16(|identity| identity.write_bytes(b"ticket"));
            identities.write_u32(42);
        });
        extension.write_vector_u16(|binders| binders.write_vector_u8(|binder| binder.write_bytes(&[7; 32])));
        let data = extension.into_bytes();
        let offered = OfferedPsks::decode(&data).unwrap();
        assert_eq!(offered.identities[0].identity, b"ticket");
        assert_eq!(offered.identities[0].obfuscated_ticket_age, 42);
        assert_eq!(offered.binders_bytes(), 2 + 1 + 32);
        let mut message = b"hello".to_vec();
        message.extend_from_slice(&data);
        assert_eq!(
            partial_client_hello(&message, &offered).unwrap().len(),
            message.len() - 35
        );
    }

    #[test]
    fn a_binder_verifies_only_with_its_key_and_transcript() {
        let hash = HashKind::Sha256;
        let transcript = hash.digest(b"partial hello");
        let made = binder(hash, &[1; 32], &transcript);
        assert!(binder_matches(hash, &[1; 32], &transcript, &made));
        assert!(!binder_matches(hash, &[2; 32], &transcript, &made));
        assert!(!binder_matches(hash, &[1; 32], &hash.digest(b"other"), &made));
    }
}
