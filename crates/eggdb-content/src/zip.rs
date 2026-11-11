use crate::ContentError;
use crate::inflate::inflate;

const END_SIGNATURE: u32 = 0x0605_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const MAX_ENTRIES: usize = 65_535;

fn invalid(message: &str) -> ContentError {
    ContentError(format!("not a readable ZIP archive: {message}"))
}

fn u16_at(data: &[u8], at: usize) -> Result<u16, ContentError> {
    data.get(at..at + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .ok_or_else(|| invalid("a header ends early"))
}

fn u32_at(data: &[u8], at: usize) -> Result<u32, ContentError> {
    data.get(at..at + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or_else(|| invalid("a header ends early"))
}

const CRC32_POLYNOMIAL: u32 = 0xEDB8_8320;
const CRC32_TABLE: [u32; 256] = crc32_table();

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ CRC32_POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

/// The CRC-32 (ISO 3309, as ZIP and gzip use it) of `bytes`.
pub fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, &byte| {
        CRC32_TABLE[usize::from((crc as u8) ^ byte)] ^ (crc >> 8)
    })
}

const ZIP_VERSION: u16 = 20;
const UTF8_NAMES_FLAG: u16 = 0x0800;
const STORED_METHOD: u16 = 0;
const DOS_DATE_1980_01_01: u16 = 0x0021;

fn too_large() -> ContentError {
    ContentError("the archive would pass ZIP's 4 GiB or 65,535-entry limits".to_owned())
}

/// A ZIP archive (PKWARE's APPNOTE) holding `entries`, each a path and its bytes, stored
/// uncompressed with UTF-8 names and CRC-32 checks, in the order given.
pub fn write_stored(entries: &[(&str, &[u8])]) -> Result<Vec<u8>, ContentError> {
    let count = u16::try_from(entries.len()).map_err(|_| too_large())?;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = u32::try_from(out.len()).map_err(|_| too_large())?;
        let size = u32::try_from(data.len()).map_err(|_| too_large())?;
        let name_length = u16::try_from(name.len()).map_err(|_| too_large())?;
        let crc = crc32(data);
        let mut shared = Vec::with_capacity(26);
        for half in [ZIP_VERSION, UTF8_NAMES_FLAG, STORED_METHOD, 0, DOS_DATE_1980_01_01] {
            shared.extend_from_slice(&half.to_le_bytes());
        }
        for word in [crc, size, size] {
            shared.extend_from_slice(&word.to_le_bytes());
        }
        shared.extend_from_slice(&name_length.to_le_bytes());
        shared.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&LOCAL_SIGNATURE.to_le_bytes());
        out.extend_from_slice(&shared);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&CENTRAL_SIGNATURE.to_le_bytes());
        central.extend_from_slice(&ZIP_VERSION.to_le_bytes());
        central.extend_from_slice(&shared);
        for half in [0u16, 0, 0] {
            central.extend_from_slice(&half.to_le_bytes());
        }
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = u32::try_from(out.len()).map_err(|_| too_large())?;
    let central_size = u32::try_from(central.len()).map_err(|_| too_large())?;
    out.extend_from_slice(&central);
    out.extend_from_slice(&END_SIGNATURE.to_le_bytes());
    for half in [0u16, 0, count, count] {
        out.extend_from_slice(&half.to_le_bytes());
    }
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    Ok(out)
}

/// One file in a ZIP archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    /// Its path inside the archive.
    pub name: String,
    method: u16,
    flags: u16,
    crc: u32,
    compressed_size: usize,
    size: usize,
    local_offset: usize,
}

/// A ZIP archive (PKWARE's APPNOTE) read from memory: its entries, stored or deflated.
#[derive(Debug)]
pub struct ZipArchive<'a> {
    data: &'a [u8],
    /// The entries, in the central directory's order.
    pub entries: Vec<ZipEntry>,
}

impl<'a> ZipArchive<'a> {
    /// Reads the central directory; fails when `data` is not a ZIP archive, spans disks or needs
    /// ZIP64.
    pub fn new(data: &'a [u8]) -> Result<Self, ContentError> {
        let earliest = data.len().saturating_sub(22 + 65_535);
        let end = (earliest..=data.len().saturating_sub(22))
            .rev()
            .find(|at| u32_at(data, *at).ok() == Some(END_SIGNATURE))
            .ok_or_else(|| invalid("it has no end of central directory record"))?;
        if u16_at(data, end + 4)? != 0 || u16_at(data, end + 6)? != 0 {
            return Err(invalid("it spans several disks"));
        }
        let count = usize::from(u16_at(data, end + 10)?);
        let mut at = u32_at(data, end + 16)? as usize;
        if at == 0xFFFF_FFFF || count == 0xFFFF {
            return Err(invalid("ZIP64 archives are not read yet"));
        }
        let mut entries = Vec::with_capacity(count.min(MAX_ENTRIES));
        for _ in 0..count {
            if u32_at(data, at)? != CENTRAL_SIGNATURE {
                return Err(invalid("a central directory entry is missing"));
            }
            let name_length = usize::from(u16_at(data, at + 28)?);
            let extra_length = usize::from(u16_at(data, at + 30)?);
            let comment_length = usize::from(u16_at(data, at + 32)?);
            let name = data
                .get(at + 46..at + 46 + name_length)
                .ok_or_else(|| invalid("a name ends early"))?;
            entries.push(ZipEntry {
                name: String::from_utf8_lossy(name).into_owned(),
                flags: u16_at(data, at + 8)?,
                method: u16_at(data, at + 10)?,
                crc: u32_at(data, at + 16)?,
                compressed_size: u32_at(data, at + 20)? as usize,
                size: u32_at(data, at + 24)? as usize,
                local_offset: u32_at(data, at + 42)? as usize,
            });
            at += 46 + name_length + extra_length + comment_length;
        }
        Ok(Self { data, entries })
    }

    /// The contents of the entry named `name`, or `None` when there is none; refuses encrypted
    /// entries, methods other than stored and deflate, a wrong CRC and more than `limit` bytes.
    pub fn read(&self, name: &str, limit: usize) -> Result<Option<Vec<u8>>, ContentError> {
        let Some(entry) = self.entries.iter().find(|entry| entry.name == name) else {
            return Ok(None);
        };
        if entry.flags & 1 == 1 {
            return Err(invalid(&format!("{name} is encrypted")));
        }
        if entry.size > limit {
            return Err(ContentError(format!("{name} is larger than {limit} bytes")));
        }
        let local = entry.local_offset;
        if u32_at(self.data, local)? != LOCAL_SIGNATURE {
            return Err(invalid(&format!("{name}'s local header is missing")));
        }
        let start = local
            + 30
            + usize::from(u16_at(self.data, local + 26)?)
            + usize::from(u16_at(self.data, local + 28)?);
        let compressed = self
            .data
            .get(start..start.saturating_add(entry.compressed_size))
            .ok_or_else(|| invalid(&format!("{name}'s data ends early")))?;
        let contents = match entry.method {
            0 => compressed.to_vec(),
            8 => inflate(compressed, limit)?,
            method => {
                return Err(invalid(&format!(
                    "{name} uses compression method {method}, not stored or deflate"
                )));
            }
        };
        if contents.len() != entry.size || crc32(&contents) != entry.crc {
            return Err(invalid(&format!("{name} fails its CRC-32 or size check")));
        }
        Ok(Some(contents))
    }
}
