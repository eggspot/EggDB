//! Handshake messages of TLS 1.3 (RFC 8446 section 4) and the TLS 1.2 messages EggDB needs
//! (RFC 5246, RFC 8422), with the extensions both use, plus the buffer that reassembles
//! messages split across records. Decoding checks every length against its definition and never
//! panics.

use crate::codec::{CodecError, Reader, Writer};

/// Handshake message types.
pub mod handshake_type {
    /// `client_hello`.
    pub const CLIENT_HELLO: u8 = 1;
    /// `server_hello` (also a HelloRetryRequest).
    pub const SERVER_HELLO: u8 = 2;
    /// `new_session_ticket`.
    pub const NEW_SESSION_TICKET: u8 = 4;
    /// `end_of_early_data`.
    pub const END_OF_EARLY_DATA: u8 = 5;
    /// `encrypted_extensions`.
    pub const ENCRYPTED_EXTENSIONS: u8 = 8;
    /// `certificate`.
    pub const CERTIFICATE: u8 = 11;
    /// `server_key_exchange` (TLS 1.2).
    pub const SERVER_KEY_EXCHANGE: u8 = 12;
    /// `certificate_request`.
    pub const CERTIFICATE_REQUEST: u8 = 13;
    /// `server_hello_done` (TLS 1.2).
    pub const SERVER_HELLO_DONE: u8 = 14;
    /// `certificate_verify`.
    pub const CERTIFICATE_VERIFY: u8 = 15;
    /// `client_key_exchange` (TLS 1.2).
    pub const CLIENT_KEY_EXCHANGE: u8 = 16;
    /// `finished`.
    pub const FINISHED: u8 = 20;
    /// `key_update`.
    pub const KEY_UPDATE: u8 = 24;
    /// `message_hash`, the synthetic message that replaces ClientHello1 after a retry.
    pub const MESSAGE_HASH: u8 = 254;
}

/// Extension types.
pub mod extension_type {
    /// `server_name` (RFC 6066).
    pub const SERVER_NAME: u16 = 0;
    /// `supported_groups`.
    pub const SUPPORTED_GROUPS: u16 = 10;
    /// `ec_point_formats` (RFC 8422).
    pub const EC_POINT_FORMATS: u16 = 11;
    /// `signature_algorithms`.
    pub const SIGNATURE_ALGORITHMS: u16 = 13;
    /// `application_layer_protocol_negotiation` (RFC 7301).
    pub const APPLICATION_LAYER_PROTOCOL_NEGOTIATION: u16 = 16;
    /// `extended_master_secret` (RFC 7627).
    pub const EXTENDED_MASTER_SECRET: u16 = 23;
    /// `session_ticket` (RFC 5077).
    pub const SESSION_TICKET: u16 = 35;
    /// `pre_shared_key`.
    pub const PRE_SHARED_KEY: u16 = 41;
    /// `early_data`.
    pub const EARLY_DATA: u16 = 42;
    /// `supported_versions`.
    pub const SUPPORTED_VERSIONS: u16 = 43;
    /// `cookie`.
    pub const COOKIE: u16 = 44;
    /// `psk_key_exchange_modes`.
    pub const PSK_KEY_EXCHANGE_MODES: u16 = 45;
    /// `signature_algorithms_cert`.
    pub const SIGNATURE_ALGORITHMS_CERT: u16 = 50;
    /// `key_share`.
    pub const KEY_SHARE: u16 = 51;
    /// `renegotiation_info` (RFC 5746).
    pub const RENEGOTIATION_INFO: u16 = 0xff01;
}

/// Key exchange groups.
pub mod named_group {
    /// `secp256r1` (P-256).
    pub const SECP256R1: u16 = 0x0017;
    /// `x25519`.
    pub const X25519: u16 = 0x001d;
}

/// Signature schemes.
pub mod signature_scheme {
    /// `rsa_pkcs1_sha256` (certificates and TLS 1.2 only).
    pub const RSA_PKCS1_SHA256: u16 = 0x0401;
    /// `rsa_pkcs1_sha384`.
    pub const RSA_PKCS1_SHA384: u16 = 0x0501;
    /// `rsa_pkcs1_sha512`.
    pub const RSA_PKCS1_SHA512: u16 = 0x0601;
    /// `ecdsa_secp256r1_sha256`.
    pub const ECDSA_SECP256R1_SHA256: u16 = 0x0403;
    /// `ecdsa_secp384r1_sha384`.
    pub const ECDSA_SECP384R1_SHA384: u16 = 0x0503;
    /// `ecdsa_secp521r1_sha512`.
    pub const ECDSA_SECP521R1_SHA512: u16 = 0x0603;
    /// `rsa_pss_rsae_sha256`.
    pub const RSA_PSS_RSAE_SHA256: u16 = 0x0804;
    /// `rsa_pss_rsae_sha384`.
    pub const RSA_PSS_RSAE_SHA384: u16 = 0x0805;
    /// `rsa_pss_rsae_sha512`.
    pub const RSA_PSS_RSAE_SHA512: u16 = 0x0806;
    /// `ed25519`.
    pub const ED25519: u16 = 0x0807;
}

