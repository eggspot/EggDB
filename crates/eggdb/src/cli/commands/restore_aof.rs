use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use eggdb_resp::{AofManifest, AofReplay, AofReport};
use std::io::Read;
use std::path::{Path, PathBuf};

const MANIFEST_SUFFIX: &str = ".manifest";
const MAX_MANIFEST_BYTES: u64 = 1 << 20;

fn now_ms() -> u64 {
    eggdb_platform::time::SystemTime::now()
        .duration_since(eggdb_platform::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(super) fn manifests_of(path: &Path) -> Option<Vec<PathBuf>> {
    if path.is_file() {
        return is_manifest(path).then(|| vec![path.to_path_buf()]);
    }
    let mut manifests: Vec<PathBuf> = std::fs::read_dir(path)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|entry| entry.is_file() && is_manifest(entry))
        .collect();
    manifests.sort();
    (!manifests.is_empty()).then_some(manifests)
}

fn is_manifest(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(MANIFEST_SUFFIX))
}

fn usage(message: String) -> Failure {
    Failure::usage(message, Some(CommandName::Restore))
}

fn read_manifest(manifest: &Path, shown: &str) -> Result<AofManifest, Failure> {
    let file = std::fs::File::open(manifest)
        .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
    let mut text = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut text)
        .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
    if text.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(usage(format!(
            "cannot restore {shown}: the manifest is larger than {MAX_MANIFEST_BYTES} bytes"
        )));
    }
    eggdb_resp::parse_aof_manifest(&text).map_err(|message| {
        usage(format!(
            "cannot restore {shown}: {}",
            render::text(message.as_bytes())
        ))
    })
}

impl Session<'_> {
    pub(super) fn restore_aof(&mut self, dump: &Path) -> Result<Exit, Failure> {
        let shown = render::text(dump.display().to_string().as_bytes());
        let input = std::fs::File::open(dump)
            .map_err(|error| usage(format!("cannot read {shown}: {error}")))?;
        let db = self.open(Access::Write { create: true }, None)?;
        let keyspace = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::RESP_COLLECTIONS,
        };
        let report = eggdb_resp::replay_aof(&keyspace, std::io::BufReader::new(input), now_ms())
            .map_err(|message| {
                Failure::database(
                    format!("cannot restore {shown}: {message}"),
                    Exit::DatabaseError,
                )
            })?;
        self.report_aof(&shown, "Redis append-only file", 1, &report);
        self.close(db)?;
        Ok(aof_exit(&report))
    }

    pub(super) fn restore_aof_directory(
        &mut self,
        dump: &Path,
        manifests: &[PathBuf],
    ) -> Result<Exit, Failure> {
        let shown = render::text(dump.display().to_string().as_bytes());
        let [manifest_path] = manifests else {
            return Err(usage(format!(
                "cannot restore {shown}: it holds {} manifests; give the one to restore",
                manifests.len()
            )));
        };
        let manifest_shown = render::text(manifest_path.display().to_string().as_bytes());
        let manifest = read_manifest(manifest_path, &manifest_shown)?;
        let directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let mut files = Vec::with_capacity(manifest.replay.len());
        for listed in &manifest.replay {
            let path = directory.join(&listed.name);
            let name = render::text(listed.name.as_bytes());
            let file = std::fs::File::open(&path).map_err(|error| {
                usage(format!(
                    "cannot restore {shown}: the manifest lists {name}, which cannot be read: {error}"
                ))
            })?;
            if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
                return Err(usage(format!(
                    "cannot restore {shown}: the manifest lists {name}, which is not a file"
                )));
            }
            files.push((name, file));
        }
        let db = self.open(Access::Write { create: true }, None)?;
        let keyspace = super::serve_resp::DatabaseKeyspace {
            db: &db,
            prefix: super::serve_resp::RESP_COLLECTIONS,
        };
        let mut replay = AofReplay::new(&keyspace, now_ms());
        for (name, file) in files {
            replay
                .replay(std::io::BufReader::new(file))
                .map_err(|message| {
                    Failure::database(
                        format!("cannot restore {shown}: {name}: {message}"),
                        Exit::DatabaseError,
                    )
                })?;
        }
        let report = replay.finish();
        self.report_aof(
            &shown,
            "Redis multi-part append-only file",
            manifest.replay.len(),
            &report,
        );
        self.close(db)?;
        Ok(aof_exit(&report))
    }

    fn report_aof(&mut self, shown: &str, what: &str, files: usize, report: &AofReport) {
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("format", "redis-aof")
                    .number("files", files as u64)
                    .number("commands", report.commands)
                    .number("failed", report.failed)
                    .line(),
            ),
            _ => {
                let parts = if files == 1 {
                    String::new()
                } else {
                    format!(", {files} files")
                };
                self.out.text(&format!(
                    "restored {shown} ({what}{parts}): {} commands, {} failed\n",
                    report.commands, report.failed
                ));
                if let Some(error) = &report.first_error {
                    self.out.text(&format!(
                        "first failure: {}\n",
                        render::text(error.as_bytes())
                    ));
                }
            }
        }
    }
}

fn aof_exit(report: &AofReport) -> Exit {
    if report.failed == 0 {
        Exit::Done
    } else {
        Exit::DatabaseError
    }
}
