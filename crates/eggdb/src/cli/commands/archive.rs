use super::{Access, Session};
use crate::cli::args::Format;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use eggdb_platform::time::{SystemTime, UNIX_EPOCH};

pub(super) const MANIFEST: &str = "manifest.jsonl";
const STAGING_SUFFIX: &str = ".restoring";
const UPLOAD_SUFFIX: &str = ".uploading";
pub(super) const BUCKET_SCHEME: &str = "s3://";
const MS_PER_DAY: i64 = 86_400_000;

fn bucket(url: &str) -> Result<eggdb_s3::client::Location, String> {
    let largest = usize::try_from(16u64 << 30).unwrap_or(usize::MAX);
    eggdb_s3::client::Location::parse(url, &|name| std::env::var(name).ok(), largest)
}

/// A page store's objects under an `s3://bucket/prefix` (D1099).
pub(super) struct BucketObjects(eggdb_s3::client::Location);

impl eggdb::page_store::ObjectStore for BucketObjects {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        self.0.bucket.get(&self.0.key(name))
    }

    fn put(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.0.bucket.put(&self.0.key(name), bytes)
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        self.0.bucket.delete(&self.0.key(name))
    }
}

/// The objects of the page store at `url`, with the bucket's settings from the environment.
pub(super) fn page_store_objects(url: &str) -> Result<BucketObjects, String> {
    Ok(BucketObjects(bucket(url)?))
}

fn choose(entries: &[Archived], at: Option<&str>, shown: &str) -> Result<Archived, String> {
    let limit = match at {
        Some(text) => Some(
            parse_time(text)
                .ok_or_else(|| format!("--at {text}: give a time such as 2026-10-31T12:00:00Z"))?,
        ),
        None => None,
    };
    entries
        .iter()
        .filter(|entry| limit.is_none_or(|limit| entry.taken_at_ms <= limit))
        .max_by_key(|entry| entry.taken_at_ms)
        .cloned()
        .ok_or_else(|| format!("{shown} holds no archived copy taken at or before that time"))
}

/// A read-only edge copy of what the bucket at `url` archived at or before `at`: the copy kept in
/// the local cache (`EGGDB_EDGE_CACHE`, or `eggdb-edge` in the temporary directory) under its
/// SHA-256, downloaded and checked first when the cache lacks it.
const LOCKED_MANIFEST: &str = "manifest.d/";

/// The archived copies a bucket's manifest lists, each line stored under Object Lock in
/// `manifest.d/` taking the place of its line in `manifest.jsonl`, which stays writable (D1022).
/// Where archived copies are read from: a bucket, or a test's objects (D1100).
trait ArchiveObjects {
    fn object(&self, name: &str) -> Result<Option<Vec<u8>>, String>;
    fn object_at(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    fn keys_under(&self, name: &str) -> Result<Vec<String>, String>;
}

impl ArchiveObjects for eggdb_s3::client::Location {
    fn object(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        self.bucket.get(&self.key(name))
    }

    fn object_at(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.bucket.get(key)
    }

    fn keys_under(&self, name: &str) -> Result<Vec<String>, String> {
        self.bucket.list(&self.key(name))
    }
}

fn bucket_entries(location: &impl ArchiveObjects, url: &str) -> Result<Vec<Archived>, String> {
    let manifest_text = location.object(MANIFEST)?;
    let locked_keys = location.keys_under(LOCKED_MANIFEST)?;
    if manifest_text.is_none() && locked_keys.is_empty() {
        return Err(format!("{url} has no {MANIFEST}: archive into it with eggdb backup <file> {url}"));
    }
    let mut entries = parse_manifest(&String::from_utf8_lossy(&manifest_text.unwrap_or_default()));
    for key in locked_keys {
        let Some(line) = location.object_at(&key)? else {
            continue;
        };
        for locked in parse_manifest(&String::from_utf8_lossy(&line)) {
            entries.retain(|entry| entry.file != locked.file);
            entries.push(locked);
        }
    }
    Ok(entries)
}

pub(super) fn edge_copy(url: &str, at: Option<&str>) -> Result<PathBuf, String> {
    let location = bucket(url)?;
    let chosen = choose(&bucket_entries(&location, url)?, at, url)?;
    if chosen.sha256.len() != 64 || !chosen.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "the manifest of {url} gives {} no valid SHA-256",
            chosen.file
        ));
    }
    let cache = std::env::var_os("EGGDB_EDGE_CACHE")
        .map_or_else(|| std::env::temp_dir().join("eggdb-edge"), PathBuf::from);
    let copy = cache.join(format!("{}.eggdb", chosen.sha256));
    if file_sha256(&copy).is_ok_and(|actual| actual == chosen.sha256) {
        return Ok(copy);
    }
    std::fs::create_dir_all(&cache)
        .map_err(|error| format!("cannot create the edge cache {}: {error}", cache.display()))?;
    let bytes = location
        .bucket
        .get(&location.key(&chosen.file))?
        .ok_or_else(|| format!("{} is in the manifest but not in the bucket", chosen.file))?;
    let actual: String = eggdb_crypto::sha256(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != chosen.sha256 {
        return Err(format!(
            "{} has changed since it was archived (its SHA-256 differs from the manifest)",
            chosen.file
        ));
    }
    let mut staging = copy.clone().into_os_string();
    staging.push(STAGING_SUFFIX);
    let staging = PathBuf::from(staging);
    std::fs::File::create(&staging)
        .and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()))
        .and_then(|()| std::fs::rename(&staging, &copy))
        .map_err(|error| format!("cannot keep {} in the edge cache: {error}", chosen.file))?;
    Ok(copy)
}

