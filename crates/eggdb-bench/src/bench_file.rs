use std::path::PathBuf;

pub(crate) struct BenchFile {
    pub(crate) path: PathBuf,
}

impl BenchFile {
    pub(crate) fn new(name: &str) -> Result<Self, String> {
        let file = format!("eggdb-bench-{name}-{}.db", std::process::id());
        let bench_file = Self {
            path: std::env::temp_dir().join(file),
        };
        bench_file.remove()?;
        Ok(bench_file)
    }

    fn remove(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {error}", self.path.display()))
            }
            _ => Ok(()),
        }
    }
}

impl Drop for BenchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
