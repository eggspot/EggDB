//! Kerberos 5 logins through GSS-API (D1184), written from RFC 4120 (Kerberos), RFC 3961 and
//! RFC 3962 (the AES encryption types), RFC 1964 and RFC 4121 (the GSS-API Kerberos mechanism),
//! RFC 4178 (SPNEGO) and the MIT keytab file format. The server holds its service keys in a
//! keytab; a client's AP-REQ is checked by decrypting the service ticket with one of them and
//! the authenticator with the ticket's session key, its time and its replay checked, and the
//! client's principal names the user. When the client asks for mutual authentication the
//! server answers with an AP-REP. Only `aes256-cts-hmac-sha1-96` and `aes128-cts-hmac-sha1-96`
//! are accepted; DES, RC4 and the other weak types are not.

use crate::ber;
use eggdb_crypto::{Aes, Hmac, Sha1};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

const AES128: i32 = 17;
const AES256: i32 = 18;
const BLOCK: usize = 16;
const MAC_BYTES: usize = 12;
const USAGE_TICKET: u32 = 2;
const USAGE_AUTHENTICATOR: u32 = 11;
const USAGE_AP_REP: u32 = 12;
const MAX_SKEW_SECONDS: i64 = 300;
const GSS_CHECKSUM_TYPE: i64 = 0x8003;
const GSS_MUTUAL_FLAG: u32 = 2;
const MOST_TOKEN_BYTES: usize = 64 * 1024;
const MOST_REPLAYS: usize = 100_000;
const KRB5_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x12, 0x01, 0x02, 0x02];
const MS_KRB5_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x82, 0xf7, 0x12, 0x01, 0x02, 0x02];
const SPNEGO_OID: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x02];
const OID: u8 = 0x06;
const GENERAL_STRING: u8 = 0x1b;
const GENERALIZED_TIME: u8 = 0x18;
const BIT_STRING: u8 = 0x03;

/// Why a Kerberos login failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KerberosError(pub String);

impl std::fmt::Display for KerberosError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for KerberosError {}

fn failed(message: impl Into<String>) -> KerberosError {
    KerberosError(message.into())
}

/// One key of a keytab.
#[derive(Clone, PartialEq, Eq)]
pub struct KeytabEntry {
    /// The service principal's name components, such as `postgres` and `db.example.com`.
    pub components: Vec<String>,
    /// The principal's realm.
    pub realm: String,
    /// The key version number.
    pub kvno: u32,
    /// The encryption type: 17 or 18.
    pub enctype: i32,
    /// The key.
    pub key: Vec<u8>,
}

impl std::fmt::Debug for KeytabEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeytabEntry")
            .field("components", &self.components)
            .field("realm", &self.realm)
            .field("kvno", &self.kvno)
            .field("enctype", &self.enctype)
            .finish_non_exhaustive()
    }
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Cursor<'b> {
    fn take(&mut self, count: usize) -> Result<&'b [u8], KerberosError> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or_else(|| failed("the keytab ends inside an entry"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, KerberosError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, KerberosError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, KerberosError> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn counted_text(&mut self) -> Result<String, KerberosError> {
        let length = usize::from(self.u16()?);
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| failed("a keytab name is not UTF-8"))
    }
}

/// Reads an MIT keytab (format version `0x0502`), keeping the AES keys.
pub fn parse_keytab(bytes: &[u8]) -> Result<Vec<KeytabEntry>, KerberosError> {
    if bytes.get(..2) != Some(&[0x05, 0x02]) {
        return Err(failed("the keytab is not format 0x0502"));
    }
    let mut cursor = Cursor { bytes, at: 2 };
    let mut entries = Vec::new();
    while cursor.at < bytes.len() {
        let size = i32::from_be_bytes(cursor.take(4)?.try_into().map_err(|_| failed("a keytab entry size is short"))?);
        if size <= 0 {
            cursor.take(size.unsigned_abs() as usize)?;
            continue;
        }
        let record = cursor.take(size as usize)?;
        let mut entry = Cursor { bytes: record, at: 0 };
        let count = entry.u16()?;
        let realm = entry.counted_text()?;
        let mut components = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            components.push(entry.counted_text()?);
        }
        let _name_type = entry.u32()?;
        let _timestamp = entry.u32()?;
        let short_kvno = u32::from(entry.u8()?);
        let enctype = i32::from(entry.u16()?);
        let key_length = usize::from(entry.u16()?);
        let key = entry.take(key_length)?.to_vec();
        let kvno = if record.len() - entry.at >= 4 { entry.u32()? } else { short_kvno };
        if matches!(enctype, AES128 | AES256) && key.len() == key_bytes(enctype) {
            entries.push(KeytabEntry {
                components,
                realm,
                kvno,
                enctype,
                key,
            });
        }
    }
    Ok(entries)
}