/// Restores into the new file `target` the copy archived in the bucket at `url` taken at or
/// before `at` (the newest without it), after checking its SHA-256; returns the copy chosen.
pub(super) fn restore_from_bucket_into(
    url: &str,
    at: Option<&str>,
    target: &Path,
) -> Result<(String, i64), String> {
    restore_from_objects(&bucket(url)?, url, at, target)
}

fn restore_from_objects(
    location: &impl ArchiveObjects,
    url: &str,
    at: Option<&str>,
    target: &Path,
) -> Result<(String, i64), String> {
    let chosen = choose(&bucket_entries(location, url)?, at, url)?;
    let bytes = location
        .object(&chosen.file)?
        .ok_or_else(|| format!("{} is in the manifest but not in the bucket", chosen.file))?;
    let actual: String = eggdb_crypto::sha256(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual != chosen.sha256 {
        return Err(format!(
            "{} has changed since it was archived (its SHA-256 differs from the manifest); pick an earlier copy with --at",
            chosen.file
        ));
    }
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(format!(
            "{} exists; restoring an archived copy writes a new file",
            target.display()
        ));
    }
    let mut staging = target.to_path_buf().into_os_string();
    staging.push(STAGING_SUFFIX);
    let staging = PathBuf::from(staging);
    std::fs::File::create(&staging)
        .and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()))
        .map_err(|error| format!("cannot write {}: {error}", chosen.file))?;
    std::fs::rename(&staging, target)
        .map_err(|error| format!("cannot move the restored copy into place: {error}"))?;
    Ok((chosen.file, chosen.taken_at_ms))
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

pub(super) fn rfc3339(epoch_ms: i64) -> String {
    let (seconds, milliseconds) = (epoch_ms.div_euclid(1_000), epoch_ms.rem_euclid(1_000));
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{milliseconds:03}Z",
        of_day / 3_600,
        of_day / 60 % 60,
        of_day % 60
    )
}

/// Reads `YYYY-MM-DDTHH:MM[:SS[.fff]](Z|±HH:MM)` or a date alone (midnight UTC) as milliseconds
/// since 1970.
pub(super) fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim();
    let number = |from: usize, to: usize| text.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    if text.get(4..5)? != "-"
        || text.get(7..8)? != "-"
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if text.len() == 10 {
        return Some(days * 86_400_000);
    }
    if !matches!(text.get(10..11)?, "T" | " ") || text.get(13..14)? != ":" {
        return None;
    }
    let (hour, minute) = (number(11, 13)?, number(14, 16)?);
    let mut rest = &text[16..];
    let mut seconds = 0;
    let mut milliseconds = 0;
    if let Some(after) = rest.strip_prefix(':') {
        seconds = after.get(..2)?.parse::<i64>().ok()?;
        rest = &after[2..];
        if let Some(fraction) = rest.strip_prefix('.') {
            let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
            milliseconds = format!("{:0<3}", &fraction[..digits.min(3)])
                .parse::<i64>()
                .ok()?;
            rest = &fraction[digits..];
        }
    }
    let offset_minutes = match rest {
        "Z" | "z" | "" => 0,
        zone if zone.len() == 6 && zone.get(3..4) == Some(":") => {
            let sign = match zone.get(..1)? {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            sign * (zone.get(1..3)?.parse::<i64>().ok()? * 60
                + zone.get(4..6)?.parse::<i64>().ok()?)
        }
        _ => return None,
    };
    if hour > 23 || minute > 59 || seconds > 60 {
        return None;
    }
    Some(
        (((days * 24 + hour) * 60 + minute - offset_minutes) * 60 + seconds) * 1_000 + milliseconds,
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

fn file_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = eggdb_crypto::Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hash
        .finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[derive(Debug, Clone)]
struct Archived {
    file: String,
    taken_at_ms: i64,
    sha256: String,
    stamp: Option<u64>,
    differential: bool,
    base: Option<String>,
    encrypted: bool,
    locked_until_ms: Option<i64>,
}

fn json_field<'l>(line: &'l str, name: &str) -> Option<&'l str> {
    let marker = format!("\"{name}\":");
    let rest = &line[line.find(&marker)? + marker.len()..];
    let rest = rest.trim_start();
    if let Some(quoted) = rest.strip_prefix('"') {
        return quoted.find('"').map(|end| &quoted[..end]);
    }
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim())
}

fn manifest(directory: &Path) -> std::io::Result<Vec<Archived>> {
    Ok(parse_manifest(&std::fs::read_to_string(
        directory.join(MANIFEST),
    )?))
}

fn parse_manifest(text: &str) -> Vec<Archived> {
    let pruned: Vec<&str> = text.lines().filter_map(|line| json_field(line, "pruned")).collect();
    text.lines()
        .filter(|line| json_field(line, "file").is_none_or(|file| !pruned.contains(&file)))
        .filter_map(|line| {
            let file = json_field(line, "file")?;
            if file.contains(['/', '\\']) || file.starts_with('.') {
                return None;
            }
            Some(Archived {
                file: file.to_owned(),
                taken_at_ms: json_field(line, "taken_at_ms")?.parse().ok()?,
                sha256: json_field(line, "sha256")?.to_owned(),
                stamp: json_field(line, "stamp").and_then(|stamp| stamp.parse().ok()),
                differential: json_field(line, "kind") == Some("differential"),
                base: json_field(line, "base").map(str::to_owned),
                encrypted: json_field(line, "encrypted") == Some("true"),
                locked_until_ms: json_field(line, "locked_until").and_then(parse_time),
            })
        })
        .collect()
}

