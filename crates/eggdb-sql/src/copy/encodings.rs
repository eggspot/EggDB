use super::{CopyChannel, CopyStart};
use crate::error::{SqlError, SqlResult, SqlState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CopyEncoding {
    #[default]
    Utf8,
    Latin1,
    Latin9,
    Win1252,
    Utf16,
    Utf16Le,
    Utf16Be,
}

pub(crate) enum EncodingName {
    Supported(CopyEncoding),
    NotSupported,
    Unknown,
}

const POSTGRESQL_ONLY_ENCODINGS: &[&str] = &[
    "abc", "alt", "big5", "cp1250", "cp1251", "cp1252", "cp1253", "cp1254", "cp1255", "cp1256",
    "cp1257", "cp1258", "cp866", "cp874", "cp932", "cp936", "cp949", "cp950", "euccn",
    "eucjis2004", "eucjp", "euckr", "euctw", "gb18030", "gbk", "iso88592", "iso88593",
    "iso88594", "iso88595", "iso88596", "iso88597", "iso88598", "iso88599", "iso885910",
    "iso885913", "iso885914", "iso885916", "johab", "koi8", "koi8r", "koi8u", "latin10",
    "latin2", "latin3", "latin4", "latin5", "latin6", "latin7", "latin8", "mskanji",
    "muleinternal", "shiftjis", "shiftjis2004", "sjis", "tcvn", "tcvn5712", "uhc", "vscii",
    "win", "win1250", "win1251", "win1253", "win1254", "win1255", "win1256", "win1257",
    "win1258", "win866", "win874", "win932", "win936", "win949", "win950", "windows1250",
    "windows1251", "windows1253", "windows1254", "windows1255", "windows1256", "windows1257",
    "windows1258", "windows866", "windows874", "windows932", "windows936", "windows949",
    "windows950",
];

const WIN1252_HIGH: [u16; 32] = [
    0x20AC, 0, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0, 0x017D, 0, 0, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC,
    0x2122, 0x0161, 0x203A, 0x0153, 0, 0x017E, 0x0178,
];

const LATIN9_CHANGES: [(u8, u16); 8] = [
    (0xA4, 0x20AC),
    (0xA6, 0x0160),
    (0xA8, 0x0161),
    (0xB4, 0x017D),
    (0xB8, 0x017E),
    (0xBC, 0x0152),
    (0xBD, 0x0153),
    (0xBE, 0x0178),
];

const UTF16_BYTE_ORDER_MARK: char = '\u{FEFF}';

fn normalized(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

impl CopyEncoding {
    pub(crate) fn named(name: &str) -> EncodingName {
        let normalized = normalized(name);
        let encoding = match normalized.as_str() {
            "utf8" | "unicode" | "sqlascii" => Self::Utf8,
            "latin1" | "iso88591" => Self::Latin1,
            "latin9" | "iso885915" => Self::Latin9,
            "win1252" | "windows1252" => Self::Win1252,
            "utf16" => Self::Utf16,
            "utf16le" => Self::Utf16Le,
            "utf16be" => Self::Utf16Be,
            other if POSTGRESQL_ONLY_ENCODINGS.contains(&other) => {
                return EncodingName::NotSupported;
            }
            _ => return EncodingName::Unknown,
        };
        EncodingName::Supported(encoding)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF8",
            Self::Latin1 => "LATIN1",
            Self::Latin9 => "LATIN9",
            Self::Win1252 => "WIN1252",
            Self::Utf16 => "UTF16",
            Self::Utf16Le => "UTF16LE",
            Self::Utf16Be => "UTF16BE",
        }
    }

    fn is_utf16(self) -> bool {
        matches!(self, Self::Utf16 | Self::Utf16Le | Self::Utf16Be)
    }

    fn decoded_byte(self, byte: u8) -> Option<char> {
        let code = match (self, byte) {
            (Self::Win1252, 0x80..=0x9F) => WIN1252_HIGH[usize::from(byte - 0x80)],
            (Self::Latin9, _) => LATIN9_CHANGES
                .iter()
                .find(|(changed, _)| *changed == byte)
                .map_or(u16::from(byte), |(_, code)| *code),
            _ => u16::from(byte),
        };
        if code == 0 && byte != 0 {
            return None;
        }
        char::from_u32(u32::from(code))
    }

    fn encoded_char(self, character: char) -> Option<u8> {
        let code = u32::from(character);
        let high = |table: &[u16]| {
            table
                .iter()
                .position(|&entry| entry != 0 && u32::from(entry) == code)
        };
        match self {
            Self::Win1252 => match code {
                0x80..=0x9F => None,
                0..=0xFF => u8::try_from(code).ok(),
                _ => high(&WIN1252_HIGH).and_then(|at| u8::try_from(0x80 + at).ok()),
            },
            Self::Latin9 => {
                if let Some((byte, _)) = LATIN9_CHANGES
                    .iter()
                    .find(|(_, changed)| u32::from(*changed) == code)
                {
                    return Some(*byte);
                }
                let byte = u8::try_from(code).ok()?;
                (!LATIN9_CHANGES.iter().any(|(changed, _)| *changed == byte)).then_some(byte)
            }
            _ => u8::try_from(code).ok(),
        }
    }
}

fn shown_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("0x{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn invalid_sequence(encoding: CopyEncoding, bytes: &[u8]) -> SqlError {
    SqlError::new(
        SqlState::CHARACTER_NOT_IN_REPERTOIRE,
        format!(
            "invalid byte sequence for encoding \"{}\": {}",
            encoding.name(),
            shown_bytes(bytes)
        ),
    )
}

fn no_equivalent(bytes: &[u8], from: &str, to: &str) -> SqlError {
    SqlError::new(
        SqlState::UNTRANSLATABLE_CHARACTER,
        format!(
            "character with byte sequence {} in encoding \"{from}\" has no equivalent in encoding \"{to}\"",
            shown_bytes(bytes)
        ),
    )
}

pub(crate) struct Transcoded<'c> {
    inner: &'c mut dyn CopyChannel,
    encoding: CopyEncoding,
    pending: Vec<u8>,
    little_endian: Option<bool>,
    input_ended: bool,
    written: Vec<u8>,
    wrote_any: bool,
}