fn key_bytes(enctype: i32) -> usize {
    if enctype == AES256 { 32 } else { 16 }
}

fn rotated_right(input: &[u8], bits: usize) -> Vec<u8> {
    let total = input.len() * 8;
    let mut out = vec![0u8; input.len()];
    for position in 0..total {
        let source = (position + total - bits % total) % total;
        if input[source / 8] & (0x80 >> (source % 8)) != 0 {
            out[position / 8] |= 0x80 >> (position % 8);
        }
    }
    out
}

fn greatest_common_divisor(left: usize, right: usize) -> usize {
    if right == 0 { left } else { greatest_common_divisor(right, left % right) }
}

/// RFC 3961's n-fold: `input` stretched or folded to `out_bytes` bytes.
pub fn n_fold(input: &[u8], out_bytes: usize) -> Vec<u8> {
    if input.is_empty() || out_bytes == 0 {
        return vec![0; out_bytes];
    }
    let common = input.len() / greatest_common_divisor(input.len(), out_bytes) * out_bytes;
    let mut stretched = Vec::with_capacity(common);
    let mut copy = 0;
    while stretched.len() < common {
        stretched.extend_from_slice(&rotated_right(input, 13 * copy));
        copy += 1;
    }
    let mut sum = vec![0u32; out_bytes];
    for chunk in stretched.chunks(out_bytes) {
        for (at, byte) in chunk.iter().enumerate() {
            sum[at] += u32::from(*byte);
        }
    }
    loop {
        let mut carry = 0u32;
        for value in sum.iter_mut().rev() {
            *value += carry;
            carry = *value >> 8;
            *value &= 0xff;
        }
        if carry == 0 {
            break;
        }
        if let Some(last) = sum.last_mut() {
            *last += carry;
        }
    }
    sum.into_iter().map(|value| value as u8).collect()
}

fn cipher(key: &[u8]) -> Result<Aes, KerberosError> {
    Aes::new(key).map_err(|_| failed("an AES key has the wrong length"))
}

fn encrypt_block(aes: &Aes, block: &[u8]) -> [u8; BLOCK] {
    let mut out = [0u8; BLOCK];
    out[..block.len().min(BLOCK)].copy_from_slice(&block[..block.len().min(BLOCK)]);
    aes.encrypt_block(&mut out);
    out
}

fn xored(left: &[u8], right: &[u8]) -> [u8; BLOCK] {
    let mut out = [0u8; BLOCK];
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = left.get(at).copied().unwrap_or(0) ^ right.get(at).copied().unwrap_or(0);
    }
    out
}

fn cts_encrypt(key: &[u8], plain: &[u8]) -> Result<Vec<u8>, KerberosError> {
    let aes = cipher(key)?;
    if plain.len() < BLOCK {
        return Err(failed("Kerberos encrypts at least one block"));
    }
    let mut blocks: Vec<[u8; BLOCK]> = Vec::new();
    let mut previous = [0u8; BLOCK];
    for chunk in plain.chunks(BLOCK) {
        let mut block = xored(chunk, &previous);
        aes.encrypt_block(&mut block);
        previous = block;
        blocks.push(block);
    }
    if blocks.len() == 1 {
        return Ok(blocks[0].to_vec());
    }
    let remainder = plain.len() - BLOCK * (blocks.len() - 1);
    let last = blocks.len() - 1;
    let mut out = Vec::with_capacity(plain.len());
    for block in &blocks[..last - 1] {
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&blocks[last]);
    out.extend_from_slice(&blocks[last - 1][..remainder]);
    Ok(out)
}

fn cts_decrypt(key: &[u8], sealed: &[u8]) -> Result<Vec<u8>, KerberosError> {
    let aes = cipher(key)?;
    if sealed.len() < BLOCK {
        return Err(failed("a Kerberos ciphertext is shorter than a block"));
    }
    if sealed.len() == BLOCK {
        let mut block = [0u8; BLOCK];
        block.copy_from_slice(sealed);
        aes.decrypt_block(&mut block);
        return Ok(block.to_vec());
    }
    let count = sealed.len().div_ceil(BLOCK);
    let remainder = sealed.len() - BLOCK * (count - 1);
    let full = BLOCK * (count - 2);
    let mut out = Vec::with_capacity(sealed.len());
    let mut previous = [0u8; BLOCK];
    for chunk in sealed[..full].chunks(BLOCK) {
        let mut block = [0u8; BLOCK];
        block.copy_from_slice(chunk);
        let kept = block;
        aes.decrypt_block(&mut block);
        out.extend_from_slice(&xored(&block, &previous));
        previous = kept;
    }
    let mut swapped = [0u8; BLOCK];
    swapped.copy_from_slice(&sealed[full..full + BLOCK]);
    let tail = &sealed[full + BLOCK..];
    aes.decrypt_block(&mut swapped);
    let mut second_last = [0u8; BLOCK];
    second_last[..remainder].copy_from_slice(tail);
    second_last[remainder..].copy_from_slice(&swapped[remainder..]);
    let last_plain: Vec<u8> = swapped[..remainder].iter().zip(tail).map(|(left, right)| left ^ right).collect();
    let mut decrypted = second_last;
    aes.decrypt_block(&mut decrypted);
    out.extend_from_slice(&xored(&decrypted, &previous));
    out.extend_from_slice(&last_plain);
    Ok(out)
}