/// The directory of the change archive kept beside the copies of `directory`.
pub(super) const CHANGES: &str = "changes";

/// Replays the change archive beside `directory`'s copies onto the restored copy at `path`,
/// from the copy's stamp up to `at` (every archived commit without it). `None` when there is no
/// change archive, or the copy's manifest line predates stamps.
fn replay_into(path: &Path, directory: &Path, stamp: Option<u64>, at: Option<&str>) -> Result<Option<eggdb::ReplayReport>, String> {
    let changes = directory.join(CHANGES);
    let Some(stamp) = stamp.filter(|_| changes.is_dir()) else {
        return Ok(None);
    };
    let up_to = match at {
        Some(text) => {
            let at_ms = parse_time(text).ok_or_else(|| format!("--at {text} is not a time"))?;
            let micros = u64::try_from(at_ms).unwrap_or(0).saturating_mul(1000).saturating_add(999);
            let last = eggdb::read_change_archive(&changes)
                .map_err(|error| format!("cannot read the change archive: {error}"))?
                .into_iter()
                .filter_map(|event| match event {
                    eggdb::ArchivedEvent::Commit(found, _) if found.unix_micros() <= micros => Some(found),
                    _ => None,
                })
                .last();
            Some(last.unwrap_or(eggdb::CommitStamp::from_bits(stamp)))
        }
        None => None,
    };
    let db = eggdb::Database::open(path).map_err(|error| format!("cannot open the restored copy: {error}"))?;
    let report = eggdb::replay_changes(&db, &changes, eggdb::CommitStamp::from_bits(stamp), up_to)
        .map_err(|error| format!("cannot replay the change archive: {error}"));
    let closed = db.close().map_err(|error| format!("cannot close the restored copy: {error}"));
    let report = report?;
    closed?;
    Ok(Some(report))
}

/// The archived copy in `directory` taken at or before `at` (the newest without it), after
/// checking its SHA-256 against the manifest.
pub(super) fn archived_copy(directory: &Path, at: Option<&str>) -> Result<PathBuf, String> {
    let shown = render::text(directory.display().to_string().as_bytes());
    let entries: Vec<Archived> = manifest(directory)
        .map_err(|error| format!("cannot read {shown}/{MANIFEST}: {error}"))?
        .into_iter()
        .filter(|entry| !entry.differential && !entry.encrypted)
        .collect();
    let chosen = choose(&entries, at, &shown)?;
    let source = directory.join(&chosen.file);
    let actual =
        file_sha256(&source).map_err(|error| format!("cannot read {}: {error}", chosen.file))?;
    if actual != chosen.sha256 {
        return Err(format!(
            "{} has changed since it was archived (its SHA-256 differs from the manifest); pick an earlier copy with --at",
            chosen.file
        ));
    }
    Ok(source)
}

/// A verified copy to upload: a file staged on disk, or its bytes in memory (D1100).
enum UploadedCopy<'a> {
    Staged(&'a Path),
    InMemory(Vec<u8>),
}

/// Copies up to this size are built in memory and uploaded with no staging file (D1100).
const IN_MEMORY_UPLOAD_BYTES: u64 = 1 << 30;

