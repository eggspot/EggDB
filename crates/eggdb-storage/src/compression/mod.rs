pub(crate) mod lz4;
pub(crate) mod zstd;

/// Whether large values are compressed when written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueCompression {
    /// Values are stored as given.
    #[default]
    Off,
    /// A value of at least 4 KiB whose LZ4 block saves at least a sixteenth of it is stored
    /// compressed on overflow pages of layout 2, which sets the file's incompatible feature
    /// [`crate::Features::COMPRESSED_VALUES`].
    Lz4,
    /// A value of at least 4 KiB whose Zstandard frame (RFC 8878, our own codec) saves at least a
    /// sixteenth of it is stored compressed on overflow pages of layout 3, which sets the file's
    /// incompatible feature [`crate::Features::ZSTD_VALUES`]. Slower to write than
    /// [`ValueCompression::Lz4`], for cold data.
    Zstd,
}
