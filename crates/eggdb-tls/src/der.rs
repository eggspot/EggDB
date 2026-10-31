//! DER, the Distinguished Encoding Rules of ASN.1 (ITU-T X.690), as X.509 certificates, private
//! keys and ECDSA signatures use them: a strict reader that refuses every encoding DER does not
//! allow (indefinite and non-minimal lengths, high tag numbers, non-minimal integers, bit
//! strings with unused bits), and a writer that only produces DER.

/// The universal and context-specific tags used by certificates and keys.
pub mod tag {
    /// `BOOLEAN`.
    pub const BOOLEAN: u8 = 0x01;
    /// `INTEGER`.
    pub const INTEGER: u8 = 0x02;
    /// `BIT STRING`.
    pub const BIT_STRING: u8 = 0x03;
    /// `OCTET STRING`.
    pub const OCTET_STRING: u8 = 0x04;
    /// `NULL`.
    pub const NULL: u8 = 0x05;
    /// `OBJECT IDENTIFIER`.
    pub const OBJECT_IDENTIFIER: u8 = 0x06;
    /// `UTF8String`.
    pub const UTF8_STRING: u8 = 0x0c;
    /// `PrintableString`.
    pub const PRINTABLE_STRING: u8 = 0x13;
    /// `TeletexString` (T61String), still found in old certificates.
    pub const TELETEX_STRING: u8 = 0x14;
    /// `IA5String` (ASCII).
    pub const IA5_STRING: u8 = 0x16;
    /// `UTCTime`.
    pub const UTC_TIME: u8 = 0x17;
    /// `GeneralizedTime`.
    pub const GENERALIZED_TIME: u8 = 0x18;
    /// `UniversalString` (UTF-32).
    pub const UNIVERSAL_STRING: u8 = 0x1c;
    /// `BMPString` (UTF-16 without surrogates).
    pub const BMP_STRING: u8 = 0x1e;
    /// `SEQUENCE` and `SEQUENCE OF`.
    pub const SEQUENCE: u8 = 0x30;
    /// `SET` and `SET OF`.
    pub const SET: u8 = 0x31;

    /// The tag `[number]` of a constructed context-specific field, such as an explicit
    /// `[0] EXPLICIT Version`.
    pub const fn context_constructed(number: u8) -> u8 {
        0xa0 | (number & 0x1f)
    }

    /// The tag `[number]` of a primitive context-specific field, such as an implicit
    /// `[2] IMPLICIT IA5String` in a subject alternative name.
    pub const fn context_primitive(number: u8) -> u8 {
        0x80 | (number & 0x1f)
    }
}

const HIGH_TAG_NUMBER: u8 = 0x1f;
const LONG_LENGTH: u8 = 0x80;
const MAX_LENGTH_BYTES: usize = 4;

/// Why DER input was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerError {
    /// The input ends inside an element.
    Truncated,
    /// A tag number of 31 or more, which certificates never use.
    HighTagNumber,
    /// An indefinite, non-minimal or over-long length.
    LengthEncoding,
    /// An element other than the one the structure requires here.
    UnexpectedTag {
        /// The tag the structure requires.
        expected: u8,
        /// The tag that was found, or `None` at the end of the input.
        found: Option<u8>,
    },
    /// Bytes after the last element of a structure.
    TrailingData,
    /// An integer that is empty, not minimally encoded, negative where it must not be, or too
    /// large.
    IntegerEncoding,
    /// A `BOOLEAN` that is not one byte of `0x00` or `0xff`, or a `NULL` with contents.
    PrimitiveEncoding,
    /// A `BIT STRING` that is empty or whose unused bits are not zero (or not allowed here).
    BitStringEncoding,
    /// An `OBJECT IDENTIFIER` that is empty or has a non-minimal or unfinished arc.
    ObjectIdentifierEncoding,
}