fn derived_key(base: &[u8], constant: &[u8]) -> Result<Vec<u8>, KerberosError> {
    let aes = cipher(base)?;
    let mut block = n_fold(constant, BLOCK);
    let mut out = Vec::with_capacity(base.len());
    while out.len() < base.len() {
        let next = encrypt_block(&aes, &block);
        out.extend_from_slice(&next);
        block = next.to_vec();
    }
    out.truncate(base.len());
    Ok(out)
}

fn usage_key(base: &[u8], usage: u32, kind: u8) -> Result<Vec<u8>, KerberosError> {
    let mut constant = usage.to_be_bytes().to_vec();
    constant.push(kind);
    derived_key(base, &constant)
}

fn hmac_sha1(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha1>::new(key);
    mac.update(message);
    mac.finish().to_vec()
}

/// RFC 3962's string-to-key: PBKDF2-HMAC-SHA1 of the password and salt, then the derived key
/// for `"kerberos"`.
pub fn string_to_key(password: &str, salt: &str, iterations: u32, enctype: i32) -> Result<Vec<u8>, KerberosError> {
    let length = key_bytes(enctype);
    let mut output = vec![0u8; length];
    for (index, chunk) in output.chunks_mut(20).enumerate() {
        let mut salted = salt.as_bytes().to_vec();
        salted.extend_from_slice(&(index as u32 + 1).to_be_bytes());
        let mut round = hmac_sha1(password.as_bytes(), &salted);
        let mut block = round.clone();
        for _ in 1..iterations.max(1) {
            round = hmac_sha1(password.as_bytes(), &round);
            for (into, byte) in block.iter_mut().zip(&round) {
                *into ^= byte;
            }
        }
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
    derived_key(&output, b"kerberos")
}

/// Decrypts `sealed` with `base` for key usage `usage`, checking its HMAC.
pub fn decrypt(base: &[u8], usage: u32, sealed: &[u8]) -> Result<Vec<u8>, KerberosError> {
    if sealed.len() < BLOCK + MAC_BYTES {
        return Err(failed("a Kerberos ciphertext is too short"));
    }
    let (body, mac) = sealed.split_at(sealed.len() - MAC_BYTES);
    let plain = cts_decrypt(&usage_key(base, usage, 0xAA)?, body)?;
    let expected = hmac_sha1(&usage_key(base, usage, 0x55)?, &plain);
    if !eggdb_crypto::constant_time_eq(&expected[..MAC_BYTES], mac) {
        return Err(failed("a Kerberos message failed its integrity check (wrong key?)"));
    }
    Ok(plain[BLOCK..].to_vec())
}

/// Encrypts `plain` with `base` for key usage `usage`, after the 16-byte `confounder`.
pub fn encrypt(base: &[u8], usage: u32, plain: &[u8], confounder: [u8; BLOCK]) -> Result<Vec<u8>, KerberosError> {
    let mut whole = confounder.to_vec();
    whole.extend_from_slice(plain);
    let mut out = cts_encrypt(&usage_key(base, usage, 0xAA)?, &whole)?;
    out.extend_from_slice(&hmac_sha1(&usage_key(base, usage, 0x55)?, &whole)[..MAC_BYTES]);
    Ok(out)
}

fn element(bytes: &[u8], tag: u8) -> Result<ber::Element<'_>, KerberosError> {
    let (found, _) = ber::read(bytes).ok_or_else(|| failed("a Kerberos message is not valid DER"))?;
    if found.tag != tag {
        return Err(failed(format!("a Kerberos message has tag {:#04x} where {tag:#04x} belongs", found.tag)));
    }
    Ok(found)
}

fn application(bytes: &[u8], tag: u8) -> Result<HashMap<u8, &[u8]>, KerberosError> {
    fields(element(element(bytes, tag)?.content, ber::SEQUENCE)?.content)
}

fn fields(sequence: &[u8]) -> Result<HashMap<u8, &[u8]>, KerberosError> {
    let mut found = HashMap::new();
    let mut rest = sequence;
    while !rest.is_empty() {
        let (field, after) = ber::read(rest).ok_or_else(|| failed("a Kerberos message is not valid DER"))?;
        if field.tag & 0xE0 == 0xA0 {
            found.insert(field.tag & 0x1F, field.content);
        }
        rest = after;
    }
    Ok(found)
}