/// The `random` of a ServerHello that marks it as a HelloRetryRequest (SHA-256 of
/// "HelloRetryRequest").
pub const HELLO_RETRY_REQUEST_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91,
    0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// The last eight bytes of a TLS 1.3 server's random when it negotiates TLS 1.2 (RFC 8446 section
/// 4.1.3), so a client that supports TLS 1.3 detects a downgrade.
pub const DOWNGRADE_TO_TLS12_SENTINEL: [u8; 8] = *b"DOWNGRD\x01";

/// The largest handshake message EggDB accepts: room for a chain of eight large certificates.
pub const MAX_HANDSHAKE_MESSAGE_BYTES: usize = 1 << 18;
const HANDSHAKE_HEADER_BYTES: usize = 4;
const MAX_SESSION_ID_BYTES: usize = 32;
const VECTOR_U16_MAX: usize = 0xffff;
const VECTOR_U24_MAX: usize = 0xff_ffff;

/// A handshake message with its four-byte header.
pub fn handshake_message(message_type: u8, body: &[u8]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.write_u8(message_type);
    writer.write_vector_u24(|contents| contents.write_bytes(body));
    writer.into_bytes()
}

/// One extension: its type and its still-encoded data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    /// The extension type.
    pub extension_type: u16,
    /// The extension data.
    pub data: Vec<u8>,
}

impl Extension {
    fn build(extension_type: u16, contents: impl FnOnce(&mut Writer)) -> Self {
        let mut writer = Writer::new();
        contents(&mut writer);
        Self { extension_type, data: writer.into_bytes() }
    }

    /// `server_name` with one host name.
    pub fn server_name(host_name: &str) -> Self {
        Self::build(extension_type::SERVER_NAME, |writer| {
            writer.write_vector_u16(|list| {
                list.write_u8(0);
                list.write_vector_u16(|name| name.write_bytes(host_name.as_bytes()));
            });
        })
    }

    /// An empty extension, such as a server's `server_name` acknowledgement or
    /// `extended_master_secret`.
    pub fn empty(extension_type: u16) -> Self {
        Self { extension_type, data: Vec::new() }
    }

    /// A client's `supported_versions`.
    pub fn client_supported_versions(versions: &[u16]) -> Self {
        Self::build(extension_type::SUPPORTED_VERSIONS, |writer| {
            writer.write_vector_u8(|list| versions.iter().for_each(|version| list.write_u16(*version)));
        })
    }

    /// A server's `supported_versions`: the one version it selected.
    pub fn server_supported_version(version: u16) -> Self {
        Self::build(extension_type::SUPPORTED_VERSIONS, |writer| writer.write_u16(version))
    }

    /// `supported_groups`.
    pub fn supported_groups(groups: &[u16]) -> Self {
        Self::build(extension_type::SUPPORTED_GROUPS, |writer| {
            writer.write_vector_u16(|list| groups.iter().for_each(|group| list.write_u16(*group)));
        })
    }

    /// `signature_algorithms`.
    pub fn signature_algorithms(schemes: &[u16]) -> Self {
        Self::build(extension_type::SIGNATURE_ALGORITHMS, |writer| {
            writer.write_vector_u16(|list| schemes.iter().for_each(|scheme| list.write_u16(*scheme)));
        })
    }

    /// A client's `key_share` with its offered shares.
    pub fn client_key_shares(shares: &[KeyShareEntry]) -> Self {
        Self::build(extension_type::KEY_SHARE, |writer| {
            writer.write_vector_u16(|list| shares.iter().for_each(|share| share.write(list)));
        })
    }

    /// A ServerHello's `key_share`.
    pub fn server_key_share(share: &KeyShareEntry) -> Self {
        Self::build(extension_type::KEY_SHARE, |writer| share.write(writer))
    }

    /// A HelloRetryRequest's `key_share`: the group the client must use.
    pub fn retry_key_share(group: u16) -> Self {
        Self::build(extension_type::KEY_SHARE, |writer| writer.write_u16(group))
    }

    /// `application_layer_protocol_negotiation` with `protocols` (a server sends exactly one).
    pub fn application_protocols(protocols: &[&[u8]]) -> Self {
        Self::build(extension_type::APPLICATION_LAYER_PROTOCOL_NEGOTIATION, |writer| {
            writer.write_vector_u16(|list| {
                for protocol in protocols {
                    list.write_vector_u8(|name| name.write_bytes(protocol));
                }
            });
        })
    }

    /// `psk_key_exchange_modes`.
    pub fn psk_key_exchange_modes(modes: &[u8]) -> Self {
        Self::build(extension_type::PSK_KEY_EXCHANGE_MODES, |writer| {
            writer.write_vector_u8(|list| list.write_bytes(modes));
        })
    }

    /// `ec_point_formats` offering only the uncompressed form.
    pub fn uncompressed_point_format() -> Self {
        Self::build(extension_type::EC_POINT_FORMATS, |writer| {
            writer.write_vector_u8(|list| list.write_u8(0));
        })
    }

    /// An empty `renegotiation_info`, which says renegotiation is never done (RFC 5746).
    pub fn empty_renegotiation_info() -> Self {
        Self::build(extension_type::RENEGOTIATION_INFO, |writer| writer.write_vector_u8(|_| {}))
    }
}