impl std::fmt::Display for DerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => f.write_str("the DER input ends inside an element"),
            Self::HighTagNumber => f.write_str("the DER input uses a tag number above 30"),
            Self::LengthEncoding => f.write_str("the DER input has an invalid length"),
            Self::UnexpectedTag {
                expected,
                found: Some(found),
            } => write!(f, "expected DER tag 0x{expected:02x}, found 0x{found:02x}"),
            Self::UnexpectedTag {
                expected,
                found: None,
            } => write!(f, "expected DER tag 0x{expected:02x}, found the end"),
            Self::TrailingData => f.write_str("the DER structure has trailing bytes"),
            Self::IntegerEncoding => f.write_str("the DER integer is not validly encoded"),
            Self::PrimitiveEncoding => f.write_str("the DER BOOLEAN or NULL is not valid"),
            Self::BitStringEncoding => f.write_str("the DER BIT STRING is not valid"),
            Self::ObjectIdentifierEncoding => {
                f.write_str("the DER OBJECT IDENTIFIER is not valid")
            }
        }
    }
}

impl std::error::Error for DerError {}

/// One element read from DER input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerElement<'a> {
    /// The tag byte.
    pub tag: u8,
    /// The contents, without the tag and length.
    pub contents: &'a [u8],
    /// The whole element: tag, length and contents, as a signature covers it.
    pub encoded: &'a [u8],
}

/// Reads DER elements one after another from a byte slice. After an error the input is not
/// valid DER, so the reader should be dropped.
#[derive(Debug, Clone, Copy)]
pub struct DerReader<'a> {
    input: &'a [u8],
}

impl<'a> DerReader<'a> {
    /// A reader over `input`.
    pub fn new(input: &'a [u8]) -> Self {
        Self { input }
    }

    /// Whether every element has been read.
    pub fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// The tag of the next element, without reading it.
    pub fn peek_tag(&self) -> Option<u8> {
        self.input.first().copied()
    }

