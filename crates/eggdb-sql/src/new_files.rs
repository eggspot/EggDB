/// Creates the files that statements such as SQLite's `VACUUM INTO` write. An embedded database
/// gives its sessions one with [`crate::Session::set_new_file_writer`]; a server gives none, so
/// its clients cannot write files on the server's machine.
pub trait NewFileWriter: Sync {
    /// Creates the file at `path` holding `bytes`, and fails when a file is already there.
    fn write_new_file(&self, path: &str, bytes: &[u8]) -> Result<(), String>;
}
