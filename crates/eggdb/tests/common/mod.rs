use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub struct TempPath {
    path: PathBuf,
}

impl TempPath {
    pub fn new(name: &str) -> Self {
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let file = format!("eggdb-test-{name}-{}-{number}.db", std::process::id());
        Self {
            path: std::env::temp_dir().join(file),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