    /// Fails with [`DerError::TrailingData`] unless every element has been read.
    pub fn finish(&self) -> Result<(), DerError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(DerError::TrailingData)
        }
    }

    /// Reads the next element, whatever its tag.
    pub fn read_element(&mut self) -> Result<DerElement<'a>, DerError> {
        let (&tag, rest) = self.input.split_first().ok_or(DerError::Truncated)?;
        if tag & HIGH_TAG_NUMBER == HIGH_TAG_NUMBER {
            return Err(DerError::HighTagNumber);
        }
        let (&first_length, rest) = rest.split_first().ok_or(DerError::Truncated)?;
        let (length, rest) = if first_length < LONG_LENGTH {
            (usize::from(first_length), rest)
        } else {
            let count = usize::from(first_length & !LONG_LENGTH);
            if count == 0 || count > MAX_LENGTH_BYTES {
                return Err(DerError::LengthEncoding);
            }
            let (length_bytes, rest) = rest
                .split_at_checked(count)
                .ok_or(DerError::Truncated)?;
            if length_bytes[0] == 0 {
                return Err(DerError::LengthEncoding);
            }
            let length = length_bytes
                .iter()
                .fold(0usize, |length, &byte| (length << 8) | usize::from(byte));
            if length < usize::from(LONG_LENGTH) {
                return Err(DerError::LengthEncoding);
            }
            (length, rest)
        };
        let contents = rest.get(..length).ok_or(DerError::Truncated)?;
        let header_length = self.input.len() - rest.len();
        let encoded = &self.input[..header_length + length];
        self.input = &self.input[header_length + length..];
        Ok(DerElement {
            tag,
            contents,
            encoded,
        })
    }

    /// Reads the next element, which must have tag `expected`, and returns it whole.
    pub fn read_tagged_element(&mut self, expected: u8) -> Result<DerElement<'a>, DerError> {
        match self.peek_tag() {
            Some(found) if found == expected => self.read_element(),
            found => Err(DerError::UnexpectedTag { expected, found }),
        }
    }

    /// Reads the next element, which must have tag `expected`, and returns its contents.
    pub fn read(&mut self, expected: u8) -> Result<&'a [u8], DerError> {
        Ok(self.read_tagged_element(expected)?.contents)
    }

    /// Reads the next element if it has tag `expected` (an `OPTIONAL` or `DEFAULT` field).
    pub fn read_optional(&mut self, expected: u8) -> Result<Option<&'a [u8]>, DerError> {
        if self.peek_tag() == Some(expected) {
            self.read(expected).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Reads a `SEQUENCE` and returns a reader over its elements.
    pub fn read_sequence(&mut self) -> Result<DerReader<'a>, DerError> {
        self.read(tag::SEQUENCE).map(DerReader::new)
    }

    /// Reads a constructed element with tag `expected` and returns a reader over its elements.
    pub fn read_constructed(&mut self, expected: u8) -> Result<DerReader<'a>, DerError> {
        self.read(expected).map(DerReader::new)
    }

    /// Reads a non-negative `INTEGER` and returns its big-endian magnitude without the sign
    /// byte (one `0x00` byte for zero).
    pub fn read_unsigned_integer(&mut self) -> Result<&'a [u8], DerError> {
        let contents = self.read(tag::INTEGER)?;
        unsigned_integer_magnitude(contents)
    }

    /// Reads a non-negative `INTEGER` that fits in 64 bits, such as a version number.
    pub fn read_small_unsigned_integer(&mut self) -> Result<u64, DerError> {
        let magnitude = self.read_unsigned_integer()?;
        if magnitude.len() > 8 {
            return Err(DerError::IntegerEncoding);
        }
        Ok(magnitude
            .iter()
            .fold(0u64, |value, &byte| (value << 8) | u64::from(byte)))
    }

    /// Reads a `BOOLEAN`.
    pub fn read_boolean(&mut self) -> Result<bool, DerError> {
        match self.read(tag::BOOLEAN)? {
            [0x00] => Ok(false),
            [0xff] => Ok(true),
            _ => Err(DerError::PrimitiveEncoding),
        }
    }

    /// Reads a `NULL`.
    pub fn read_null(&mut self) -> Result<(), DerError> {
        if self.read(tag::NULL)?.is_empty() {
            Ok(())
        } else {
            Err(DerError::PrimitiveEncoding)
        }
    }

    /// Reads an `OBJECT IDENTIFIER` and returns its encoded arcs, to compare with the
    /// constants in [`crate::oid`].
    pub fn read_object_identifier(&mut self) -> Result<&'a [u8], DerError> {
        let contents = self.read(tag::OBJECT_IDENTIFIER)?;
        validate_object_identifier(contents)?;
        Ok(contents)
    }

    /// Reads a `BIT STRING` of whole bytes (no unused bits), as keys and signatures are.
    pub fn read_bit_string(&mut self) -> Result<&'a [u8], DerError> {
        match self.read(tag::BIT_STRING)? {
            [0, bytes @ ..] => Ok(bytes),
            _ => Err(DerError::BitStringEncoding),
        }
    }

    /// Reads a `BIT STRING` of named flags, such as `KeyUsage`, and returns its bytes; the
    /// unused trailing bits must be zero.
    pub fn read_flag_bit_string(&mut self) -> Result<&'a [u8], DerError> {
        let contents = self.read(tag::BIT_STRING)?;
        match contents {
            [0] => Ok(&contents[1..]),
            [unused, .., last] if *unused < 8 && last & ((1u8 << unused) - 1) == 0 => {
                Ok(&contents[1..])
            }
            _ => Err(DerError::BitStringEncoding),
        }
    }

    /// Reads an `OCTET STRING`.
    pub fn read_octet_string(&mut self) -> Result<&'a [u8], DerError> {
        self.read(tag::OCTET_STRING)
    }
}

/// The big-endian magnitude of a non-negative DER `INTEGER`'s contents, without the sign byte.
pub fn unsigned_integer_magnitude(contents: &[u8]) -> Result<&[u8], DerError> {
    match contents {
        [] => Err(DerError::IntegerEncoding),
        [0x00] => Ok(contents),
        [0x00, next, ..] if next & 0x80 == 0 => Err(DerError::IntegerEncoding),
        [0x00, rest @ ..] => Ok(rest),
        [first, ..] if first & 0x80 != 0 => Err(DerError::IntegerEncoding),
        _ => Ok(contents),
    }
}