impl<'c> Transcoded<'c> {
    pub(crate) fn new(inner: &'c mut dyn CopyChannel, encoding: CopyEncoding) -> Self {
        Self {
            inner,
            encoding,
            pending: Vec::new(),
            little_endian: match encoding {
                CopyEncoding::Utf16Be => Some(false),
                CopyEncoding::Utf16Le => Some(true),
                _ => None,
            },
            input_ended: false,
            written: Vec::new(),
            wrote_any: false,
        }
    }

    fn decode_pending(&mut self, data: &mut Vec<u8>, ended: bool) -> SqlResult<()> {
        if self.encoding.is_utf16() {
            return self.decode_utf16(data, ended);
        }
        let mut buffer = [0u8; 4];
        for &byte in &self.pending {
            if byte == 0 {
                return Err(invalid_sequence(self.encoding, &[byte]));
            }
            let character = self
                .encoding
                .decoded_byte(byte)
                .ok_or_else(|| no_equivalent(&[byte], self.encoding.name(), "UTF8"))?;
            data.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        }
        self.pending.clear();
        Ok(())
    }

    fn unit_at(&self, at: usize, little_endian: bool) -> Option<u16> {
        let pair = [*self.pending.get(at)?, *self.pending.get(at + 1)?];
        Some(if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        })
    }

    fn decode_utf16(&mut self, data: &mut Vec<u8>, ended: bool) -> SqlResult<()> {
        let mut at = 0;
        let little_endian = match self.little_endian {
            Some(order) => order,
            None => {
                if self.pending.len() < 2 && !ended {
                    return Ok(());
                }
                let order = match self.pending.get(..2) {
                    Some([0xFE, 0xFF]) => {
                        at = 2;
                        false
                    }
                    Some([0xFF, 0xFE]) => {
                        at = 2;
                        true
                    }
                    _ => true,
                };
                self.little_endian = Some(order);
                order
            }
        };
        let mut buffer = [0u8; 4];
        while let Some(unit) = self.unit_at(at, little_endian) {
            let (character, width) = match unit {
                0 => return Err(invalid_sequence(self.encoding, &self.pending[at..at + 2])),
                0xD800..=0xDBFF => match self.unit_at(at + 2, little_endian) {
                    None if !ended => break,
                    Some(low @ 0xDC00..=0xDFFF) => {
                        let code = 0x10000
                            + ((u32::from(unit) - 0xD800) << 10)
                            + (u32::from(low) - 0xDC00);
                        (char::from_u32(code), 4)
                    }
                    _ => (None, 2),
                },
                0xDC00..=0xDFFF => (None, 2),
                unit => (char::from_u32(u32::from(unit)), 2),
            };
            let Some(character) = character else {
                let end = (at + 4).min(self.pending.len());
                return Err(invalid_sequence(self.encoding, &self.pending[at..end]));
            };
            at += width;
            data.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
        }
        self.pending.drain(..at);
        if ended && !self.pending.is_empty() {
            return Err(invalid_sequence(self.encoding, &self.pending));
        }
        Ok(())
    }

    fn encode(&mut self, data: &[u8]) -> SqlResult<()> {
        let text = std::str::from_utf8(data).map_err(|_| {
            SqlError::new(
                SqlState::INTERNAL_ERROR,
                "a row of COPY data was not UTF-8 before its encoding",
            )
        })?;
        self.written.clear();
        if self.encoding == CopyEncoding::Utf16 && !self.wrote_any {
            self.push_utf16(UTF16_BYTE_ORDER_MARK);
        }
        self.wrote_any = true;
        for character in text.chars() {
            if self.encoding.is_utf16() {
                self.push_utf16(character);
                continue;
            }
            match self.encoding.encoded_char(character) {
                Some(byte) => self.written.push(byte),
                None => {
                    let mut buffer = [0u8; 4];
                    let bytes = character.encode_utf8(&mut buffer).as_bytes();
                    return Err(no_equivalent(bytes, "UTF8", self.encoding.name()));
                }
            }
        }
        Ok(())
    }

    fn push_utf16(&mut self, character: char) {
        let mut units = [0u16; 2];
        let big_endian = self.encoding == CopyEncoding::Utf16Be;
        for unit in character.encode_utf16(&mut units) {
            let bytes = if big_endian {
                unit.to_be_bytes()
            } else {
                unit.to_le_bytes()
            };
            self.written.extend_from_slice(&bytes);
        }
    }
}