fn hex_of(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn upload_copy(
    location: &eggdb_s3::client::Location,
    copy: UploadedCopy<'_>,
    name: &str,
    taken_at_ms: i64,
    shown: &str,
    locked_until_ms: Option<i64>,
) -> Result<(String, u64), String> {
    let not_verified = |error: eggdb::Error| format!("the copy of {shown} does not verify: {error}");
    let read_back = |error: std::io::Error| format!("cannot read back the copy: {error}");
    let retain_until = locked_until_ms.map(rfc3339);
    let (verified, size, sha256) = match copy {
        UploadedCopy::Staged(staging) => {
            let verified = eggdb::Database::open_with(staging, eggdb::Options::default().read_only(true))
                .and_then(|copy| copy.check())
                .map_err(not_verified)?;
            let size = std::fs::metadata(staging).map_err(read_back)?.len();
            let sha256 = file_sha256(staging).map_err(read_back)?;
            if size > eggdb_s3::client::PART_BYTES as u64 {
                let mut file = std::fs::File::open(staging).map_err(read_back)?;
                location.bucket.put_from(&location.key(name), &mut file, retain_until.as_deref())?;
            } else {
                let bytes = std::fs::read(staging).map_err(read_back)?;
                match &retain_until {
                    Some(until) => location.bucket.put_locked(&location.key(name), &bytes, until)?,
                    None => location.bucket.put(&location.key(name), &bytes)?,
                }
            }
            (verified, size, sha256)
        }
        UploadedCopy::InMemory(bytes) => {
            let verified = eggdb::Database::from_image(bytes.clone(), eggdb::Options::default())
                .and_then(|copy| copy.check())
                .map_err(not_verified)?;
            let size = bytes.len() as u64;
            let sha256 = hex_of(&eggdb_crypto::sha256(&bytes));
            if bytes.len() > eggdb_s3::client::PART_BYTES {
                let mut reader = std::io::Cursor::new(bytes);
                location.bucket.put_from(&location.key(name), &mut reader, retain_until.as_deref())?;
            } else {
                match &retain_until {
                    Some(until) => location.bucket.put_locked(&location.key(name), &bytes, until)?,
                    None => location.bucket.put(&location.key(name), &bytes)?,
                }
            }
            (verified, size, sha256)
        }
    };
    let mut line = JsonObject::new()
        .string("file", name)
        .string("taken_at", &rfc3339(taken_at_ms))
        .number("taken_at_ms", u64::try_from(taken_at_ms).unwrap_or(0))
        .number("bytes", size)
        .number("entries", verified.entries)
        .string("sha256", &sha256)
        .boolean("verified", true);
    if let Some(until) = locked_until_ms {
        line = line.string("locked_until", &rfc3339(until));
    }
    let line = line.line();
    if let Some(until) = &retain_until {
        location.bucket.put_locked(&location.key(&format!("{LOCKED_MANIFEST}{name}.json")), line.as_bytes(), until)?;
    }
    let mut manifest_text = location
        .bucket
        .get(&location.key(MANIFEST))?
        .unwrap_or_default();
    manifest_text.extend_from_slice(line.as_bytes());
    location
        .bucket
        .put(&location.key(MANIFEST), &manifest_text)?;
    Ok((line, verified.entries))
}

impl Session<'_> {
    pub(super) fn backup_into_directory(&mut self, directory: &Path) -> Result<Exit, Failure> {
        let settings = self.archive_settings();
        let outcome = archive_database(&self.request.file, directory, &settings)
            .map_err(|message| Failure::database(message, Exit::DatabaseError))?;
        self.print_archived(&self.file.clone(), directory, &outcome);
        Ok(Exit::Done)
    }

    fn archive_settings(&self) -> ArchiveSettings {
        ArchiveSettings {
            differential: self.request.differential,
            clone: self.request.clone_copy,
            lock_days: self.request.lock_days,
            key: self.request.backup_key.as_ref().map(|key| key.0),
            keep: self.request.keep_copies,
            options: self.database_options(Access::ReadOnly, None),
        }
    }

    fn print_archived(&mut self, file: &str, directory: &Path, outcome: &ArchiveOutcome) {
        let shown = render::text(directory.display().to_string().as_bytes());
        match self.format {
            Format::Json => self.out.text(&outcome.line),
            _ => {
                let kind = if outcome.differential { "a differential backup of " } else { "" };
                self.out.text(&format!(
                    "archived {kind}{file} into {shown}/{} ({} {}, verified{})\n",
                    outcome.name,
                    render::count(outcome.entries),
                    if outcome.differential { "changes" } else { "entries" },
                    if outcome.encrypted { ", encrypted" } else { "" }
                ));
                for pruned in &outcome.pruned {
                    self.out.text(&format!("pruned {pruned}\n"));
                }
            }
        }
    }

    /// Archives every database of a served directory (`<name>.eggdb`, D1020) into its own
    /// subdirectory of `archive`, several at once.
    pub(super) fn backup_served_directory(&mut self, archive: &Path) -> Result<Exit, Failure> {
        let failed = |message: String| Failure::database(message, Exit::DatabaseError);
        let source = self.request.file.clone();
        let mut databases: Vec<PathBuf> = std::fs::read_dir(&source)
            .map_err(|error| failed(format!("cannot read {}: {error}", self.file)))?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|extension| extension == "eggdb"))
            .collect();
        databases.sort();
        if databases.is_empty() {
            return Err(failed(format!("{} holds no databases (files named <name>.eggdb)", self.file)));
        }
        let settings = self.archive_settings();
        let workers = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get).min(MOST_PARALLEL_BACKUPS).min(databases.len());
        let next = std::sync::atomic::AtomicUsize::new(0);
        let outcomes: std::sync::Mutex<Vec<(usize, Result<ArchiveOutcome, String>)>> = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(database) = databases.get(index) else {
                            break;
                        };
                        let name = database.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
                        let directory = archive.join(&name);
                        let outcome = std::fs::create_dir_all(&directory)
                            .map_err(|error| format!("cannot create {}: {error}", directory.display()))
                            .and_then(|()| archive_database(database, &directory, &settings));
                        if let Ok(mut outcomes) = outcomes.lock() {
                            outcomes.push((index, outcome));
                        }
                    }
                });
            }
        });
        let mut outcomes = outcomes.into_inner().unwrap_or_default();
        outcomes.sort_by_key(|(index, _)| *index);
        let mut failures = Vec::new();
        for (index, outcome) in outcomes {
            let database = &databases[index];
            let name = database.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
            match outcome {
                Ok(outcome) => self.print_archived(&name, &archive.join(&name), &outcome),
                Err(message) => failures.push(format!("{name}: {message}")),
            }
        }
        if failures.is_empty() {
            Ok(Exit::Done)
        } else {
            Err(failed(format!("{} of {} databases were not archived: {}", failures.len(), databases.len(), failures.join("; "))))
        }
    }

    pub(super) fn backup_into_bucket(&mut self, url: &str) -> Result<Exit, Failure> {
        let failed = |message: String| Failure::database(message, Exit::DatabaseError);
        let location = bucket(url).map_err(|message| {
            Failure::usage(message, Some(crate::cli::args::CommandName::Backup))
        })?;
        let taken_at_ms = now_ms();
        let stem = self
            .request
            .file
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "database".to_owned());
        let stamp: String = rfc3339(taken_at_ms)
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect();
        let name = format!("{stem}-{stamp}.eggdb");
        let mut staging = self.request.file.clone().into_os_string();
        staging.push(format!(".{stamp}{UPLOAD_SUFFIX}"));
        let staging = PathBuf::from(staging);
        let locked_until_ms = self
            .request
            .lock_days
            .map(|days| taken_at_ms.saturating_add(i64::from(days) * MS_PER_DAY));
        let db = self.open(Access::ReadOnly, None)?;
        let in_memory = db.stats().is_ok_and(|stats| stats.file_bytes <= IN_MEMORY_UPLOAD_BYTES);
        let outcome = if in_memory {
            let copied = db.backup_to_bytes();
            drop(db);
            let (bytes, _) = copied.map_err(|error| failed(format!("cannot copy {} for {url}: {error}", self.file)))?;
            upload_copy(&location, UploadedCopy::InMemory(bytes), &name, taken_at_ms, &self.file, locked_until_ms)
        } else {
            let copied = db.backup_to(&staging);
            drop(db);
            copied.map_err(|error| failed(format!("cannot copy {} for {url}: {error}", self.file)))?;
            let outcome = upload_copy(&location, UploadedCopy::Staged(&staging), &name, taken_at_ms, &self.file, locked_until_ms);
            let _ = std::fs::remove_file(&staging);
            outcome
        };
        let (line, entries) = outcome.map_err(failed)?;
        match self.format {
            Format::Json => self.out.text(&line),
            _ => self.out.text(&format!(
                "archived {} into {url} as {name} ({} entries, verified)\n",
                self.file,
                render::count(entries)
            )),
        }
        Ok(Exit::Done)
    }

    pub(super) fn restore_from_bucket(
        &mut self,
        url: &str,
        at: Option<&str>,
    ) -> Result<Exit, Failure> {
        let target = self.request.file.clone();
        let (file, taken_at_ms) = restore_from_bucket_into(url, at, &target)
            .map_err(|message| Failure::database(message, Exit::DatabaseError))?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("restored", &file)
                    .string("taken_at", &rfc3339(taken_at_ms))
                    .string("into", &self.file_display)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "restored {file} (taken {}) from {url} into {}\n",
                rfc3339(taken_at_ms),
                self.file
            )),
        }
        Ok(Exit::Done)
    }

    pub(super) fn restore_from_archive(
        &mut self,
        directory: &Path,
        at: Option<&str>,
    ) -> Result<Exit, Failure> {
        let failed = |message: String| Failure::database(message, Exit::DatabaseError);
        let shown = render::text(directory.display().to_string().as_bytes());
        let entries = manifest(directory).map_err(|error| failed(format!("cannot read {shown}/{MANIFEST}: {error}")))?;
        let chosen = choose(&entries, at, &shown).map_err(failed)?;
        let base = match (&chosen.differential, &chosen.base) {
            (true, Some(base)) => entries
                .iter()
                .find(|entry| entry.file == *base)
                .cloned()
                .ok_or_else(|| failed(format!("{} is a differential backup of {base}, which is not in {shown}", chosen.file)))?,
            _ => chosen.clone(),
        };
        let target = self.request.file.clone();
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err(Failure::coded(
                "already_exists",
                format!("{} exists; restoring an archived copy writes a new file", self.file),
                Exit::DatabaseError,
            ));
        }
        let key = self.request.backup_key.as_ref().map(|key| key.0);
        let mut staging = target.clone().into_os_string();
        staging.push(STAGING_SUFFIX);
        let staging = PathBuf::from(staging);
        let (source, temporary) = plain_copy(directory, &base, key.as_ref()).map_err(failed)?;
        let copied = match &temporary {
            Some(temporary) => std::fs::rename(temporary, &staging),
            None => std::fs::copy(&source, &staging).map(drop),
        };
        copied
            .and_then(|()| std::fs::File::open(&staging)?.sync_all())
            .map_err(|error| failed(format!("cannot copy {}: {error}", base.file)))?;
        let mut replayed_commits = 0;
        let mut reached = base.stamp;
        if chosen.differential {
            let applied = apply_differential(&staging, directory, &chosen, base.stamp, key.as_ref());
            match applied {
                Ok(()) => {
                    replayed_commits += 1;
                    reached = chosen.stamp;
                }
                Err(message) => {
                    let _ = std::fs::remove_file(&staging);
                    return Err(failed(message));
                }
            }
        }
        let replayed = match replay_into(&staging, directory, reached, at) {
            Ok(replayed) => replayed,
            Err(message) => {
                let _ = std::fs::remove_file(&staging);
                return Err(failed(message));
            }
        };
        replayed_commits += replayed.map_or(0, |report| report.commits);
        let position = replayed.map(|report| report.reached.bits()).or(reached);
        if let Some(position) = position
            && let Err(message) = record_standby_position(&staging, position)
        {
            let _ = std::fs::remove_file(&staging);
            return Err(failed(message));
        }
        std::fs::rename(&staging, &target)
            .map_err(|error| failed(format!("cannot move the restored copy into place: {error}")))?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("restored", &chosen.file)
                    .string("taken_at", &rfc3339(chosen.taken_at_ms))
                    .string("into", &self.file_display)
                    .number("replayed_commits", replayed_commits)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "restored {} (taken {}) into {}{}\n",
                chosen.file,
                rfc3339(chosen.taken_at_ms),
                self.file,
                if replayed_commits > 0 { format!(", then replayed {} archived commits", render::count(replayed_commits)) } else { String::new() }
            )),
        }
        Ok(Exit::Done)
    }
}

