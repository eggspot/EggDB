use crate::frame::Http2Error;
use crate::huffman::huffman_decode;
use std::collections::VecDeque;

const STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

const ENTRY_OVERHEAD: usize = 32;
const DEFAULT_TABLE_SIZE: usize = 4096;
const MOST_HEADER_LIST_BYTES: usize = 256 * 1024;

/// Decodes header blocks, keeping the dynamic table between them (RFC 7541).
#[derive(Debug)]
pub struct HeaderDecoder {
    dynamic: VecDeque<(String, String)>,
    size: usize,
    max_size: usize,
}

impl Default for HeaderDecoder {
    fn default() -> Self {
        Self {
            dynamic: VecDeque::new(),
            size: 0,
            max_size: DEFAULT_TABLE_SIZE,
        }
    }
}

struct Input<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl Input<'_> {
    fn byte(&mut self) -> Result<u8, Http2Error> {
        let byte = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| Http2Error::compression("a header block ends early"))?;
        self.at += 1;
        Ok(byte)
    }

    fn integer(&mut self, first: u8, prefix_bits: u32) -> Result<usize, Http2Error> {
        let mask = (1u16 << prefix_bits) - 1;
        let mut value = usize::from(u16::from(first) & mask);
        if value < usize::from(mask) {
            return Ok(value);
        }
        let mut shift = 0u32;
        loop {
            let byte = self.byte()?;
            if shift > 28 {
                return Err(Http2Error::compression("an integer too large"));
            }
            value = value
                .checked_add(usize::from(byte & 0x7F) << shift)
                .ok_or_else(|| Http2Error::compression("an integer too large"))?;
            shift += 7;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    }

    fn string(&mut self) -> Result<String, Http2Error> {
        let first = self.byte()?;
        let length = self.integer(first, 7)?;
        let end = self
            .at
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| Http2Error::compression("a header string runs past its block"))?;
        let raw = &self.bytes[self.at..end];
        self.at = end;
        let bytes = if first & 0x80 != 0 { huffman_decode(raw)? } else { raw.to_vec() };
        String::from_utf8(bytes).map_err(|_| Http2Error::compression("a header that is not UTF-8"))
    }
}

impl HeaderDecoder {
    fn entry(&self, index: usize) -> Result<(String, String), Http2Error> {
        if index == 0 {
            return Err(Http2Error::compression("header index 0"));
        }
        if let Some((name, value)) = STATIC_TABLE.get(index - 1) {
            return Ok(((*name).to_owned(), (*value).to_owned()));
        }
        self.dynamic
            .get(index - 1 - STATIC_TABLE.len())
            .cloned()
            .ok_or_else(|| Http2Error::compression(format!("header index {index} is not in the table")))
    }

    fn evict_to(&mut self, size: usize) {
        while self.size > size {
            match self.dynamic.pop_back() {
                Some((name, value)) => self.size -= name.len() + value.len() + ENTRY_OVERHEAD,
                None => break,
            }
        }
    }

    fn insert(&mut self, name: String, value: String) {
        let entry_size = name.len() + value.len() + ENTRY_OVERHEAD;
        if entry_size > self.max_size {
            self.dynamic.clear();
            self.size = 0;
            return;
        }
        self.evict_to(self.max_size - entry_size);
        self.size += entry_size;
        self.dynamic.push_front((name, value));
    }

    /// The header fields of one header block, in order.
    pub fn decode(&mut self, block: &[u8]) -> Result<Vec<(String, String)>, Http2Error> {
        let mut input = Input { bytes: block, at: 0 };
        let mut fields = Vec::new();
        let mut total = 0usize;
        while input.at < block.len() {
            let first = input.byte()?;
            let (name, value) = if first & 0x80 != 0 {
                let index = input.integer(first, 7)?;
                self.entry(index)?
            } else if first & 0x40 != 0 {
                let index = input.integer(first, 6)?;
                let name = if index == 0 { input.string()? } else { self.entry(index)?.0 };
                let value = input.string()?;
                self.insert(name.clone(), value.clone());
                (name, value)
            } else if first & 0x20 != 0 {
                let size = input.integer(first, 5)?;
                if size > DEFAULT_TABLE_SIZE {
                    return Err(Http2Error::compression("a table size above the one EggDB announced"));
                }
                self.max_size = size;
                self.evict_to(size);
                continue;
            } else {
                let index = input.integer(first, 4)?;
                let name = if index == 0 { input.string()? } else { self.entry(index)?.0 };
                (name, input.string()?)
            };
            total += name.len() + value.len() + ENTRY_OVERHEAD;
            if total > MOST_HEADER_LIST_BYTES {
                return Err(Http2Error::protocol("a header list larger than 256 KiB"));
            }
            fields.push((name, value));
        }
        Ok(fields)
    }
}

fn put_integer(out: &mut Vec<u8>, first_bits: u8, prefix_bits: u32, value: usize) {
    let limit = (1usize << prefix_bits) - 1;
    if value < limit {
        out.push(first_bits | value as u8);
        return;
    }
    out.push(first_bits | limit as u8);
    let mut rest = value - limit;
    while rest >= 0x80 {
        out.push((rest as u8 & 0x7F) | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

fn put_string(out: &mut Vec<u8>, text: &str) {
    put_integer(out, 0, 7, text.len());
    out.extend_from_slice(text.as_bytes());
}

/// Encodes header fields as literals without indexing, naming a static table entry where
/// the name is there, so the encoder keeps no state.
pub fn encode_headers(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        if let Some(exact) = STATIC_TABLE.iter().position(|entry| entry == &(*name, *value)) {
            put_integer(&mut out, 0x80, 7, exact + 1);
            continue;
        }
        match STATIC_TABLE.iter().position(|(known, _)| known == name) {
            Some(index) => put_integer(&mut out, 0, 4, index + 1),
            None => {
                out.push(0);
                put_string(&mut out, name);
            }
        }
        put_string(&mut out, value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_rfc_requests_with_huffman_and_the_dynamic_table() {
        let mut decoder = HeaderDecoder::default();
        let first = [
            0x82, 0x86, 0x84, 0x41, 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff,
        ];
        assert_eq!(
            decoder.decode(&first).unwrap(),
            [
                (":method".to_owned(), "GET".to_owned()),
                (":scheme".to_owned(), "http".to_owned()),
                (":path".to_owned(), "/".to_owned()),
                (":authority".to_owned(), "www.example.com".to_owned()),
            ]
        );
        let second = [0x82, 0x86, 0x84, 0xbe, 0x58, 0x86, 0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf];
        let fields = decoder.decode(&second).unwrap();
        assert_eq!(fields[3], (":authority".to_owned(), "www.example.com".to_owned()));
        assert_eq!(fields[4], ("cache-control".to_owned(), "no-cache".to_owned()));
        let encoded = encode_headers(&[(":status", "200"), ("content-type", "application/grpc"), ("grpc-status", "0")]);
        let decoded = HeaderDecoder::default().decode(&encoded).unwrap();
        assert_eq!(decoded[1].1, "application/grpc");
        assert_eq!(decoded[2].0, "grpc-status");
        assert!(HeaderDecoder::default().decode(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x0f]).is_err());
    }
}
