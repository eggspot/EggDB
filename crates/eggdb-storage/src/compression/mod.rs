pub(crate) mod lz4;

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
}