const MOST_PARALLEL_BACKUPS: usize = 8;

/// How [`archive_database`] archives a database into a directory.
struct ArchiveSettings {
    differential: bool,
    clone: bool,
    lock_days: Option<u32>,
    key: Option<[u8; 32]>,
    keep: Option<u32>,
    options: eggdb::Options,
}

/// What [`archive_database`] archived.
struct ArchiveOutcome {
    line: String,
    name: String,
    entries: u64,
    differential: bool,
    encrypted: bool,
    pruned: Vec<String>,
}

/// Archives the database at `source` into `directory` (D208, D1020): a full copy, checked by
/// opening it and reading every page, or with `differential` the changes since the newest
/// full copy there; sealed with the backup key when one is given; recorded in the manifest
/// with its commit stamp; and with `keep`, the older copies beyond it pruned.
fn archive_database(source: &Path, directory: &Path, settings: &ArchiveSettings) -> Result<ArchiveOutcome, String> {
    let shown = render::text(directory.display().to_string().as_bytes());
    let file_shown = source.display().to_string();
    let taken_at_ms = now_ms();
    let stem = source.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_else(|| "database".to_owned());
    let time: String = rfc3339(taken_at_ms).chars().filter(|character| character.is_ascii_alphanumeric()).collect();
    let plain_name = format!("{stem}-{time}.{}", if settings.differential { "eggdelta" } else { "eggdb" });
    let plain = directory.join(&plain_name);
    let open_source = || eggdb::Database::open_with(source, settings.options.clone()).map_err(|error| format!("cannot open {file_shown}: {error}"));
    let (entries, stamp, base) = if settings.differential {
        let known = manifest(directory).unwrap_or_default();
        let base = known
            .iter()
            .filter(|entry| !entry.differential)
            .max_by_key(|entry| entry.taken_at_ms)
            .cloned()
            .ok_or_else(|| format!("{shown} holds no full copy to take a differential backup against; run eggdb backup without --differential first"))?;
        let (base_path, temporary) = plain_copy(directory, &base, settings.key.as_ref())?;
        let outcome = eggdb::Database::open_with(&base_path, eggdb::Options::default().read_only(true))
            .map_err(|error| format!("cannot open the full copy {}: {error}", base.file))
            .and_then(|base_db| {
                let db = open_source()?;
                db.backup_differential_to(&base_db, &plain)
                    .map_err(|error| format!("cannot write a differential backup into {shown}: {error}"))
            });
        if let Some(temporary) = &temporary {
            let _ = std::fs::remove_file(temporary);
        }
        let report = outcome?;
        eggdb::read_change_file(&plain)
            .map_err(|error| format!("the differential backup {plain_name} does not read back: {error}; it was kept for inspection"))?;
        (report.changes, report.stamp.bits(), Some(base.file))
    } else {
        let db = open_source()?;
        let copied = if settings.clone { db.clone_to(&plain).map(|report| report.stamp) } else { db.backup_to(&plain).map(|report| report.stamp) };
        drop(db);
        let copied_stamp = copied.map_err(|error| format!("cannot archive {file_shown} into {shown}: {error}"))?;
        let verified = eggdb::Database::open_with(&plain, eggdb::Options::default().read_only(true))
            .and_then(|copy| copy.check())
            .map_err(|error| format!("the archived copy {plain_name} does not verify: {error}; it was kept for inspection"))?;
        (verified.entries, copied_stamp.bits(), None)
    };
    let (name, stored) = match &settings.key {
        Some(key) => {
            let sealed_name = format!("{plain_name}.enc");
            let sealed = directory.join(&sealed_name);
            super::sealed::seal_file(&plain, &sealed, key)?;
            std::fs::remove_file(&plain).map_err(|error| format!("cannot remove the unencrypted {plain_name}: {error}"))?;
            (sealed_name, sealed)
        }
        None => (plain_name, plain),
    };
    let sha256 = file_sha256(&stored).map_err(|error| format!("cannot read back {name}: {error}"))?;
    let bytes = std::fs::metadata(&stored).map(|metadata| metadata.len()).unwrap_or(0);
    let locked = match settings.lock_days {
        Some(days) => Some((taken_at_ms + i64::from(days) * MS_PER_DAY, lock_local(&stored)?)),
        None => None,
    };
    let mut line = JsonObject::new()
        .string("file", &name)
        .string("taken_at", &rfc3339(taken_at_ms))
        .number("taken_at_ms", u64::try_from(taken_at_ms).unwrap_or(0))
        .number("bytes", bytes)
        .number("entries", entries)
        .string("sha256", &sha256)
        .boolean("verified", true)
        .number("stamp", stamp)
        .boolean("encrypted", settings.key.is_some());
    if let Some(base) = &base {
        line = line.string("kind", "differential").string("base", base);
    }
    if let Some((until, immutable)) = locked {
        line = line.string("locked_until", &rfc3339(until)).boolean("immutable", immutable);
    }
    let line = line.line();
    append_manifest(directory, &line).map_err(|error| format!("cannot record {name} in {shown}/{MANIFEST}: {error}"))?;
    let pruned = match settings.keep {
        Some(keep) => prune(directory, keep).map_err(|error| format!("cannot prune {shown}: {error}"))?,
        None => Vec::new(),
    };
    Ok(ArchiveOutcome { line, name, entries, differential: base.is_some(), encrypted: settings.key.is_some(), pruned })
}