fn read_extensions(reader: &mut Reader<'_>) -> Result<Vec<Extension>, CodecError> {
    let mut list = Reader::new(reader.read_vector_u16(0..=VECTOR_U16_MAX)?);
    let mut extensions: Vec<Extension> = Vec::new();
    while !list.is_empty() {
        let extension_type = list.read_u16()?;
        let data = list.read_vector_u16(0..=VECTOR_U16_MAX)?.to_vec();
        if extensions.iter().any(|seen| seen.extension_type == extension_type) {
            return Err(CodecError::IllegalValue);
        }
        extensions.push(Extension { extension_type, data });
    }
    Ok(extensions)
}

fn write_extensions(writer: &mut Writer, extensions: &[Extension]) {
    writer.write_vector_u16(|list| {
        for extension in extensions {
            list.write_u16(extension.extension_type);
            list.write_vector_u16(|data| data.write_bytes(&extension.data));
        }
    });
}

fn find_extension(extensions: &[Extension], extension_type: u16) -> Option<&[u8]> {
    extensions
        .iter()
        .find(|extension| extension.extension_type == extension_type)
        .map(|extension| extension.data.as_slice())
}

fn read_u16_list(data: &[u8], prefix_bounds: std::ops::RangeInclusive<usize>, u8_prefix: bool) -> Result<Vec<u16>, CodecError> {
    let mut reader = Reader::new(data);
    let bytes = if u8_prefix {
        reader.read_vector_u8(prefix_bounds)?
    } else {
        reader.read_vector_u16(prefix_bounds)?
    };
    reader.finish()?;
    if bytes.len() % 2 != 0 {
        return Err(CodecError::LengthOutOfRange);
    }
    Ok(bytes.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect())
}

fn read_alpn_list(data: &[u8]) -> Result<Vec<Vec<u8>>, CodecError> {
    let mut reader = Reader::new(data);
    let mut list = Reader::new(reader.read_vector_u16(2..=VECTOR_U16_MAX)?);
    reader.finish()?;
    let mut protocols = Vec::new();
    while !list.is_empty() {
        protocols.push(list.read_vector_u8(1..=255)?.to_vec());
    }
    Ok(protocols)
}

/// One key share: a group and the sender's public value in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyShareEntry {
    /// The named group.
    pub group: u16,
    /// The public key in the group's encoding.
    pub key_exchange: Vec<u8>,
}

impl KeyShareEntry {
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let group = reader.read_u16()?;
        let key_exchange = reader.read_vector_u16(1..=VECTOR_U16_MAX)?.to_vec();
        Ok(Self { group, key_exchange })
    }

    fn write(&self, writer: &mut Writer) {
        writer.write_u16(self.group);
        writer.write_vector_u16(|key| key.write_bytes(&self.key_exchange));
    }
}

/// A ClientHello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    /// The legacy version, `0x0303` for TLS 1.2 and 1.3.
    pub legacy_version: u16,
    /// 32 random bytes.
    pub random: [u8; 32],
    /// The legacy session ID (TLS 1.3 clients send 32 random bytes for middlebox compatibility).
    pub legacy_session_id: Vec<u8>,
    /// The cipher suites offered, in the client's order.
    pub cipher_suites: Vec<u16>,
    /// The compression methods; only `null` (0) is acceptable.
    pub compression_methods: Vec<u8>,
    /// The extensions.
    pub extensions: Vec<Extension>,
}