fn field<'f>(fields: &HashMap<u8, &'f [u8]>, number: u8) -> Result<&'f [u8], KerberosError> {
    fields.get(&number).copied().ok_or_else(|| failed(format!("a Kerberos message lacks field [{number}]")))
}

fn integer(content: &[u8]) -> Result<i64, KerberosError> {
    let value = element(content, ber::INTEGER)?.content;
    if value.is_empty() || value.len() > 8 {
        return Err(failed("a Kerberos integer is empty or too long"));
    }
    let mut number: i64 = if value[0] & 0x80 != 0 { -1 } else { 0 };
    for byte in value {
        number = (number << 8) | i64::from(*byte);
    }
    Ok(number)
}

fn octets(content: &[u8]) -> Result<&[u8], KerberosError> {
    Ok(element(content, ber::OCTET_STRING)?.content)
}

fn text(content: &[u8]) -> Result<String, KerberosError> {
    String::from_utf8(element(content, GENERAL_STRING)?.content.to_vec()).map_err(|_| failed("a Kerberos name is not UTF-8"))
}

fn principal(content: &[u8]) -> Result<Vec<String>, KerberosError> {
    let sequence = element(content, ber::SEQUENCE)?.content;
    let parts = fields(sequence)?;
    let mut names = Vec::new();
    let mut rest = element(field(&parts, 1)?, ber::SEQUENCE)?.content;
    while !rest.is_empty() {
        let (name, after) = ber::read(rest).ok_or_else(|| failed("a principal name is not valid DER"))?;
        if name.tag != GENERAL_STRING {
            return Err(failed("a principal name component is not a GeneralString"));
        }
        names.push(String::from_utf8(name.content.to_vec()).map_err(|_| failed("a principal name is not UTF-8"))?);
        rest = after;
    }
    Ok(names)
}

fn kerberos_time(content: &[u8]) -> Result<i64, KerberosError> {
    let raw = element(content, GENERALIZED_TIME)?.content;
    let text = std::str::from_utf8(raw).map_err(|_| failed("a Kerberos time is not text"))?;
    let digits = text.strip_suffix('Z').filter(|digits| digits.len() == 14 && digits.bytes().all(|byte| byte.is_ascii_digit())).ok_or_else(|| failed("a Kerberos time is not YYYYMMDDHHMMSSZ"))?;
    let part = |from: usize, to: usize| digits[from..to].parse::<i64>().unwrap_or(0);
    let (year, month, day) = (part(0, 4), part(4, 6), part(6, 8));
    let (hour, minute, second) = (part(8, 10), part(10, 12), part(12, 14));
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Ok(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn time_text(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}{:02}{:02}{:02}Z", rest / 3_600, rest % 3_600 / 60, rest % 60)
}

struct EncryptedData<'d> {
    etype: i32,
    kvno: Option<u32>,
    cipher: &'d [u8],
}

fn encrypted_data(content: &[u8]) -> Result<EncryptedData<'_>, KerberosError> {
    let parts = fields(element(content, ber::SEQUENCE)?.content)?;
    Ok(EncryptedData {
        etype: i32::try_from(integer(field(&parts, 0)?)?).map_err(|_| failed("an encryption type is out of range"))?,
        kvno: parts.get(&1).map(|kvno| integer(kvno)).transpose()?.and_then(|kvno| u32::try_from(kvno).ok()),
        cipher: octets(field(&parts, 2)?)?,
    })
}

/// What a checked AP-REQ said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The client's principal name components, such as `alice` or `alice/admin`.
    pub client: Vec<String>,
    /// The client's realm.
    pub realm: String,
    /// The GSS-API token to send back for mutual authentication, when the client asked for it.
    pub reply: Option<Vec<u8>>,
}

impl Accepted {
    /// The client's principal as `name@REALM`.
    pub fn principal(&self) -> String {
        format!("{}@{}", self.client.join("/"), self.realm)
    }
}

/// How the server accepts Kerberos logins: its keytab, the realm users must come from, and
/// whether the role name keeps the realm (`alice@EXAMPLE.COM`) or drops it (`alice`).
#[derive(Debug, Clone)]
pub struct KerberosConfig {
    /// The service keys.
    pub keys: Vec<KeytabEntry>,
    /// The only realm logins may come from; any when `None`.
    pub realm: Option<String>,
    /// Whether the user name includes the realm.
    pub include_realm: bool,
    replays: Arc<Mutex<HashMap<(String, i64, i64), i64>>>,
}

