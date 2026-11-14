//! QPACK field compression (RFC 9204) without a dynamic table: this server announces a table
//! capacity of 0, so a field section uses the static table of appendix A and literals only,
//! and a reference to the dynamic table is an error.

use crate::QuicCodecError;

/// QPACK's static table (RFC 9204 appendix A).
pub const STATIC_TABLE: [(&str, &str); 99] = [
    (":authority", ""),
    (":path", "/"),
    ("age", "0"),
    ("content-disposition", ""),
    ("content-length", "0"),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("referer", ""),
    ("set-cookie", ""),
    (":method", "CONNECT"),
    (":method", "DELETE"),
    (":method", "GET"),
    (":method", "HEAD"),
    (":method", "OPTIONS"),
    (":method", "POST"),
    (":method", "PUT"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "103"),
    (":status", "200"),
    (":status", "304"),
    (":status", "404"),
    (":status", "503"),
    ("accept", "*/*"),
    ("accept", "application/dns-message"),
    ("accept-encoding", "gzip, deflate, br"),
    ("accept-ranges", "bytes"),
    ("access-control-allow-headers", "cache-control"),
    ("access-control-allow-headers", "content-type"),
    ("access-control-allow-origin", "*"),
    ("cache-control", "max-age=0"),
    ("cache-control", "max-age=2592000"),
    ("cache-control", "max-age=604800"),
    ("cache-control", "no-cache"),
    ("cache-control", "no-store"),
    ("cache-control", "public, max-age=31536000"),
    ("content-encoding", "br"),
    ("content-encoding", "gzip"),
    ("content-type", "application/dns-message"),
    ("content-type", "application/javascript"),
    ("content-type", "application/json"),
    ("content-type", "application/x-www-form-urlencoded"),
    ("content-type", "image/gif"),
    ("content-type", "image/jpeg"),
    ("content-type", "image/png"),
    ("content-type", "text/css"),
    ("content-type", "text/html; charset=utf-8"),
    ("content-type", "text/plain"),
    ("content-type", "text/plain;charset=utf-8"),
    ("range", "bytes=0-"),
    ("strict-transport-security", "max-age=31536000"),
    ("strict-transport-security", "max-age=31536000; includesubdomains"),
    ("strict-transport-security", "max-age=31536000; includesubdomains; preload"),
    ("vary", "accept-encoding"),
    ("vary", "origin"),
    ("x-content-type-options", "nosniff"),
    ("x-xss-protection", "1; mode=block"),
    (":status", "100"),
    (":status", "204"),
    (":status", "206"),
    (":status", "302"),
    (":status", "400"),
    (":status", "403"),
    (":status", "421"),
    (":status", "425"),
    (":status", "500"),
    ("accept-language", ""),
    ("access-control-allow-credentials", "FALSE"),
    ("access-control-allow-credentials", "TRUE"),
    ("access-control-allow-headers", "*"),
    ("access-control-allow-methods", "get"),
    ("access-control-allow-methods", "get, post, options"),
    ("access-control-allow-methods", "options"),
    ("access-control-expose-headers", "content-length"),
    ("access-control-request-headers", "content-type"),
    ("access-control-request-method", "get"),
    ("access-control-request-method", "post"),
    ("alt-svc", "clear"),
    ("authorization", ""),
    ("content-security-policy", "script-src 'none'; object-src 'none'; base-uri 'none'"),
    ("early-data", "1"),
    ("expect-ct", ""),
    ("forwarded", ""),
    ("if-range", ""),
    ("origin", ""),
    ("purpose", "prefetch"),
    ("server", ""),
    ("timing-allow-origin", "*"),
    ("upgrade-insecure-requests", "1"),
    ("user-agent", ""),
    ("x-forwarded-for", ""),
    ("x-frame-options", "deny"),
    ("x-frame-options", "sameorigin"),
];

const MOST_FIELD_SECTION_BYTES: usize = 64 * 1024;

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn byte(&self) -> Result<u8, QuicCodecError> {
        self.bytes.get(self.position).copied().ok_or(QuicCodecError::Truncated)
    }

    fn integer(&mut self, prefix_bits: u32) -> Result<u64, QuicCodecError> {
        let mask = (1u64 << prefix_bits) - 1;
        let mut value = u64::from(self.byte()?) & mask;
        self.position += 1;
        if value < mask {
            return Ok(value);
        }
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            self.position += 1;
            if shift > 56 {
                return Err(QuicCodecError::InvalidValue);
            }
            value = value.checked_add(u64::from(byte & 0x7f) << shift).ok_or(QuicCodecError::InvalidValue)?;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    }

    fn string(&mut self, prefix_bits: u32) -> Result<String, QuicCodecError> {
        let huffman = self.byte()? & (1 << prefix_bits) != 0;
        let length = usize::try_from(self.integer(prefix_bits)?).map_err(|_| QuicCodecError::InvalidValue)?;
        if length > MOST_FIELD_SECTION_BYTES {
            return Err(QuicCodecError::InvalidValue);
        }
        let end = self.position.checked_add(length).ok_or(QuicCodecError::Truncated)?;
        let raw = self.bytes.get(self.position..end).ok_or(QuicCodecError::Truncated)?;
        self.position = end;
        let bytes = if huffman {
            eggdb_http2::huffman_decode(raw).map_err(|_| QuicCodecError::InvalidValue)?
        } else {
            raw.to_vec()
        };
        String::from_utf8(bytes).map_err(|_| QuicCodecError::InvalidValue)
    }
}