impl CopyChannel for Transcoded<'_> {
    fn start_copy_in(&mut self, start: &CopyStart) -> SqlResult<()> {
        self.inner.start_copy_in(start)
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        let before = data.len();
        loop {
            if self.input_ended {
                return Ok(false);
            }
            if !self.inner.read_copy_data(&mut self.pending)? {
                self.input_ended = true;
                self.decode_pending(data, true)?;
                return Ok(data.len() > before);
            }
            self.decode_pending(data, false)?;
            if data.len() > before {
                return Ok(true);
            }
        }
    }

    fn abandon_copy_in(&mut self) {
        self.inner.abandon_copy_in();
    }

    fn start_copy_out(&mut self, start: &CopyStart) -> SqlResult<()> {
        self.inner.start_copy_out(start)
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        self.encode(data)?;
        self.inner.write_copy_data(&self.written)
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        self.inner.finish_copy_out()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Pieces {
        pieces: Vec<Vec<u8>>,
        written: Vec<u8>,
    }

    impl CopyChannel for Pieces {
        fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
            Ok(())
        }

        fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
            if self.pieces.is_empty() {
                return Ok(false);
            }
            data.extend_from_slice(&self.pieces.remove(0));
            Ok(true)
        }

        fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
            Ok(())
        }

        fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
            self.written.extend_from_slice(data);
            Ok(())
        }

        fn finish_copy_out(&mut self) -> SqlResult<()> {
            Ok(())
        }
    }

    fn decoded(encoding: CopyEncoding, bytes: &[u8], piece: usize) -> SqlResult<String> {
        let mut inner = Pieces {
            pieces: bytes.chunks(piece.max(1)).map(<[u8]>::to_vec).collect(),
            ..Pieces::default()
        };
        let mut channel = Transcoded::new(&mut inner, encoding);
        let mut data = Vec::new();
        while channel.read_copy_data(&mut data)? {}
        Ok(String::from_utf8(data).unwrap())
    }

    fn encoded(encoding: CopyEncoding, rows: &[&str]) -> SqlResult<Vec<u8>> {
        let mut inner = Pieces::default();
        let mut channel = Transcoded::new(&mut inner, encoding);
        for row in rows {
            channel.write_copy_data(row.as_bytes())?;
        }
        Ok(inner.written)
    }

    #[test]
    fn single_byte_encodings_read_and_write_their_tables() {
        assert_eq!(
            decoded(CopyEncoding::Latin1, b"caf\xe9\t\x80\n", 1).unwrap(),
            "café\t\u{80}\n"
        );
        assert_eq!(
            decoded(CopyEncoding::Win1252, b"\x80 \x93q\x94 \x9f", 2).unwrap(),
            "€ \u{201C}q\u{201D} Ÿ"
        );
        assert_eq!(
            decoded(CopyEncoding::Latin9, b"\xa4\xbd\xe9", 1).unwrap(),
            "€œé"
        );
        let undefined = decoded(CopyEncoding::Win1252, b"a\x81", 1).unwrap_err();
        assert_eq!(undefined.state().code(), "22P05");
        assert_eq!(
            undefined.message(),
            "character with byte sequence 0x81 in encoding \"WIN1252\" has no equivalent in encoding \"UTF8\""
        );
        assert_eq!(
            decoded(CopyEncoding::Latin1, b"a\0b", 1)
                .unwrap_err()
                .state()
                .code(),
            "22021"
        );
        assert_eq!(
            encoded(CopyEncoding::Win1252, &["€ é\n"]).unwrap(),
            b"\x80 \xe9\n"
        );
        assert_eq!(encoded(CopyEncoding::Latin9, &["€\n"]).unwrap(), b"\xa4\n");
        let missing = encoded(CopyEncoding::Latin1, &["a€\n"]).unwrap_err();
        assert_eq!(missing.state().code(), "22P05");
        assert_eq!(
            missing.message(),
            "character with byte sequence 0xe2 0x82 0xac in encoding \"UTF8\" has no equivalent in encoding \"LATIN1\""
        );
        assert!(encoded(CopyEncoding::Latin9, &["\u{a4}"]).is_err());
        assert!(encoded(CopyEncoding::Win1252, &["\u{81}"]).is_err());
    }

    #[test]
    fn every_byte_of_a_single_byte_encoding_round_trips() {
        for encoding in [
            CopyEncoding::Latin1,
            CopyEncoding::Latin9,
            CopyEncoding::Win1252,
        ] {
            for byte in 1..=255u8 {
                if let Ok(text) = decoded(encoding, &[byte], 1) {
                    assert_eq!(encoded(encoding, &[&text]).unwrap(), [byte], "{byte}");
                }
            }
        }
    }

    #[test]
    fn utf16_reads_its_byte_order_mark_pairs_and_pieces_split_anywhere() {
        let text = "id\tnamé 😀\n";
        let little: Vec<u8> = std::iter::once(0xFEFFu16)
            .chain(text.encode_utf16())
            .flat_map(u16::to_le_bytes)
            .collect();
        let big: Vec<u8> = std::iter::once(0xFEFFu16)
            .chain(text.encode_utf16())
            .flat_map(u16::to_be_bytes)
            .collect();
        for piece in 1..6 {
            assert_eq!(decoded(CopyEncoding::Utf16, &little, piece).unwrap(), text);
            assert_eq!(decoded(CopyEncoding::Utf16, &big, piece).unwrap(), text);
            assert_eq!(
                decoded(CopyEncoding::Utf16Le, &little[2..], piece).unwrap(),
                text
            );
        }
        assert_eq!(encoded(CopyEncoding::Utf16, &[text]).unwrap(), little);
        assert_eq!(encoded(CopyEncoding::Utf16Be, &[text]).unwrap(), big[2..]);
        for broken in [&[0x3D, 0xD8, b'a', 0][..], &[0x00, 0xDC], &[b'a'], &[0, 0]] {
            assert_eq!(
                decoded(CopyEncoding::Utf16Le, broken, 1)
                    .unwrap_err()
                    .state()
                    .code(),
                "22021",
                "{broken:?}"
            );
        }
    }

    #[test]
    fn names_are_matched_as_postgresql_matches_them() {
        for (name, expected) in [
            ("UTF8", Some(CopyEncoding::Utf8)),
            ("utf-8", Some(CopyEncoding::Utf8)),
            ("Latin-1", Some(CopyEncoding::Latin1)),
            ("ISO_8859_1", Some(CopyEncoding::Latin1)),
            ("windows-1252", Some(CopyEncoding::Win1252)),
            ("LATIN9", Some(CopyEncoding::Latin9)),
            ("UTF-16LE", Some(CopyEncoding::Utf16Le)),
        ] {
            assert!(
                matches!(CopyEncoding::named(name), EncodingName::Supported(found) if Some(found) == expected),
                "{name}"
            );
        }
        assert!(matches!(
            CopyEncoding::named("EUC_JP"),
            EncodingName::NotSupported
        ));
        assert!(matches!(
            CopyEncoding::named("klingon"),
            EncodingName::Unknown
        ));
    }
}
