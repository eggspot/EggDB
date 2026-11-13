//! OPC UA's binary encoding (IEC 62541-6) for the fields the client writes and reads.

use std::time::{SystemTime, UNIX_EPOCH};

const TICKS_FROM_1601_TO_1970: i64 = 116_444_736_000_000_000;
const MOST_ARRAY_ITEMS: i32 = 100_000;

/// A NodeId as OPC UA's binary encoding carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeId {
    /// A numeric identifier.
    Numeric(u16, u32),
    /// A string identifier.
    Text(u16, String),
    /// A GUID identifier, its 16 bytes as encoded.
    Guid(u16, [u8; 16]),
    /// An opaque identifier.
    Opaque(u16, Vec<u8>),
}

impl NodeId {
    /// The null NodeId (`i=0`).
    pub const NULL: NodeId = NodeId::Numeric(0, 0);

    /// Reads the text form OPC UA tools write: `i=85`, `ns=2;s=Temperature`, `ns=1;i=1001`,
    /// `ns=3;g=...` (a GUID) or `ns=4;b=...` (base64).
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (namespace, rest) = match text.strip_prefix("ns=").and_then(|rest| rest.split_once(';')) {
            Some((namespace, rest)) => (namespace.parse::<u16>().map_err(|_| format!("{text}: the namespace is not a number"))?, rest),
            None => (0, text),
        };
        let (kind, identifier) = rest.split_once('=').ok_or_else(|| format!("{text} is not a NodeId such as ns=2;s=Temperature"))?;
        Ok(match kind {
            "i" => NodeId::Numeric(namespace, identifier.parse().map_err(|_| format!("{text}: the identifier is not a number"))?),
            "s" => NodeId::Text(namespace, identifier.to_owned()),
            "g" => {
                let hex: String = identifier.chars().filter(|character| *character != '-').collect();
                let bytes: Vec<u8> = (0..hex.len() / 2).filter_map(|at| u8::from_str_radix(hex.get(at * 2..at * 2 + 2)?, 16).ok()).collect();
                let [a0, a1, a2, a3, b0, b1, c0, c1, rest @ ..] = bytes.as_slice() else {
                    return Err(format!("{text}: a GUID has 32 hex digits"));
                };
                if hex.len() != 32 || rest.len() != 8 {
                    return Err(format!("{text}: a GUID has 32 hex digits"));
                }
                let mut encoded = [0u8; 16];
                encoded[..8].copy_from_slice(&[*a3, *a2, *a1, *a0, *b1, *b0, *c1, *c0]);
                encoded[8..].copy_from_slice(rest);
                NodeId::Guid(namespace, encoded)
            }
            "b" => NodeId::Opaque(namespace, eggdb_crypto::base64_decode(identifier.as_bytes()).map_err(|_| format!("{text}: the identifier is not base64"))?),
            _ => return Err(format!("{text}: the identifier type must be i, s, g or b")),
        })
    }
}

/// Writes OPC UA binary values.
#[derive(Debug, Default)]
pub struct Writer {
    /// The bytes written so far.
    pub bytes: Vec<u8>,
}

impl Writer {
    /// Appends a Byte.
    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.bytes.push(value);
        self
    }

    /// Appends a UInt16.
    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Appends a UInt32.
    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Appends an Int32.
    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Appends a Double.
    pub fn f64(&mut self, value: f64) -> &mut Self {
        self.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// Appends a Boolean.
    pub fn boolean(&mut self, value: bool) -> &mut Self {
        self.u8(u8::from(value))
    }

    /// Appends a String, or a null one.
    pub fn string(&mut self, value: Option<&str>) -> &mut Self {
        self.byte_string(value.map(str::as_bytes))
    }

    /// Appends a ByteString, or a null one.
    pub fn byte_string(&mut self, value: Option<&[u8]>) -> &mut Self {
        match value {
            Some(bytes) => {
                self.i32(i32::try_from(bytes.len()).unwrap_or(i32::MAX));
                self.bytes.extend_from_slice(bytes);
            }
            None => {
                self.i32(-1);
            }
        }
        self
    }

    /// Appends the current time as a DateTime.
    pub fn now(&mut self) -> &mut Self {
        let ticks = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_nanos() / 100).unwrap_or(i64::MAX));
        self.bytes.extend_from_slice(&ticks.saturating_add(TICKS_FROM_1601_TO_1970).to_le_bytes());
        self
    }

    /// Appends a NodeId in its shortest encoding.
    pub fn node_id(&mut self, node: &NodeId) -> &mut Self {
        match node {
            NodeId::Numeric(0, id) if *id < 256 => self.u8(0x00).u8(*id as u8),
            NodeId::Numeric(namespace, id) if *namespace < 256 && *id < 65_536 => self.u8(0x01).u8(*namespace as u8).u16(*id as u16),
            NodeId::Numeric(namespace, id) => self.u8(0x02).u16(*namespace).u32(*id),
            NodeId::Text(namespace, text) => self.u8(0x03).u16(*namespace).string(Some(text)),
            NodeId::Guid(namespace, bytes) => {
                self.u8(0x04).u16(*namespace);
                self.bytes.extend_from_slice(bytes);
                self
            }
            NodeId::Opaque(namespace, bytes) => self.u8(0x05).u16(*namespace).byte_string(Some(bytes)),
        }
    }

    /// Appends an ExtensionObject holding a binary body of type `type_id`.
    pub fn extension_object(&mut self, type_id: u32, body: &[u8]) -> &mut Self {
        self.node_id(&NodeId::Numeric(0, type_id)).u8(0x01).byte_string(Some(body))
    }

    /// Appends an empty ExtensionObject.
    pub fn no_extension_object(&mut self) -> &mut Self {
        self.node_id(&NodeId::NULL).u8(0x00)
    }
}

