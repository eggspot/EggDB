//! EggDB's content extraction for full-text search (D56): [`inflate`] decompresses DEFLATE
//! (RFC 1951), [`gunzip`] gzip (RFC 1952) and [`zlib_inflate`] the zlib format (RFC 1950) that
//! HTTP's `deflate` coding names (D343), [`ZipArchive`] reads ZIP archives (PKWARE's APPNOTE, stored and deflated
//! entries, CRC-checked), and [`extract_text`] returns the text of Office Open XML (ECMA-376:
//! `.docx`, `.xlsx`, `.pptx`) and OpenDocument (OASIS ODF: `.odt`, `.ods`, `.odp`) files. Every
//! reader bounds what it produces, so a hostile file cannot exhaust memory.

mod inflate;
mod text;
mod zip;

pub use inflate::{DecompressError, gunzip, inflate, zlib_inflate};
pub use zip::{ZipArchive, ZipEntry, crc32};

/// Why content could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentError(pub String);

impl std::fmt::Display for ContentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The text of an Office Open XML or OpenDocument file, one paragraph per line, decompressing
/// at most `limit` bytes over all its parts and refusing a text longer than `limit`.
pub fn extract_text(file: &[u8], limit: usize) -> Result<String, ContentError> {
    text::extract(&ZipArchive::new(file)?, limit)
}

#[cfg(test)]
mod tests;