impl ClientHello {
    /// Parses the body of a ClientHello message.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let legacy_version = reader.read_u16()?;
        let random = reader.read_array()?;
        let legacy_session_id = reader.read_vector_u8(0..=MAX_SESSION_ID_BYTES)?.to_vec();
        let suites = reader.read_vector_u16(2..=VECTOR_U16_MAX - 1)?;
        if suites.len() % 2 != 0 {
            return Err(CodecError::LengthOutOfRange);
        }
        let cipher_suites = suites.chunks_exact(2).map(|pair| u16::from_be_bytes([pair[0], pair[1]])).collect();
        let compression_methods = reader.read_vector_u8(1..=255)?.to_vec();
        let extensions = if reader.is_empty() { Vec::new() } else { read_extensions(&mut reader)? };
        reader.finish()?;
        Ok(Self { legacy_version, random, legacy_session_id, cipher_suites, compression_methods, extensions })
    }

    /// The whole ClientHello message, header included.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_u16(self.legacy_version);
        writer.write_bytes(&self.random);
        writer.write_vector_u8(|id| id.write_bytes(&self.legacy_session_id));
        writer.write_vector_u16(|list| self.cipher_suites.iter().for_each(|suite| list.write_u16(*suite)));
        writer.write_vector_u8(|list| list.write_bytes(&self.compression_methods));
        write_extensions(&mut writer, &self.extensions);
        handshake_message(handshake_type::CLIENT_HELLO, &writer.into_bytes())
    }

    /// The data of the extension of `extension_type`, if present.
    pub fn extension(&self, extension_type: u16) -> Option<&[u8]> {
        find_extension(&self.extensions, extension_type)
    }

    /// The host name in `server_name`, if the client sent one.
    pub fn server_name(&self) -> Result<Option<String>, CodecError> {
        let Some(data) = self.extension(extension_type::SERVER_NAME) else {
            return Ok(None);
        };
        let mut reader = Reader::new(data);
        let mut list = Reader::new(reader.read_vector_u16(1..=VECTOR_U16_MAX)?);
        reader.finish()?;
        let mut host_name = None;
        while !list.is_empty() {
            let name_type = list.read_u8()?;
            let name = list.read_vector_u16(1..=VECTOR_U16_MAX)?;
            if name_type == 0 {
                if host_name.is_some() {
                    return Err(CodecError::IllegalValue);
                }
                let text = std::str::from_utf8(name).map_err(|_| CodecError::IllegalValue)?;
                if !text.is_ascii() || text.contains('\0') {
                    return Err(CodecError::IllegalValue);
                }
                host_name = Some(text.to_owned());
            }
        }
        Ok(host_name)
    }

    /// The versions in `supported_versions`, empty when the extension is absent (a TLS 1.2 client).
    pub fn supported_versions(&self) -> Result<Vec<u16>, CodecError> {
        match self.extension(extension_type::SUPPORTED_VERSIONS) {
            Some(data) => read_u16_list(data, 2..=254, true),
            None => Ok(Vec::new()),
        }
    }

    /// The groups in `supported_groups`.
    pub fn supported_groups(&self) -> Result<Vec<u16>, CodecError> {
        match self.extension(extension_type::SUPPORTED_GROUPS) {
            Some(data) => read_u16_list(data, 2..=VECTOR_U16_MAX, false),
            None => Ok(Vec::new()),
        }
    }

    /// The schemes in `signature_algorithms`.
    pub fn signature_algorithms(&self) -> Result<Vec<u16>, CodecError> {
        match self.extension(extension_type::SIGNATURE_ALGORITHMS) {
            Some(data) => read_u16_list(data, 2..=VECTOR_U16_MAX - 1, false),
            None => Ok(Vec::new()),
        }
    }

    /// The offered key shares; two shares of one group are refused.
    pub fn key_shares(&self) -> Result<Vec<KeyShareEntry>, CodecError> {
        let Some(data) = self.extension(extension_type::KEY_SHARE) else {
            return Ok(Vec::new());
        };
        let mut reader = Reader::new(data);
        let mut list = Reader::new(reader.read_vector_u16(0..=VECTOR_U16_MAX)?);
        reader.finish()?;
        let mut shares: Vec<KeyShareEntry> = Vec::new();
        while !list.is_empty() {
            let share = KeyShareEntry::read(&mut list)?;
            if shares.iter().any(|seen| seen.group == share.group) {
                return Err(CodecError::IllegalValue);
            }
            shares.push(share);
        }
        Ok(shares)
    }

    /// The protocols in ALPN.
    pub fn application_protocols(&self) -> Result<Vec<Vec<u8>>, CodecError> {
        match self.extension(extension_type::APPLICATION_LAYER_PROTOCOL_NEGOTIATION) {
            Some(data) => read_alpn_list(data),
            None => Ok(Vec::new()),
        }
    }

    /// The modes in `psk_key_exchange_modes`.
    pub fn psk_key_exchange_modes(&self) -> Result<Vec<u8>, CodecError> {
        let Some(data) = self.extension(extension_type::PSK_KEY_EXCHANGE_MODES) else {
            return Ok(Vec::new());
        };
        let mut reader = Reader::new(data);
        let modes = reader.read_vector_u8(1..=255)?.to_vec();
        reader.finish()?;
        Ok(modes)
    }
}

/// A ServerHello, or a HelloRetryRequest when its random is [`HELLO_RETRY_REQUEST_RANDOM`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    /// The legacy version, `0x0303`.
    pub legacy_version: u16,
    /// 32 random bytes.
    pub random: [u8; 32],
    /// The client's legacy session ID, echoed.
    pub legacy_session_id_echo: Vec<u8>,
    /// The selected cipher suite.
    pub cipher_suite: u16,
    /// The selected compression method, always 0.
    pub compression_method: u8,
    /// The extensions.
    pub extensions: Vec<Extension>,
}