/// Reads OPC UA binary values; every length is checked before it is used.
#[derive(Debug)]
pub struct Reader<'b> {
    /// The bytes read.
    pub bytes: &'b [u8],
    /// Where the next value starts.
    pub at: usize,
}

impl<'b> Reader<'b> {
    /// A reader at the start of `bytes`.
    pub fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Takes the next `count` bytes.
    pub fn take(&mut self, count: usize) -> Result<&'b [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or("an OPC UA message is cut short")?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// Reads a Byte.
    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.fixed::<1>()?[0])
    }

    /// Reads a UInt16.
    pub fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.fixed()?))
    }

    /// Reads a UInt32.
    pub fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.fixed()?))
    }

    /// Reads an Int32.
    pub fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.fixed()?))
    }

    /// Reads a Double.
    pub fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_le_bytes(self.fixed()?))
    }

    /// Reads a DateTime's ticks.
    pub fn date_time(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.fixed()?))
    }

    /// Reads a ByteString (`None` when null).
    pub fn byte_string(&mut self) -> Result<Option<&'b [u8]>, String> {
        let length = self.i32()?;
        if length < 0 {
            return Ok(None);
        }
        self.take(length as usize).map(Some)
    }

    /// Reads a String (`None` when null).
    pub fn string(&mut self) -> Result<Option<String>, String> {
        Ok(self.byte_string()?.map(|bytes| String::from_utf8_lossy(bytes).into_owned()))
    }

    /// Reads an array's length (0 when null), refusing more than 100,000 items.
    pub fn array_length(&mut self) -> Result<usize, String> {
        let length = self.i32()?;
        if length > MOST_ARRAY_ITEMS {
            return Err(format!("an array of {length} items is more than the client reads"));
        }
        Ok(length.max(0) as usize)
    }

    /// Reads a NodeId or ExpandedNodeId.
    pub fn node_id(&mut self) -> Result<NodeId, String> {
        let mask = self.u8()?;
        let node = match mask & 0x0f {
            0x00 => NodeId::Numeric(0, u32::from(self.u8()?)),
            0x01 => {
                let namespace = u16::from(self.u8()?);
                NodeId::Numeric(namespace, u32::from(self.u16()?))
            }
            0x02 => {
                let namespace = self.u16()?;
                NodeId::Numeric(namespace, self.u32()?)
            }
            0x03 => {
                let namespace = self.u16()?;
                NodeId::Text(namespace, self.string()?.unwrap_or_default())
            }
            0x04 => {
                let namespace = self.u16()?;
                NodeId::Guid(namespace, self.fixed()?)
            }
            0x05 => {
                let namespace = self.u16()?;
                NodeId::Opaque(namespace, self.byte_string()?.unwrap_or_default().to_vec())
            }
            other => return Err(format!("0x{other:02x} is not a NodeId encoding")),
        };
        if mask & 0x80 != 0 {
            self.string()?;
        }
        if mask & 0x40 != 0 {
            self.u32()?;
        }
        Ok(node)
    }

    /// Reads a LocalizedText's text.
    pub fn localized_text(&mut self) -> Result<Option<String>, String> {
        let mask = self.u8()?;
        if mask & 0x01 != 0 {
            self.string()?;
        }
        if mask & 0x02 != 0 {
            return self.string();
        }
        Ok(None)
    }

    /// Reads an ExtensionObject: its type and binary body (empty for none or XML).
    pub fn extension_object(&mut self) -> Result<(NodeId, &'b [u8]), String> {
        let type_id = self.node_id()?;
        let body = match self.u8()? {
            0x00 => &[][..],
            0x01 | 0x02 => self.byte_string()?.unwrap_or_default(),
            other => return Err(format!("0x{other:02x} is not an ExtensionObject encoding")),
        };
        Ok((type_id, body))
    }

    /// Skips a DiagnosticInfo.
    pub fn skip_diagnostic_info(&mut self, depth: usize) -> Result<(), String> {
        if depth > 8 {
            return Err("DiagnosticInfos nest too deeply".to_owned());
        }
        let mask = self.u8()?;
        for bit in [0x01, 0x02, 0x04, 0x08] {
            if mask & bit != 0 {
                self.i32()?;
            }
        }
        if mask & 0x10 != 0 {
            self.string()?;
        }
        if mask & 0x20 != 0 {
            self.u32()?;
        }
        if mask & 0x40 != 0 {
            self.skip_diagnostic_info(depth + 1)?;
        }
        Ok(())
    }

    /// Skips an array of DiagnosticInfos.
    pub fn skip_diagnostic_infos(&mut self) -> Result<(), String> {
        for _ in 0..self.array_length()? {
            self.skip_diagnostic_info(0)?;
        }
        Ok(())
    }

    /// Reads a ResponseHeader and returns its service result.
    pub fn response_header(&mut self) -> Result<u32, String> {
        self.date_time()?;
        self.u32()?;
        let result = self.u32()?;
        self.skip_diagnostic_info(0)?;
        for _ in 0..self.array_length()? {
            self.string()?;
        }
        self.extension_object()?;
        Ok(result)
    }

    /// Whether every byte has been read.
    pub fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}