fn validate_object_identifier(contents: &[u8]) -> Result<(), DerError> {
    let mut arc_start = true;
    for &byte in contents {
        if arc_start && byte == 0x80 {
            return Err(DerError::ObjectIdentifierEncoding);
        }
        arc_start = byte & 0x80 == 0;
    }
    if contents.is_empty() || !arc_start {
        return Err(DerError::ObjectIdentifierEncoding);
    }
    Ok(())
}

/// Builds DER output. Constructed elements are written with a closure that fills their
/// contents, so their lengths are always right.
#[derive(Debug, Clone, Default)]
pub struct DerWriter {
    bytes: Vec<u8>,
}

impl DerWriter {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// The DER written so far.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Writes an element with `tag` and `contents`.
    pub fn write(&mut self, tag: u8, contents: &[u8]) {
        self.bytes.push(tag);
        self.write_length(contents.len());
        self.bytes.extend_from_slice(contents);
    }

    /// Copies an already encoded element (tag, length and contents) as it is.
    pub fn write_encoded(&mut self, encoded: &[u8]) {
        self.bytes.extend_from_slice(encoded);
    }

    fn write_length(&mut self, length: usize) {
        if length < usize::from(LONG_LENGTH) {
            self.bytes.push(length as u8);
            return;
        }
        let length_bytes = (length as u64).to_be_bytes();
        let first_nonzero = length_bytes
            .iter()
            .position(|&byte| byte != 0)
            .unwrap_or(length_bytes.len() - 1);
        let significant = &length_bytes[first_nonzero..];
        self.bytes.push(LONG_LENGTH | significant.len() as u8);
        self.bytes.extend_from_slice(significant);
    }

    /// Writes a constructed element with `tag` whose contents `build` writes.
    pub fn write_constructed(&mut self, tag: u8, build: impl FnOnce(&mut DerWriter)) {
        let mut inner = DerWriter::new();
        build(&mut inner);
        self.write(tag, &inner.bytes);
    }

    /// Writes a `SEQUENCE` whose elements `build` writes.
    pub fn write_sequence(&mut self, build: impl FnOnce(&mut DerWriter)) {
        self.write_constructed(tag::SEQUENCE, build);
    }

    /// Writes a non-negative `INTEGER` from its big-endian magnitude (leading zero bytes are
    /// dropped and a sign byte is added when needed).
    pub fn write_unsigned_integer(&mut self, magnitude: &[u8]) {
        let first_nonzero = magnitude.iter().position(|&byte| byte != 0);
        let significant = first_nonzero.map_or(&[][..], |start| &magnitude[start..]);
        let mut contents = Vec::with_capacity(significant.len() + 1);
        if significant.first().is_none_or(|&byte| byte & 0x80 != 0) {
            contents.push(0x00);
        }
        contents.extend_from_slice(significant);
        self.write(tag::INTEGER, &contents);
    }

    /// Writes a small non-negative `INTEGER`.
    pub fn write_small_unsigned_integer(&mut self, value: u64) {
        self.write_unsigned_integer(&value.to_be_bytes());
    }

    /// Writes a `BOOLEAN`.
    pub fn write_boolean(&mut self, value: bool) {
        self.write(tag::BOOLEAN, &[if value { 0xff } else { 0x00 }]);
    }

    /// Writes a `NULL`.
    pub fn write_null(&mut self) {
        self.write(tag::NULL, &[]);
    }

    /// Writes an `OBJECT IDENTIFIER` from its encoded arcs (a constant of [`crate::oid`]).
    pub fn write_object_identifier(&mut self, encoded_arcs: &[u8]) {
        self.write(tag::OBJECT_IDENTIFIER, encoded_arcs);
    }

    /// Writes a `BIT STRING` of whole bytes.
    pub fn write_bit_string(&mut self, bytes: &[u8]) {
        let mut contents = Vec::with_capacity(bytes.len() + 1);
        contents.push(0);
        contents.extend_from_slice(bytes);
        self.write(tag::BIT_STRING, &contents);
    }

    /// Writes an `OCTET STRING`.
    pub fn write_octet_string(&mut self, bytes: &[u8]) {
        self.write(tag::OCTET_STRING, bytes);
    }
}