impl ServerHello {
    /// Parses the body of a ServerHello message.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let legacy_version = reader.read_u16()?;
        let random = reader.read_array()?;
        let legacy_session_id_echo = reader.read_vector_u8(0..=MAX_SESSION_ID_BYTES)?.to_vec();
        let cipher_suite = reader.read_u16()?;
        let compression_method = reader.read_u8()?;
        let extensions = if reader.is_empty() { Vec::new() } else { read_extensions(&mut reader)? };
        reader.finish()?;
        Ok(Self { legacy_version, random, legacy_session_id_echo, cipher_suite, compression_method, extensions })
    }

    /// The whole ServerHello message, header included.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_u16(self.legacy_version);
        writer.write_bytes(&self.random);
        writer.write_vector_u8(|id| id.write_bytes(&self.legacy_session_id_echo));
        writer.write_u16(self.cipher_suite);
        writer.write_u8(self.compression_method);
        write_extensions(&mut writer, &self.extensions);
        handshake_message(handshake_type::SERVER_HELLO, &writer.into_bytes())
    }

    /// Whether this is a HelloRetryRequest.
    pub fn is_hello_retry_request(&self) -> bool {
        self.random == HELLO_RETRY_REQUEST_RANDOM
    }

    /// The data of the extension of `extension_type`, if present.
    pub fn extension(&self, extension_type: u16) -> Option<&[u8]> {
        find_extension(&self.extensions, extension_type)
    }

    /// The version in `supported_versions`, or `None` for a TLS 1.2 ServerHello.
    pub fn selected_version(&self) -> Result<Option<u16>, CodecError> {
        let Some(data) = self.extension(extension_type::SUPPORTED_VERSIONS) else {
            return Ok(None);
        };
        let mut reader = Reader::new(data);
        let version = reader.read_u16()?;
        reader.finish()?;
        Ok(Some(version))
    }

    /// The server's key share of a ServerHello.
    pub fn key_share(&self) -> Result<Option<KeyShareEntry>, CodecError> {
        let Some(data) = self.extension(extension_type::KEY_SHARE) else {
            return Ok(None);
        };
        let mut reader = Reader::new(data);
        let share = KeyShareEntry::read(&mut reader)?;
        reader.finish()?;
        Ok(Some(share))
    }

    /// The group a HelloRetryRequest asks for.
    pub fn retry_group(&self) -> Result<Option<u16>, CodecError> {
        let Some(data) = self.extension(extension_type::KEY_SHARE) else {
            return Ok(None);
        };
        let mut reader = Reader::new(data);
        let group = reader.read_u16()?;
        reader.finish()?;
        Ok(Some(group))
    }

    /// The protocol a TLS 1.2 server selected with ALPN.
    pub fn application_protocol(&self) -> Result<Option<Vec<u8>>, CodecError> {
        selected_application_protocol(&self.extensions)
    }
}

fn selected_application_protocol(extensions: &[Extension]) -> Result<Option<Vec<u8>>, CodecError> {
    let Some(data) = find_extension(extensions, extension_type::APPLICATION_LAYER_PROTOCOL_NEGOTIATION) else {
        return Ok(None);
    };
    let mut protocols = read_alpn_list(data)?;
    if protocols.len() != 1 {
        return Err(CodecError::IllegalValue);
    }
    Ok(protocols.pop())
}

/// EncryptedExtensions (TLS 1.3).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EncryptedExtensions {
    /// The extensions.
    pub extensions: Vec<Extension>,
}

impl EncryptedExtensions {
    /// Parses the body of an EncryptedExtensions message.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let extensions = read_extensions(&mut reader)?;
        reader.finish()?;
        Ok(Self { extensions })
    }

    /// The whole message, header included.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        write_extensions(&mut writer, &self.extensions);
        handshake_message(handshake_type::ENCRYPTED_EXTENSIONS, &writer.into_bytes())
    }

    /// The protocol the server selected with ALPN.
    pub fn application_protocol(&self) -> Result<Option<Vec<u8>>, CodecError> {
        selected_application_protocol(&self.extensions)
    }
}

/// One certificate of a Certificate message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateEntry {
    /// The DER certificate.
    pub certificate: Vec<u8>,
    /// Its extensions (TLS 1.3 only).
    pub extensions: Vec<Extension>,
}

/// A Certificate message: the sender's chain, leaf first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CertificateMessage {
    /// The certificate request context (TLS 1.3; empty in a server's Certificate).
    pub request_context: Vec<u8>,
    /// The chain, leaf first.
    pub entries: Vec<CertificateEntry>,
}

impl CertificateMessage {
    /// A message carrying `chain` (DER certificates, leaf first) with no extensions.
    pub fn from_chain(chain: &[&[u8]]) -> Self {
        Self {
            request_context: Vec::new(),
            entries: chain
                .iter()
                .map(|certificate| CertificateEntry { certificate: certificate.to_vec(), extensions: Vec::new() })
                .collect(),
        }
    }

    /// Parses a TLS 1.3 Certificate body.
    pub fn decode_tls13(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let request_context = reader.read_vector_u8(0..=255)?.to_vec();
        let mut list = Reader::new(reader.read_vector_u24(0..=VECTOR_U24_MAX)?);
        reader.finish()?;
        let mut entries = Vec::new();
        while !list.is_empty() {
            let certificate = list.read_vector_u24(1..=VECTOR_U24_MAX)?.to_vec();
            let extensions = read_extensions(&mut list)?;
            entries.push(CertificateEntry { certificate, extensions });
        }
        Ok(Self { request_context, entries })
    }

    /// Parses a TLS 1.2 Certificate body.
    pub fn decode_tls12(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let mut list = Reader::new(reader.read_vector_u24(0..=VECTOR_U24_MAX)?);
        reader.finish()?;
        let mut entries = Vec::new();
        while !list.is_empty() {
            let certificate = list.read_vector_u24(1..=VECTOR_U24_MAX)?.to_vec();
            entries.push(CertificateEntry { certificate, extensions: Vec::new() });
        }
        Ok(Self { request_context: Vec::new(), entries })
    }

    /// The whole TLS 1.3 message, header included.
    pub fn encode_tls13(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_vector_u8(|context| context.write_bytes(&self.request_context));
        writer.write_vector_u24(|list| {
            for entry in &self.entries {
                list.write_vector_u24(|certificate| certificate.write_bytes(&entry.certificate));
                write_extensions(list, &entry.extensions);
            }
        });
        handshake_message(handshake_type::CERTIFICATE, &writer.into_bytes())
    }