/// The plain database file or differential backup an archive entry holds, after checking its
/// SHA-256: the stored file itself, or a decrypted copy beside it (returned second, for the
/// caller to move or remove) when the entry is encrypted.
fn plain_copy(directory: &Path, entry: &Archived, key: Option<&[u8; 32]>) -> Result<(PathBuf, Option<PathBuf>), String> {
    let stored = directory.join(&entry.file);
    let actual = file_sha256(&stored).map_err(|error| format!("cannot read {}: {error}", entry.file))?;
    if actual != entry.sha256 {
        return Err(format!(
            "{} has changed since it was archived (its SHA-256 differs from the manifest); pick an earlier copy with --at",
            entry.file
        ));
    }
    if !entry.encrypted {
        return Ok((stored, None));
    }
    let key = key.ok_or_else(|| format!("{} is encrypted; give the key with --backup-key-file", entry.file))?;
    let opened = directory.join(format!(".{}.opened", entry.file));
    super::sealed::open_file(&stored, &opened, key)?;
    Ok((opened.clone(), Some(opened)))
}

fn apply_differential(staging: &Path, directory: &Path, delta: &Archived, base_stamp: Option<u64>, key: Option<&[u8; 32]>) -> Result<(), String> {
    let base_stamp = base_stamp.ok_or("the full copy's manifest line has no stamp, so its differential cannot be applied")?;
    let (path, temporary) = plain_copy(directory, delta, key)?;
    let events = eggdb::read_change_file(&path).map_err(|error| format!("cannot read {}: {error}", delta.file));
    if let Some(temporary) = &temporary {
        let _ = std::fs::remove_file(temporary);
    }
    let events = events?;
    let db = eggdb::Database::open(staging).map_err(|error| format!("cannot open the restored copy: {error}"))?;
    let applied = eggdb::replay_events(&db, events, &path, eggdb::CommitStamp::from_bits(base_stamp), None)
        .map_err(|error| format!("cannot apply {}: {error}", delta.file));
    let closed = db.close().map_err(|error| format!("cannot close the restored copy: {error}"));
    applied?;
    closed
}