fn static_entry(index: u64) -> Result<(&'static str, &'static str), QuicCodecError> {
    usize::try_from(index)
        .ok()
        .and_then(|index| STATIC_TABLE.get(index))
        .copied()
        .ok_or(QuicCodecError::InvalidValue)
}

/// Decodes a field section (the payload of an HTTP/3 HEADERS frame) into its fields, in order.
pub fn decode_field_section(bytes: &[u8]) -> Result<Vec<(String, String)>, QuicCodecError> {
    if bytes.len() > MOST_FIELD_SECTION_BYTES {
        return Err(QuicCodecError::InvalidValue);
    }
    let mut reader = Reader { bytes, position: 0 };
    let required_insert_count = reader.integer(8)?;
    reader.integer(7)?;
    if required_insert_count != 0 {
        return Err(QuicCodecError::InvalidValue);
    }
    let mut fields = Vec::new();
    while reader.position < bytes.len() {
        let first = reader.byte()?;
        if first & 0x80 != 0 {
            if first & 0x40 == 0 {
                return Err(QuicCodecError::InvalidValue);
            }
            let (name, value) = static_entry(reader.integer(6)?)?;
            fields.push((name.to_owned(), value.to_owned()));
        } else if first & 0x40 != 0 {
            if first & 0x10 == 0 {
                return Err(QuicCodecError::InvalidValue);
            }
            let (name, _) = static_entry(reader.integer(4)?)?;
            let value = reader.string(7)?;
            fields.push((name.to_owned(), value));
        } else if first & 0x20 != 0 {
            let name = reader.string(3)?;
            let value = reader.string(7)?;
            fields.push((name.to_ascii_lowercase(), value));
        } else {
            return Err(QuicCodecError::InvalidValue);
        }
    }
    Ok(fields)
}

fn write_integer(out: &mut Vec<u8>, first_bits: u8, prefix_bits: u32, value: u64) {
    let mask = (1u64 << prefix_bits) - 1;
    if value < mask {
        out.push(first_bits | value as u8);
        return;
    }
    out.push(first_bits | mask as u8);
    let mut rest = value - mask;
    while rest >= 0x80 {
        out.push((rest as u8 & 0x7f) | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

fn write_string(out: &mut Vec<u8>, first_bits: u8, prefix_bits: u32, text: &str) {
    let huffman = eggdb_http2::huffman_encode(text.as_bytes());
    if huffman.len() < text.len() {
        write_integer(out, first_bits | (1 << prefix_bits), prefix_bits, huffman.len() as u64);
        out.extend_from_slice(&huffman);
    } else {
        write_integer(out, first_bits, prefix_bits, text.len() as u64);
        out.extend_from_slice(text.as_bytes());
    }
}

/// Encodes `fields` into a field section using the static table only: an exact match as an
/// indexed line, a matching name as a literal with a name reference, anything else as a
/// literal with a literal name.
pub fn encode_field_section(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = vec![0x00, 0x00];
    for (name, value) in fields {
        if let Some(index) = STATIC_TABLE.iter().position(|entry| entry == &(*name, *value)) {
            write_integer(&mut out, 0xc0, 6, index as u64);
        } else if let Some(index) = STATIC_TABLE.iter().position(|entry| entry.0 == *name) {
            write_integer(&mut out, 0x50, 4, index as u64);
            write_string(&mut out, 0x00, 7, value);
        } else {
            write_string(&mut out, 0x20, 3, name);
            write_string(&mut out, 0x00, 7, value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_table_has_rfc_9204_s_landmarks() {
        assert_eq!(STATIC_TABLE[1], (":path", "/"));
        assert_eq!(STATIC_TABLE[17], (":method", "GET"));
        assert_eq!(STATIC_TABLE[23], (":scheme", "https"));
        assert_eq!(STATIC_TABLE[25], (":status", "200"));
        assert_eq!(STATIC_TABLE[46], ("content-type", "application/json"));
        assert_eq!(STATIC_TABLE[98], ("x-frame-options", "sameorigin"));
    }

    #[test]
    fn encoded_fields_decode_back_in_order() {
        let fields = [
            (":method", "GET"),
            (":path", "/_cluster/health"),
            (":scheme", "https"),
            (":authority", "db.example"),
            ("x-eggdb-trace", "on"),
            ("content-type", "application/json"),
        ];
        let decoded = decode_field_section(&encode_field_section(&fields)).unwrap();
        let expected: Vec<(String, String)> =
            fields.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn dynamic_table_references_and_bad_indices_are_refused() {
        assert_eq!(decode_field_section(&[0x01, 0x00]), Err(QuicCodecError::InvalidValue));
        assert_eq!(decode_field_section(&[0x00, 0x00, 0x80]), Err(QuicCodecError::InvalidValue));
        assert_eq!(decode_field_section(&[0x00, 0x00, 0xff, 0x40]), Err(QuicCodecError::InvalidValue));
        assert_eq!(decode_field_section(&[0x00, 0x00, 0x10]), Err(QuicCodecError::InvalidValue));
    }
}