    /// The whole TLS 1.2 message, header included.
    pub fn encode_tls12(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_vector_u24(|list| {
            for entry in &self.entries {
                list.write_vector_u24(|certificate| certificate.write_bytes(&entry.certificate));
            }
        });
        handshake_message(handshake_type::CERTIFICATE, &writer.into_bytes())
    }
}

/// A signature with its scheme: TLS 1.3's CertificateVerify, and the `digitally-signed` field of
/// TLS 1.2's ServerKeyExchange and CertificateVerify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigitallySigned {
    /// The signature scheme.
    pub scheme: u16,
    /// The signature.
    pub signature: Vec<u8>,
}

impl DigitallySigned {
    fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let scheme = reader.read_u16()?;
        let signature = reader.read_vector_u16(0..=VECTOR_U16_MAX)?.to_vec();
        Ok(Self { scheme, signature })
    }

    fn write(&self, writer: &mut Writer) {
        writer.write_u16(self.scheme);
        writer.write_vector_u16(|signature| signature.write_bytes(&self.signature));
    }

    /// Parses the body of a CertificateVerify message.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let signed = Self::read(&mut reader)?;
        reader.finish()?;
        Ok(signed)
    }

    /// The whole CertificateVerify message, header included.
    pub fn encode_certificate_verify(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        self.write(&mut writer);
        handshake_message(handshake_type::CERTIFICATE_VERIFY, &writer.into_bytes())
    }
}

/// What a TLS 1.3 CertificateVerify signs (RFC 8446 section 4.4.3): 64 spaces, a context
/// string, a zero byte and the transcript hash.
pub fn certificate_verify_content(from_server: bool, transcript_hash: &[u8]) -> Vec<u8> {
    let context: &[u8] = if from_server {
        b"TLS 1.3, server CertificateVerify"
    } else {
        b"TLS 1.3, client CertificateVerify"
    };
    let mut content = vec![0x20u8; 64];
    content.extend_from_slice(context);
    content.push(0);
    content.extend_from_slice(transcript_hash);
    content
}

/// The whole Finished message carrying `verify_data`, header included.
pub fn finished_message(verify_data: &[u8]) -> Vec<u8> {
    handshake_message(handshake_type::FINISHED, verify_data)
}

/// A NewSessionTicket (TLS 1.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSessionTicket {
    /// How long the ticket may be used, in seconds.
    pub lifetime_seconds: u32,
    /// The value added to the ticket age.
    pub age_add: u32,
    /// The nonce the pre-shared key is derived with.
    pub nonce: Vec<u8>,
    /// The opaque ticket.
    pub ticket: Vec<u8>,
    /// The extensions.
    pub extensions: Vec<Extension>,
}

impl NewSessionTicket {
    /// Parses the body of a NewSessionTicket message.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let lifetime_seconds = reader.read_u32()?;
        let age_add = reader.read_u32()?;
        let nonce = reader.read_vector_u8(0..=255)?.to_vec();
        let ticket = reader.read_vector_u16(1..=VECTOR_U16_MAX)?.to_vec();
        let extensions = read_extensions(&mut reader)?;
        reader.finish()?;
        Ok(Self { lifetime_seconds, age_add, nonce, ticket, extensions })
    }

    /// The whole message, header included.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_u32(self.lifetime_seconds);
        writer.write_u32(self.age_add);
        writer.write_vector_u8(|nonce| nonce.write_bytes(&self.nonce));
        writer.write_vector_u16(|ticket| ticket.write_bytes(&self.ticket));
        write_extensions(&mut writer, &self.extensions);
        handshake_message(handshake_type::NEW_SESSION_TICKET, &writer.into_bytes())
    }
}

/// Parses the body of a KeyUpdate: whether the peer asks for an update in return.
pub fn decode_key_update(body: &[u8]) -> Result<bool, CodecError> {
    let mut reader = Reader::new(body);
    let request = reader.read_u8()?;
    reader.finish()?;
    match request {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(CodecError::IllegalValue),
    }
}

/// The whole KeyUpdate message.
pub fn key_update_message(update_requested: bool) -> Vec<u8> {
    handshake_message(handshake_type::KEY_UPDATE, &[u8::from(update_requested)])
}

/// A TLS 1.3 CertificateRequest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateRequest {
    /// The request context, echoed in the client's Certificate.
    pub request_context: Vec<u8>,
    /// The extensions; `signature_algorithms` is required.
    pub extensions: Vec<Extension>,
}

impl CertificateRequest {
    /// Parses the body of a CertificateRequest message.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        let request_context = reader.read_vector_u8(0..=255)?.to_vec();
        let extensions = read_extensions(&mut reader)?;
        reader.finish()?;
        if find_extension(&extensions, extension_type::SIGNATURE_ALGORITHMS).is_none() {
            return Err(CodecError::IllegalValue);
        }
        Ok(Self { request_context, extensions })
    }

    /// The whole message, header included.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_vector_u8(|context| context.write_bytes(&self.request_context));
        write_extensions(&mut writer, &self.extensions);
        handshake_message(handshake_type::CERTIFICATE_REQUEST, &writer.into_bytes())
    }

    /// The schemes in `signature_algorithms`.
    pub fn signature_algorithms(&self) -> Result<Vec<u16>, CodecError> {
        match find_extension(&self.extensions, extension_type::SIGNATURE_ALGORITHMS) {
            Some(data) => read_u16_list(data, 2..=VECTOR_U16_MAX - 1, false),
            None => Ok(Vec::new()),
        }
    }
}