/// Makes an archived file read-only and, where the file system and privileges allow, immutable
/// (D1022); returns whether it is immutable.
fn lock_local(path: &Path) -> Result<bool, String> {
    let mut permissions = std::fs::metadata(path).map_err(|error| format!("cannot lock {}: {error}", path.display()))?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions).map_err(|error| format!("cannot lock {}: {error}", path.display()))?;
    eggdb_platform::set_immutable(path, true).map_err(|error| format!("cannot lock {}: {error}", path.display()))
}

fn unlock_local(path: &Path) -> std::io::Result<()> {
    eggdb_platform::set_immutable(path, false)?;
    let mut permissions = std::fs::metadata(path)?.permissions();
    owner_writable(&mut permissions);
    std::fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn owner_writable(permissions: &mut std::fs::Permissions) {
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o600);
}

#[cfg(not(unix))]
fn owner_writable(permissions: &mut std::fs::Permissions) {
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
}

/// Records in a restored copy the stamp of the last archived commit it holds, so it can start
/// as a standby fed by the same change archive (D1027).
fn record_standby_position(path: &Path, stamp: u64) -> Result<(), String> {
    let db = eggdb::Database::open(path).map_err(|error| format!("cannot open the restored copy: {error}"))?;
    let recorded = db
        .set_standby_position(eggdb::CommitStamp::from_bits(stamp))
        .map_err(|error| format!("cannot record the restored copy's position: {error}"));
    let closed = db.close().map_err(|error| format!("cannot close the restored copy: {error}"));
    recorded?;
    closed
}

fn append_manifest(directory: &Path, line: &str) -> std::io::Result<()> {
    let mut manifest = std::fs::OpenOptions::new().create(true).append(true).open(directory.join(MANIFEST))?;
    manifest.write_all(line.as_bytes())?;
    manifest.sync_all()
}