impl KerberosConfig {
    /// A configuration over `keys`.
    pub fn new(keys: Vec<KeytabEntry>, realm: Option<String>, include_realm: bool) -> Self {
        Self {
            keys,
            realm,
            include_realm,
            replays: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The user name a login with `accepted` stands for.
    pub fn user_of(&self, accepted: &Accepted) -> String {
        if self.include_realm {
            accepted.principal()
        } else {
            accepted.client.join("/")
        }
    }

    fn service_key(&self, realm: &str, service: &[String], ticket: &EncryptedData<'_>) -> Option<&KeytabEntry> {
        let mut matching = self
            .keys
            .iter()
            .filter(|entry| entry.enctype == ticket.etype && entry.realm == realm && entry.components == service);
        match ticket.kvno {
            Some(kvno) => matching.find(|entry| entry.kvno == kvno),
            None => matching.next(),
        }
    }

    fn fresh(&self, principal: &str, ctime: i64, cusec: i64, now: i64) -> bool {
        let mut replays = self.replays.lock().unwrap_or_else(PoisonError::into_inner);
        replays.retain(|_, expires| *expires > now);
        if replays.len() >= MOST_REPLAYS {
            return false;
        }
        replays.insert((principal.to_owned(), ctime, cusec), now + 2 * MAX_SKEW_SECONDS).is_none()
    }

    /// Checks a client's first GSS-API token: a Kerberos mechanism token holding an AP-REQ,
    /// bare or inside SPNEGO's `NegTokenInit`. `now` is the time in Unix seconds and `random`
    /// fills bytes for the reply's confounder and sequence number.
    pub fn accept(&self, token: &[u8], now: i64, random: &mut dyn FnMut(&mut [u8])) -> Result<Accepted, KerberosError> {
        if token.len() > MOST_TOKEN_BYTES {
            return Err(failed("the GSS-API token is too long"));
        }
        let (mechanism_token, spnego) = unwrapped(token)?;
        let request = gss_inner(mechanism_token, [0x01, 0x00])?;
        let mut accepted = self.accept_ap_request(request, now, random)?;
        if spnego {
            accepted.reply = Some(spnego_reply(accepted.reply.as_deref()));
        }
        Ok(accepted)
    }

    fn accept_ap_request(&self, request: &[u8], now: i64, random: &mut dyn FnMut(&mut [u8])) -> Result<Accepted, KerberosError> {
        let parts = application(request, 0x6E)?;
        if integer(field(&parts, 0)?)? != 5 || integer(field(&parts, 1)?)? != 14 {
            return Err(failed("the token is not a Kerberos 5 AP-REQ"));
        }
        let ticket_parts = application(field(&parts, 3)?, 0x61)?;
        let service_realm = text(field(&ticket_parts, 1)?)?;
        let service = principal(field(&ticket_parts, 2)?)?;
        let sealed_ticket = encrypted_data(field(&ticket_parts, 3)?)?;
        let entry = self
            .service_key(&service_realm, &service, &sealed_ticket)
            .ok_or_else(|| failed(format!("the keytab has no {} key for {}@{service_realm}", sealed_ticket.etype, service.join("/"))))?;
        let ticket = decrypt(&entry.key, USAGE_TICKET, sealed_ticket.cipher)?;
        let ticket = application(&ticket, 0x63)?;
        let session = fields(element(field(&ticket, 1)?, ber::SEQUENCE)?.content)?;
        let session_type = i32::try_from(integer(field(&session, 0)?)?).map_err(|_| failed("a session key type is out of range"))?;
        if !matches!(session_type, AES128 | AES256) {
            return Err(failed(format!("session key type {session_type} is not an AES type")));
        }
        let session_key = octets(field(&session, 1)?)?.to_vec();
        if session_key.len() != key_bytes(session_type) {
            return Err(failed("the session key has the wrong length"));
        }
        let client_realm = text(field(&ticket, 2)?)?;
        let client = principal(field(&ticket, 3)?)?;
        let end = kerberos_time(field(&ticket, 7)?)?;
        if now > end + MAX_SKEW_SECONDS {
            return Err(failed("the service ticket has expired"));
        }
        if let Some(start) = ticket.get(&6).map(|start| kerberos_time(start)).transpose()?
            && now + MAX_SKEW_SECONDS < start
        {
            return Err(failed("the service ticket is not valid yet"));
        }
        if let Some(realm) = &self.realm
            && *realm != client_realm
        {
            return Err(failed(format!("logins come from realm {realm}, not {client_realm}")));
        }
        let sealed_authenticator = encrypted_data(field(&parts, 4)?)?;
        if sealed_authenticator.etype != session_type {
            return Err(failed("the authenticator is not under the session key's type"));
        }
        let authenticator = decrypt(&session_key, USAGE_AUTHENTICATOR, sealed_authenticator.cipher)?;
        let authenticator = application(&authenticator, 0x62)?;
        if text(field(&authenticator, 1)?)? != client_realm || principal(field(&authenticator, 2)?)? != client {
            return Err(failed("the authenticator names another client than the ticket"));
        }
        let cusec = integer(field(&authenticator, 4)?)?;
        let ctime = kerberos_time(field(&authenticator, 5)?)?;
        if (now - ctime).abs() > MAX_SKEW_SECONDS {
            return Err(failed("the client's clock is more than five minutes off"));
        }
        let accepted_principal = format!("{}@{client_realm}", client.join("/"));
        if !self.fresh(&accepted_principal, ctime, cusec, now) {
            return Err(failed("the authenticator was replayed"));
        }
        let mutual = match authenticator.get(&3) {
            Some(checksum) => {
                let checksum = fields(element(checksum, ber::SEQUENCE)?.content)?;
                let kind = integer(field(&checksum, 0)?)?;
                let value = octets(field(&checksum, 1)?)?;
                kind == GSS_CHECKSUM_TYPE
                    && value.get(20..24).is_some_and(|flags| {
                        u32::from_le_bytes([flags[0], flags[1], flags[2], flags[3]]) & GSS_MUTUAL_FLAG != 0
                    })
            }
            None => false,
        };
        let reply = if mutual {
            let mut confounder = [0u8; BLOCK];
            random(&mut confounder);
            let mut sequence = [0u8; 4];
            random(&mut sequence);
            let sequence_number = u32::from_be_bytes(sequence) & 0x3FFF_FFFF;
            let encoded_time = time_text(ctime);
            let mut body = ber::element(0xA0, &ber::element(GENERALIZED_TIME, encoded_time.as_bytes()));
            body.extend(ber::element(0xA1, &ber::integer(u32::try_from(cusec).unwrap_or(0))));
            body.extend(ber::element(0xA3, &ber::integer(sequence_number)));
            let part = ber::element(0x7B, &ber::element(ber::SEQUENCE, &body));
            let sealed = encrypt(&session_key, USAGE_AP_REP, &part, confounder)?;
            let mut encrypted = ber::element(0xA0, &ber::integer(session_type as u32));
            encrypted.extend(ber::element(0xA2, &ber::element(ber::OCTET_STRING, &sealed)));
            let mut reply = ber::element(0xA0, &ber::integer(5));
            reply.extend(ber::element(0xA1, &ber::integer(15)));
            reply.extend(ber::element(0xA2, &ber::element(ber::SEQUENCE, &encrypted)));
            let ap_rep = ber::element(0x6F, &ber::element(ber::SEQUENCE, &reply));
            Some(gss_wrapped(&ap_rep, [0x02, 0x00]))
        } else {
            None
        };
        Ok(Accepted {
            client,
            realm: client_realm,
            reply,
        })
    }
}

fn gss_wrapped(inner: &[u8], token_id: [u8; 2]) -> Vec<u8> {
    let mut body = ber::element(OID, KRB5_OID);
    body.extend_from_slice(&token_id);
    body.extend_from_slice(inner);
    ber::element(0x60, &body)
}

fn gss_inner(token: &[u8], token_id: [u8; 2]) -> Result<&[u8], KerberosError> {
    let wrapper = element(token, 0x60)?.content;
    let (mechanism, rest) = ber::read(wrapper).ok_or_else(|| failed("the GSS-API token names no mechanism"))?;
    if mechanism.tag != OID || !(mechanism.content == KRB5_OID || mechanism.content == MS_KRB5_OID) {
        return Err(failed("the GSS-API token is not for the Kerberos mechanism"));
    }
    rest.strip_prefix(&token_id[..]).ok_or_else(|| failed("the GSS-API token is not an AP-REQ"))
}

fn unwrapped(token: &[u8]) -> Result<(&[u8], bool), KerberosError> {
    let wrapper = element(token, 0x60)?.content;
    let (mechanism, rest) = ber::read(wrapper).ok_or_else(|| failed("the GSS-API token names no mechanism"))?;
    if mechanism.tag != OID || mechanism.content != SPNEGO_OID {
        return Ok((token, false));
    }
    let init = element(rest, 0xA0)?.content;
    let parts = fields(element(init, ber::SEQUENCE)?.content)?;
    let offered = element(field(&parts, 0)?, ber::SEQUENCE)?.content;
    let mut mechanisms = offered;
    let mut kerberos = false;
    while !mechanisms.is_empty() {
        let (oid, after) = ber::read(mechanisms).ok_or_else(|| failed("SPNEGO's mechanism list is not valid DER"))?;
        kerberos |= oid.tag == OID && (oid.content == KRB5_OID || oid.content == MS_KRB5_OID);
        mechanisms = after;
    }
    if !kerberos {
        return Err(failed("SPNEGO offered no Kerberos mechanism"));
    }
    Ok((octets(field(&parts, 2).map_err(|_| failed("SPNEGO sent no Kerberos token"))?)?, true))
}

fn spnego_reply(reply: Option<&[u8]>) -> Vec<u8> {
    let mut body = ber::element(0xA0, &ber::element(ber::ENUMERATED, &[0]));
    body.extend(ber::element(0xA1, &ber::element(OID, KRB5_OID)));
    if let Some(reply) = reply {
        body.extend(ber::element(0xA2, &ber::element(ber::OCTET_STRING, reply)));
    }
    ber::element(0xA1, &ber::element(ber::SEQUENCE, &body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn n_fold_matches_rfc_3961() {
        assert_eq!(hex(&n_fold(b"012345", 8)), "be072631276b1955");
        assert_eq!(hex(&n_fold(b"password", 7)), "78a07b6caf85fa");
        assert_eq!(hex(&n_fold(b"Rough Consensus, and Running Code", 8)), "bb6ed30870b7f0e0");
        assert_eq!(hex(&n_fold(b"password", 21)), "59e4a8ca7c0385c3c37b3f6d2000247cb6e6bd5b3e");
        assert_eq!(hex(&n_fold(b"kerberos", 16)), "6b65726265726f737b9b5b2b93132b93");
    }

    #[test]
    fn string_to_key_matches_rfc_3962() {
        assert_eq!(
            hex(&string_to_key("password", "ATHENA.MIT.EDUraeburn", 1, AES128).unwrap()),
            "42263c6e89f4fc28b8df68ee09799f15"
        );
        assert_eq!(
            hex(&string_to_key("password", "ATHENA.MIT.EDUraeburn", 1, AES256).unwrap()),
            "fe697b52bc0d3ce14432ba036a92e65bbb52280990a2fa27883998d72af30161"
        );
    }

    #[test]
    fn sealed_messages_open_with_their_key_and_usage_only() {
        let key = string_to_key("secret", "EXAMPLE.COMpostgres", 4096, AES256).unwrap();
        for length in [0, 1, 15, 16, 17, 31, 32, 33, 100] {
            let plain: Vec<u8> = (0..length).map(|at| at as u8).collect();
            let sealed = encrypt(&key, 11, &plain, [7; 16]).unwrap();
            assert_eq!(decrypt(&key, 11, &sealed).unwrap(), plain, "{length}");
            assert!(decrypt(&key, 12, &sealed).is_err());
        }
    }

    #[test]
    fn kerberos_times_read_and_write_as_rfc_4120_says() {
        let encoded = ber::element(GENERALIZED_TIME, b"20261115093000Z");
        let seconds = kerberos_time(&encoded).unwrap();
        assert_eq!(time_text(seconds), "20261115093000Z");
        assert!(kerberos_time(&ber::element(GENERALIZED_TIME, b"2026111509Z")).is_err());
    }

    #[test]
    fn keytabs_keep_their_aes_keys() {
        let mut entry = Vec::new();
        entry.extend_from_slice(&2u16.to_be_bytes());
        entry.extend_from_slice(&11u16.to_be_bytes());
        entry.extend_from_slice(b"EXAMPLE.COM");
        entry.extend_from_slice(&8u16.to_be_bytes());
        entry.extend_from_slice(b"postgres");
        entry.extend_from_slice(&14u16.to_be_bytes());
        entry.extend_from_slice(b"db.example.com");
        entry.extend_from_slice(&1u32.to_be_bytes());
        entry.extend_from_slice(&0u32.to_be_bytes());
        entry.push(3);
        entry.extend_from_slice(&18u16.to_be_bytes());
        entry.extend_from_slice(&32u16.to_be_bytes());
        entry.extend_from_slice(&[9; 32]);
        let mut keytab = vec![0x05, 0x02];
        keytab.extend_from_slice(&(entry.len() as i32).to_be_bytes());
        keytab.extend_from_slice(&entry);
        let keys = parse_keytab(&keytab).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].components, ["postgres", "db.example.com"]);
        assert_eq!((keys[0].kvno, keys[0].enctype), (3, 18));
        assert!(parse_keytab(&keytab[..keytab.len() - 1]).is_err());
        assert!(parse_keytab(b"\x05\x01").is_err());
    }

    #[test]
    fn hostile_tokens_are_refused_without_panicking() {
        let config = KerberosConfig::new(Vec::new(), None, false);
        let mut random = |bytes: &mut [u8]| bytes.fill(1);
        for token in [&b""[..], b"\x60", b"\x60\x02\x06\x00", b"\x60\x80\xff\xff\xff\xff", &[0x60; 70_000]] {
            assert!(config.accept(token, 0, &mut random).is_err());
        }
    }

    fn sequence(parts: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (number, content) in parts {
            body.extend(ber::element(0xA0 | number, content));
        }
        ber::element(ber::SEQUENCE, &body)
    }

    fn name(kind: u32, parts: &[&str]) -> Vec<u8> {
        let mut names = Vec::new();
        for part in parts {
            names.extend(ber::element(GENERAL_STRING, part.as_bytes()));
        }
        sequence(&[(0, ber::integer(kind)), (1, ber::element(ber::SEQUENCE, &names))])
    }

    fn general(text: &str) -> Vec<u8> {
        ber::element(GENERAL_STRING, text.as_bytes())
    }

    #[test]
    fn an_ap_request_is_accepted_once_and_answered_with_an_ap_reply() {
        let service_key = string_to_key("service secret", "EXAMPLE.COMpostgresdb.example.com", 1, AES256).unwrap();
        let session_key = vec![5u8; 32];
        let now = 1_800_000_000;
        let when = ber::element(GENERALIZED_TIME, time_text(now).as_bytes());
        let later = ber::element(GENERALIZED_TIME, time_text(now + 3_600).as_bytes());
        let ticket_part = ber::element(
            0x63,
            &sequence(&[
                (0, ber::element(BIT_STRING, &[0, 0, 0, 0, 0])),
                (1, sequence(&[(0, ber::integer(18)), (1, ber::element(ber::OCTET_STRING, &session_key))])),
                (2, general("EXAMPLE.COM")),
                (3, name(1, &["alice"])),
                (4, sequence(&[(0, ber::integer(0)), (1, ber::element(ber::OCTET_STRING, b""))])),
                (5, when.clone()),
                (7, later),
            ]),
        );
        let ticket = ber::element(
            0x61,
            &sequence(&[
                (0, ber::integer(5)),
                (1, general("EXAMPLE.COM")),
                (2, name(2, &["postgres", "db.example.com"])),
                (
                    3,
                    sequence(&[
                        (0, ber::integer(18)),
                        (1, ber::integer(3)),
                        (2, ber::element(ber::OCTET_STRING, &encrypt(&service_key, USAGE_TICKET, &ticket_part, [1; 16]).unwrap())),
                    ]),
                ),
            ]),
        );
        let mut checksum = vec![16, 0, 0, 0];
        checksum.extend_from_slice(&[0; 16]);
        checksum.extend_from_slice(&(GSS_MUTUAL_FLAG | 0x20).to_le_bytes());
        let authenticator = ber::element(
            0x62,
            &sequence(&[
                (0, ber::integer(5)),
                (1, general("EXAMPLE.COM")),
                (2, name(1, &["alice"])),
                (3, sequence(&[(0, ber::integer(0x8003)), (1, ber::element(ber::OCTET_STRING, &checksum))])),
                (4, ber::integer(7)),
                (5, when),
            ]),
        );
        let request = ber::element(
            0x6E,
            &sequence(&[
                (0, ber::integer(5)),
                (1, ber::integer(14)),
                (2, ber::element(BIT_STRING, &[0, 0x20, 0, 0, 0])),
                (3, ticket),
                (
                    4,
                    sequence(&[
                        (0, ber::integer(18)),
                        (2, ber::element(ber::OCTET_STRING, &encrypt(&session_key, USAGE_AUTHENTICATOR, &authenticator, [2; 16]).unwrap())),
                    ]),
                ),
            ]),
        );
        let token = gss_wrapped(&request, [0x01, 0x00]);
        let config = KerberosConfig::new(
            vec![KeytabEntry {
                components: vec!["postgres".to_owned(), "db.example.com".to_owned()],
                realm: "EXAMPLE.COM".to_owned(),
                kvno: 3,
                enctype: AES256,
                key: service_key,
            }],
            Some("EXAMPLE.COM".to_owned()),
            false,
        );
        let mut random = |bytes: &mut [u8]| bytes.fill(9);
        let accepted = config.accept(&token, now + 10, &mut random).unwrap();
        assert_eq!(accepted.principal(), "alice@EXAMPLE.COM");
        assert_eq!(config.user_of(&accepted), "alice");
        let reply = accepted.reply.unwrap();
        let ap_rep = application(gss_inner(&reply, [0x02, 0x00]).unwrap(), 0x6F).unwrap();
        let sealed = encrypted_data(field(&ap_rep, 2).unwrap()).unwrap();
        let part = decrypt(&session_key, USAGE_AP_REP, sealed.cipher).unwrap();
        let part = application(&part, 0x7B).unwrap();
        assert_eq!(kerberos_time(field(&part, 0).unwrap()).unwrap(), now);
        assert!(config.accept(&token, now + 11, &mut random).unwrap_err().0.contains("replayed"));
        let late = KerberosConfig::new(config.keys.clone(), None, true);
        assert!(late.accept(&token, now + 4_000, &mut random).is_err());
        let elsewhere = KerberosConfig::new(config.keys.clone(), Some("OTHER.ORG".to_owned()), false);
        assert!(elsewhere.accept(&token, now, &mut random).unwrap_err().0.contains("realm"));
    }
}