const NAMED_CURVE: u8 = 3;

/// A TLS 1.2 ServerKeyExchange for ECDHE (RFC 8422 section 5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcdheServerKeyExchange {
    /// The named group.
    pub group: u16,
    /// The server's ephemeral public key.
    pub public_key: Vec<u8>,
    /// The signature over client random, server random and the parameters.
    pub signed: DigitallySigned,
}

impl EcdheServerKeyExchange {
    /// The `ServerECDHParams` bytes the signature covers after the two randoms.
    pub fn params(group: u16, public_key: &[u8]) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_u8(NAMED_CURVE);
        writer.write_u16(group);
        writer.write_vector_u8(|point| point.write_bytes(public_key));
        writer.into_bytes()
    }

    /// Parses the body of a ServerKeyExchange message.
    pub fn decode(body: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(body);
        if reader.read_u8()? != NAMED_CURVE {
            return Err(CodecError::IllegalValue);
        }
        let group = reader.read_u16()?;
        let public_key = reader.read_vector_u8(1..=255)?.to_vec();
        let signed = DigitallySigned::read(&mut reader)?;
        reader.finish()?;
        Ok(Self { group, public_key, signed })
    }

    /// The whole message, header included.
    pub fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.write_bytes(&Self::params(self.group, &self.public_key));
        self.signed.write(&mut writer);
        handshake_message(handshake_type::SERVER_KEY_EXCHANGE, &writer.into_bytes())
    }
}

/// Parses the body of a TLS 1.2 ECDHE ClientKeyExchange: the client's public key.
pub fn decode_ecdhe_client_key_exchange(body: &[u8]) -> Result<Vec<u8>, CodecError> {
    let mut reader = Reader::new(body);
    let public_key = reader.read_vector_u8(1..=255)?.to_vec();
    reader.finish()?;
    Ok(public_key)
}

/// The whole TLS 1.2 ECDHE ClientKeyExchange message.
pub fn ecdhe_client_key_exchange_message(public_key: &[u8]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.write_vector_u8(|point| point.write_bytes(public_key));
    handshake_message(handshake_type::CLIENT_KEY_EXCHANGE, &writer.into_bytes())
}

/// The synthetic `message_hash` message that stands for ClientHello1 in the transcript after a
/// HelloRetryRequest.
pub fn message_hash_message(client_hello_hash: &[u8]) -> Vec<u8> {
    handshake_message(handshake_type::MESSAGE_HASH, client_hello_hash)
}

/// Reassembles handshake messages from the fragments of handshake records.
#[derive(Debug, Default)]
pub struct HandshakeBuffer {
    pending: Vec<u8>,
}

impl HandshakeBuffer {
    /// An empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a partial message is waiting for more records; a key change must not happen then.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Adds the fragment of one handshake record (at most 2^14 bytes); call
    /// [`HandshakeBuffer::next_message`] until it returns `None` before pushing again.
    pub fn push(&mut self, fragment: &[u8]) -> Result<(), CodecError> {
        self.pending.extend_from_slice(fragment);
        match self.declared_length() {
            Some(length) if length > MAX_HANDSHAKE_MESSAGE_BYTES => Err(CodecError::LengthOutOfRange),
            _ => Ok(()),
        }
    }

    fn declared_length(&self) -> Option<usize> {
        match self.pending.get(..HANDSHAKE_HEADER_BYTES) {
            Some(&[_, high, middle, low]) => {
                usize::try_from(u32::from_be_bytes([0, high, middle, low])).ok()
            }
            _ => None,
        }
    }

    /// The next whole message (type and the message bytes with their header), if one is
    /// complete.
    pub fn next_message(&mut self) -> Option<(u8, Vec<u8>)> {
        let length = self.declared_length()?;
        let total = HANDSHAKE_HEADER_BYTES + length;
        if self.pending.len() < total {
            return None;
        }
        let message: Vec<u8> = self.pending.drain(..total).collect();
        Some((message[0], message))
    }
}