/// Keeps the newest `keep` full copies in `directory` and the differential backups taken
/// against them; removes the rest and records each removal in the manifest. Returns the files
/// removed.
fn prune(directory: &Path, keep: u32) -> std::io::Result<Vec<String>> {
    let entries = manifest(directory)?;
    let mut fulls: Vec<&Archived> = entries.iter().filter(|entry| !entry.differential).collect();
    fulls.sort_by_key(|entry| std::cmp::Reverse(entry.taken_at_ms));
    let kept: Vec<&str> = fulls.iter().take(keep as usize).map(|entry| entry.file.as_str()).collect();
    let mut pruned = Vec::new();
    let now = now_ms();
    for entry in &entries {
        if entry.locked_until_ms.is_some_and(|until| until > now) {
            continue;
        }
        let keeps = if entry.differential {
            entry.base.as_deref().is_some_and(|base| kept.contains(&base))
        } else {
            kept.contains(&entry.file.as_str())
        };
        if keeps {
            continue;
        }
        if entry.locked_until_ms.is_some() {
            match unlock_local(&directory.join(&entry.file)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        match std::fs::remove_file(directory.join(&entry.file)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        append_manifest(directory, &JsonObject::new().string("pruned", &entry.file).line())?;
        pruned.push(entry.file.clone());
    }
    let oldest_kept = fulls.iter().take(keep as usize).filter_map(|entry| entry.stamp).min();
    if let Some(oldest_kept) = oldest_kept {
        pruned.extend(prune_change_segments(&directory.join(CHANGES), oldest_kept)?);
    }
    Ok(pruned)
}

fn segment_latest_stamp(path: &Path) -> Option<u64> {
    eggdb::read_change_file(path)
        .ok()?
        .iter()
        .map(|event| match event {
            eggdb::ArchivedEvent::Base(stamp) | eggdb::ArchivedEvent::Gap(stamp) | eggdb::ArchivedEvent::Commit(stamp, _) => stamp.bits(),
        })
        .max()
}

/// Removes the change archive's segments that only hold commits the oldest kept copy already
/// has, keeping the newest of them, which shows where the archive reaches back to.
fn prune_change_segments(changes: &Path, oldest_kept: u64) -> std::io::Result<Vec<String>> {
    let entries = match std::fs::read_dir(changes) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut segments: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("changes-") && name.ends_with(".eggcl"))
        .collect();
    segments.sort();
    let covered: Vec<bool> = segments
        .iter()
        .map(|name| segment_latest_stamp(&changes.join(name)).is_some_and(|latest| latest <= oldest_kept))
        .collect();
    let Some(last_covered) = covered.iter().rposition(|covered| *covered) else {
        return Ok(Vec::new());
    };
    let mut removed = Vec::new();
    for (index, name) in segments.iter().enumerate().take(last_covered) {
        if covered[index] {
            std::fs::remove_file(changes.join(name))?;
            removed.push(format!("{CHANGES}/{name}"));
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod restore_under_failures {
    use super::{ArchiveObjects, MANIFEST, restore_from_objects};
    use std::cell::Cell;
    use std::collections::BTreeMap;

    struct FaultyObjects {
        objects: BTreeMap<String, Vec<u8>>,
        state: Cell<u64>,
    }

    impl FaultyObjects {
        fn next(&self) -> u64 {
            let mut state = self.state.get();
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            self.state.set(state);
            state
        }

        fn served(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
            let found = self.objects.get(key).cloned();
            match self.next() % 10 {
                0 => Err("connection reset by peer".to_owned()),
                1 => Ok(found.map(|bytes| bytes[..bytes.len() / 2].to_vec())),
                2 => Ok(found.map(|mut bytes| {
                    let at = (self.next() as usize) % bytes.len().max(1);
                    if let Some(byte) = bytes.get_mut(at) {
                        *byte ^= 0x5a;
                    }
                    bytes
                })),
                3 => Ok(None),
                _ => Ok(found),
            }
        }
    }

    impl ArchiveObjects for FaultyObjects {
        fn object(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
            self.served(name)
        }

        fn object_at(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
            self.served(key)
        }

        fn keys_under(&self, _: &str) -> Result<Vec<String>, String> {
            match self.next() % 10 {
                0 => Err("the listing timed out".to_owned()),
                _ => Ok(Vec::new()),
            }
        }
    }

    fn archived_copy() -> (Vec<u8>, String) {
        let db = eggdb::Database::in_memory().unwrap();
        db.write(|tx| {
            for index in 0..300u32 {
                tx.put(index.to_be_bytes(), vec![index as u8; 64])?;
            }
            Ok(())
        })
        .unwrap();
        let (bytes, _) = db.backup_to_bytes().unwrap();
        let sha256: String = eggdb_crypto::sha256(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
        (bytes, sha256)
    }

    #[test]
    fn a_restore_under_failures_gives_the_whole_copy_or_no_file() {
        let (bytes, sha256) = archived_copy();
        let manifest = format!("{{\"file\":\"db-1.eggdb\",\"taken_at_ms\":1000,\"sha256\":\"{sha256}\"}}\n");
        let objects = BTreeMap::from([
            (MANIFEST.to_owned(), manifest.into_bytes()),
            ("db-1.eggdb".to_owned(), bytes.clone()),
        ]);
        let directory = std::env::temp_dir().join(format!("eggdb-restore-faults-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut restored = 0;
        for seed in 1..=200u64 {
            let faulty = FaultyObjects { objects: objects.clone(), state: Cell::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1) };
            let target = directory.join(format!("restored-{seed}.eggdb"));
            match restore_from_objects(&faulty, "s3://test/archive", None, &target) {
                Ok((file, taken_at_ms)) => {
                    assert_eq!((file.as_str(), taken_at_ms), ("db-1.eggdb", 1000));
                    assert_eq!(std::fs::read(&target).unwrap(), bytes, "seed {seed}");
                    let copy = eggdb::Database::open_with(&target, eggdb::Options::new().read_only(true)).unwrap();
                    assert_eq!(copy.check().unwrap().entries, 300, "seed {seed}");
                    restored += 1;
                }
                Err(message) => {
                    assert!(!target.exists(), "seed {seed} failed with {message} but left {}", target.display());
                }
            }
        }
        let _ = std::fs::remove_dir_all(&directory);
        assert!(restored > 20, "only {restored} of 200 restores got through");
        assert!(restored < 200, "no failure was ever injected");
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_time, rfc3339};

    #[test]
    fn times_read_and_write_as_rfc_3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            parse_time("2026-10-31T12:30:15.250Z"),
            Some(1_793_449_815_250)
        );
        assert_eq!(rfc3339(1_793_449_815_250), "2026-10-31T12:30:15.250Z");
        assert_eq!(
            parse_time("2026-10-31T14:30:15.25+02:00"),
            Some(1_793_449_815_250)
        );
        assert_eq!(parse_time("1970-01-02"), Some(86_400_000));
        for bad in [
            "",
            "2026-13-01",
            "2026-10-31T25:00Z",
            "yesterday",
            "2026-10-31T12:00+2",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }
}