/// The body of a whole handshake message (the bytes after its header).
pub fn message_body(message: &[u8]) -> &[u8] {
    message.get(HANDSHAKE_HEADER_BYTES..).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_client_hello() -> ClientHello {
        ClientHello {
            legacy_version: 0x0303,
            random: [7; 32],
            legacy_session_id: vec![1; 32],
            cipher_suites: vec![0x1301, 0x1303],
            compression_methods: vec![0],
            extensions: vec![
                Extension::server_name("db.example.com"),
                Extension::client_supported_versions(&[0x0304, 0x0303]),
                Extension::supported_groups(&[named_group::X25519, named_group::SECP256R1]),
                Extension::signature_algorithms(&[signature_scheme::ECDSA_SECP256R1_SHA256]),
                Extension::client_key_shares(&[KeyShareEntry { group: named_group::X25519, key_exchange: vec![9; 32] }]),
                Extension::application_protocols(&[b"postgresql"]),
                Extension::psk_key_exchange_modes(&[1]),
            ],
        }
    }

    #[test]
    fn a_client_hello_round_trips_with_its_extensions() {
        let hello = sample_client_hello();
        let message = hello.encode();
        assert_eq!(message[0], handshake_type::CLIENT_HELLO);
        let decoded = ClientHello::decode(message_body(&message)).unwrap();
        assert_eq!(decoded, hello);
        assert_eq!(decoded.server_name(), Ok(Some("db.example.com".to_owned())));
        assert_eq!(decoded.supported_versions(), Ok(vec![0x0304, 0x0303]));
        assert_eq!(decoded.supported_groups(), Ok(vec![0x001d, 0x0017]));
        assert_eq!(decoded.key_shares().map(|shares| shares.len()), Ok(1));
        assert_eq!(decoded.application_protocols(), Ok(vec![b"postgresql".to_vec()]));
        assert_eq!(decoded.psk_key_exchange_modes(), Ok(vec![1]));
    }

    #[test]
    fn duplicate_extensions_and_key_shares_are_refused() {
        let mut hello = sample_client_hello();
        hello.extensions.push(Extension::server_name("again.example.com"));
        assert_eq!(ClientHello::decode(message_body(&hello.encode())), Err(CodecError::IllegalValue));
        let mut hello = sample_client_hello();
        let share = KeyShareEntry { group: named_group::X25519, key_exchange: vec![1; 32] };
        hello.extensions[4] = Extension::client_key_shares(&[share.clone(), share]);
        assert_eq!(hello.key_shares(), Err(CodecError::IllegalValue));
    }

    #[test]
    fn a_tls12_client_hello_without_extensions_parses() {
        let mut hello = sample_client_hello();
        hello.extensions.clear();
        let mut body = message_body(&hello.encode()).to_vec();
        body.truncate(body.len() - 2);
        let decoded = ClientHello::decode(&body).unwrap();
        assert!(decoded.extensions.is_empty());
        assert_eq!(decoded.supported_versions(), Ok(Vec::new()));
    }

    #[test]
    fn every_truncation_of_a_client_hello_is_an_error() {
        let message = sample_client_hello().encode();
        let body = message_body(&message);
        for cut in 0..body.len() {
            assert!(ClientHello::decode(&body[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn server_hello_and_retry_request_are_told_apart() {
        let hello = ServerHello {
            legacy_version: 0x0303,
            random: HELLO_RETRY_REQUEST_RANDOM,
            legacy_session_id_echo: vec![1; 32],
            cipher_suite: 0x1301,
            compression_method: 0,
            extensions: vec![Extension::server_supported_version(0x0304), Extension::retry_key_share(named_group::SECP256R1)],
        };
        let decoded = ServerHello::decode(message_body(&hello.encode())).unwrap();
        assert!(decoded.is_hello_retry_request());
        assert_eq!(decoded.selected_version(), Ok(Some(0x0304)));
        assert_eq!(decoded.retry_group(), Ok(Some(named_group::SECP256R1)));
    }

    #[test]
    fn certificate_messages_round_trip_in_both_versions() {
        let chain: [&[u8]; 2] = [b"leaf", b"intermediate"];
        let message = CertificateMessage::from_chain(&chain);
        let tls13 = message.encode_tls13();
        assert_eq!(CertificateMessage::decode_tls13(message_body(&tls13)), Ok(message.clone()));
        let tls12 = message.encode_tls12();
        assert_eq!(CertificateMessage::decode_tls12(message_body(&tls12)), Ok(message));
    }

    #[test]
    fn handshake_messages_reassemble_across_fragments() {
        let first = finished_message(&[1; 32]);
        let second = key_update_message(true);
        let mut stream = first.clone();
        stream.extend_from_slice(&second);
        let mut buffer = HandshakeBuffer::new();
        for byte in &stream[..10] {
            buffer.push(&[*byte]).unwrap();
        }
        assert_eq!(buffer.next_message(), None);
        buffer.push(&stream[10..]).unwrap();
        assert_eq!(buffer.next_message(), Some((handshake_type::FINISHED, first)));
        let (message_type, message) = buffer.next_message().unwrap();
        assert_eq!(message_type, handshake_type::KEY_UPDATE);
        assert_eq!(decode_key_update(message_body(&message)), Ok(true));
        assert!(buffer.is_empty());
    }

    #[test]
    fn an_oversized_handshake_message_is_refused_from_its_header() {
        let mut buffer = HandshakeBuffer::new();
        assert_eq!(buffer.push(&[11, 0xff, 0xff, 0xff]), Err(CodecError::LengthOutOfRange));
    }

    #[test]
    fn ecdhe_key_exchange_messages_round_trip() {
        let exchange = EcdheServerKeyExchange {
            group: named_group::X25519,
            public_key: vec![4; 32],
            signed: DigitallySigned { scheme: signature_scheme::RSA_PSS_RSAE_SHA256, signature: vec![5; 256] },
        };
        let message = exchange.encode();
        assert_eq!(EcdheServerKeyExchange::decode(message_body(&message)), Ok(exchange));
        let client = ecdhe_client_key_exchange_message(&[6; 65]);
        assert_eq!(decode_ecdhe_client_key_exchange(message_body(&client)), Ok(vec![6; 65]));
    }
}
