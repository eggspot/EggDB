use super::{Access, Session};
use crate::cli::args::Format;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const MANIFEST: &str = "manifest.jsonl";
const STAGING_SUFFIX: &str = ".restoring";
const UPLOAD_SUFFIX: &str = ".uploading";
pub(super) const BUCKET_SCHEME: &str = "s3://";

fn bucket(url: &str) -> Result<eggdb_s3::client::Location, String> {
    let largest = usize::try_from(16u64 << 30).unwrap_or(usize::MAX);
    eggdb_s3::client::Location::parse(url, &|name| std::env::var(name).ok(), largest)
}

fn choose(entries: &[Archived], at: Option<&str>, shown: &str) -> Result<Archived, String> {
    let limit = match at {
        Some(text) => Some(parse_time(text).ok_or_else(|| format!("--at {text}: give a time such as 2026-10-31T12:00:00Z"))?),
        None => None,
    };
    entries
        .iter()
        .filter(|entry| limit.is_none_or(|limit| entry.taken_at_ms <= limit))
        .max_by_key(|entry| entry.taken_at_ms)
        .map(|entry| Archived { file: entry.file.clone(), taken_at_ms: entry.taken_at_ms, sha256: entry.sha256.clone() })
        .ok_or_else(|| format!("{shown} holds no archived copy taken at or before that time"))
}

/// A read-only edge copy of what the bucket at `url` archived at or before `at`: the copy kept in
/// the local cache (`EGGDB_EDGE_CACHE`, or `eggdb-edge` in the temporary directory) under its
/// SHA-256, downloaded and checked first when the cache lacks it.
pub(super) fn edge_copy(url: &str, at: Option<&str>) -> Result<PathBuf, String> {
    let location = bucket(url)?;
    let manifest_text = location.bucket.get(&location.key(MANIFEST))?.ok_or_else(|| format!("{url} has no {MANIFEST}: archive into it with eggdb backup <file> {url}"))?;
    let chosen = choose(&parse_manifest(&String::from_utf8_lossy(&manifest_text)), at, url)?;
    if chosen.sha256.len() != 64 || !chosen.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("the manifest of {url} gives {} no valid SHA-256", chosen.file));
    }
    let cache = std::env::var_os("EGGDB_EDGE_CACHE").map_or_else(|| std::env::temp_dir().join("eggdb-edge"), PathBuf::from);
    let copy = cache.join(format!("{}.eggdb", chosen.sha256));
    if file_sha256(&copy).is_ok_and(|actual| actual == chosen.sha256) {
        return Ok(copy);
    }
    std::fs::create_dir_all(&cache).map_err(|error| format!("cannot create the edge cache {}: {error}", cache.display()))?;
    let bytes = location.bucket.get(&location.key(&chosen.file))?.ok_or_else(|| format!("{} is in the manifest but not in the bucket", chosen.file))?;
    let actual: String = eggdb_crypto::sha256(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    if actual != chosen.sha256 {
        return Err(format!("{} has changed since it was archived (its SHA-256 differs from the manifest)", chosen.file));
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
pub(super) fn restore_from_bucket_into(url: &str, at: Option<&str>, target: &Path) -> Result<(String, i64), String> {
    let location = bucket(url)?;
    let manifest_text = location.bucket.get(&location.key(MANIFEST))?.ok_or_else(|| format!("{url} has no {MANIFEST}: archive into it with eggdb backup <file> {url}"))?;
    let chosen = choose(&parse_manifest(&String::from_utf8_lossy(&manifest_text)), at, url)?;
    let bytes = location.bucket.get(&location.key(&chosen.file))?.ok_or_else(|| format!("{} is in the manifest but not in the bucket", chosen.file))?;
    let actual: String = eggdb_crypto::sha256(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    if actual != chosen.sha256 {
        return Err(format!("{} has changed since it was archived (its SHA-256 differs from the manifest); pick an earlier copy with --at", chosen.file));
    }
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(format!("{} exists; restoring an archived copy writes a new file", target.display()));
    }
    let mut staging = target.to_path_buf().into_os_string();
    staging.push(STAGING_SUFFIX);
    let staging = PathBuf::from(staging);
    std::fs::File::create(&staging)
        .and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()))
        .map_err(|error| format!("cannot write {}: {error}", chosen.file))?;
    std::fs::rename(&staging, target).map_err(|error| format!("cannot move the restored copy into place: {error}"))?;
    Ok((chosen.file, chosen.taken_at_ms))
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
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
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{milliseconds:03}Z", of_day / 3_600, of_day / 60 % 60, of_day % 60)
}

/// Reads `YYYY-MM-DDTHH:MM[:SS[.fff]](Z|±HH:MM)` or a date alone (midnight UTC) as milliseconds
/// since 1970.
pub(super) fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim();
    let number = |from: usize, to: usize| text.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    if text.get(4..5)? != "-" || text.get(7..8)? != "-" || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
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
            milliseconds = format!("{:0<3}", &fraction[..digits.min(3)]).parse::<i64>().ok()?;
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
            sign * (zone.get(1..3)?.parse::<i64>().ok()? * 60 + zone.get(4..6)?.parse::<i64>().ok()?)
        }
        _ => return None,
    };
    if hour > 23 || minute > 59 || seconds > 60 {
        return None;
    }
    Some((((days * 24 + hour) * 60 + minute - offset_minutes) * 60 + seconds) * 1_000 + milliseconds)
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
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
    Ok(hash.finish().iter().map(|byte| format!("{byte:02x}")).collect())
}

struct Archived {
    file: String,
    taken_at_ms: i64,
    sha256: String,
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
    Ok(parse_manifest(&std::fs::read_to_string(directory.join(MANIFEST))?))
}

fn parse_manifest(text: &str) -> Vec<Archived> {
    text.lines()
        .filter_map(|line| {
            let file = json_field(line, "file")?;
            if file.contains(['/', '\\']) || file.starts_with('.') {
                return None;
            }
            Some(Archived {
                file: file.to_owned(),
                taken_at_ms: json_field(line, "taken_at_ms")?.parse().ok()?,
                sha256: json_field(line, "sha256")?.to_owned(),
            })
        })
        .collect()
}

/// The archived copy in `directory` taken at or before `at` (the newest without it), after
/// checking its SHA-256 against the manifest.
pub(super) fn archived_copy(directory: &Path, at: Option<&str>) -> Result<PathBuf, String> {
    let shown = render::text(directory.display().to_string().as_bytes());
    let entries = manifest(directory).map_err(|error| format!("cannot read {shown}/{MANIFEST}: {error}"))?;
    let chosen = choose(&entries, at, &shown)?;
    let source = directory.join(&chosen.file);
    let actual = file_sha256(&source).map_err(|error| format!("cannot read {}: {error}", chosen.file))?;
    if actual != chosen.sha256 {
        return Err(format!("{} has changed since it was archived (its SHA-256 differs from the manifest); pick an earlier copy with --at", chosen.file));
    }
    Ok(source)
}

fn upload_copy(location: &eggdb_s3::client::Location, staging: &Path, name: &str, taken_at_ms: i64, shown: &str) -> Result<(String, u64), String> {
    let verified = eggdb::Database::open_with(staging, eggdb::Options::default().read_only(true))
        .and_then(|copy| copy.check())
        .map_err(|error| format!("the copy of {shown} does not verify: {error}"))?;
    let bytes = std::fs::read(staging).map_err(|error| format!("cannot read back the copy: {error}"))?;
    let sha256: String = eggdb_crypto::sha256(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    location.bucket.put(&location.key(name), &bytes)?;
    let line = JsonObject::new()
        .string("file", name)
        .string("taken_at", &rfc3339(taken_at_ms))
        .number("taken_at_ms", u64::try_from(taken_at_ms).unwrap_or(0))
        .number("bytes", bytes.len() as u64)
        .number("entries", verified.entries)
        .string("sha256", &sha256)
        .boolean("verified", true)
        .line();
    let mut manifest_text = location.bucket.get(&location.key(MANIFEST))?.unwrap_or_default();
    manifest_text.extend_from_slice(line.as_bytes());
    location.bucket.put(&location.key(MANIFEST), &manifest_text)?;
    Ok((line, verified.entries))
}

impl Session<'_> {
    pub(super) fn backup_into_directory(&mut self, directory: &Path) -> Result<Exit, Failure> {
        let shown = render::text(directory.display().to_string().as_bytes());
        let taken_at_ms = now_ms();
        let stem = self.request.file.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_else(|| "database".to_owned());
        let stamp: String = rfc3339(taken_at_ms).chars().filter(|character| character.is_ascii_alphanumeric()).collect();
        let name = format!("{stem}-{stamp}.eggdb");
        let target = directory.join(&name);
        let db = self.open(Access::ReadOnly, None)?;
        let copied = db.backup_to(&target);
        drop(db);
        copied.map_err(|error| Failure::database(format!("cannot archive {} into {shown}: {error}", self.file), Exit::DatabaseError))?;
        let verified = eggdb::Database::open_with(&target, eggdb::Options::default().read_only(true))
            .and_then(|copy| copy.check())
            .map_err(|error| Failure::database(format!("the archived copy {name} does not verify: {error}; it was kept for inspection"), Exit::DatabaseError))?;
        let sha256 = file_sha256(&target).map_err(|error| Failure::database(format!("cannot read back {name}: {error}"), Exit::DatabaseError))?;
        let bytes = std::fs::metadata(&target).map(|metadata| metadata.len()).unwrap_or(0);
        let line = JsonObject::new()
            .string("file", &name)
            .string("taken_at", &rfc3339(taken_at_ms))
            .number("taken_at_ms", u64::try_from(taken_at_ms).unwrap_or(0))
            .number("bytes", bytes)
            .number("entries", verified.entries)
            .string("sha256", &sha256)
            .boolean("verified", true)
            .line();
        let mut manifest = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(MANIFEST))
            .map_err(|error| Failure::database(format!("cannot record {name} in {shown}/{MANIFEST}: {error}"), Exit::DatabaseError))?;
        manifest
            .write_all(line.as_bytes())
            .and_then(|()| manifest.sync_all())
            .map_err(|error| Failure::database(format!("cannot record {name} in {shown}/{MANIFEST}: {error}"), Exit::DatabaseError))?;
        match self.format {
            Format::Json => self.out.text(&line),
            _ => self.out.text(&format!(
                "archived {} into {shown}/{name} ({} entries, verified)\n",
                self.file,
                render::count(verified.entries)
            )),
        }
        Ok(Exit::Done)
    }

    pub(super) fn backup_into_bucket(&mut self, url: &str) -> Result<Exit, Failure> {
        let failed = |message: String| Failure::database(message, Exit::DatabaseError);
        let location = bucket(url).map_err(|message| Failure::usage(message, Some(crate::cli::args::CommandName::Backup)))?;
        let taken_at_ms = now_ms();
        let stem = self.request.file.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_else(|| "database".to_owned());
        let stamp: String = rfc3339(taken_at_ms).chars().filter(|character| character.is_ascii_alphanumeric()).collect();
        let name = format!("{stem}-{stamp}.eggdb");
        let mut staging = self.request.file.clone().into_os_string();
        staging.push(format!(".{stamp}{UPLOAD_SUFFIX}"));
        let staging = PathBuf::from(staging);
        let db = self.open(Access::ReadOnly, None)?;
        let copied = db.backup_to(&staging);
        drop(db);
        copied.map_err(|error| failed(format!("cannot copy {} for {url}: {error}", self.file)))?;
        let outcome = upload_copy(&location, &staging, &name, taken_at_ms, &self.file);
        let _ = std::fs::remove_file(&staging);
        let (line, entries) = outcome.map_err(failed)?;
        match self.format {
            Format::Json => self.out.text(&line),
            _ => self.out.text(&format!("archived {} into {url} as {name} ({} entries, verified)\n", self.file, render::count(entries))),
        }
        Ok(Exit::Done)
    }

    pub(super) fn restore_from_bucket(&mut self, url: &str, at: Option<&str>) -> Result<Exit, Failure> {
        let target = self.request.file.clone();
        let (file, taken_at_ms) = restore_from_bucket_into(url, at, &target).map_err(|message| Failure::database(message, Exit::DatabaseError))?;
        match self.format {
            Format::Json => self.out.text(&JsonObject::new().string("restored", &file).string("taken_at", &rfc3339(taken_at_ms)).string("into", &self.file_display).line()),
            _ => self.out.text(&format!("restored {file} (taken {}) from {url} into {}\n", rfc3339(taken_at_ms), self.file)),
        }
        Ok(Exit::Done)
    }

    pub(super) fn restore_from_archive(&mut self, directory: &Path, at: Option<&str>) -> Result<Exit, Failure> {
        let source = archived_copy(directory, at).map_err(|message| Failure::database(message, Exit::DatabaseError))?;
        let chosen_file = source.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let taken_at_ms = manifest(directory)
            .ok()
            .and_then(|entries| entries.into_iter().find(|entry| entry.file == chosen_file).map(|entry| entry.taken_at_ms))
            .unwrap_or(0);
        let chosen = Archived { file: chosen_file, taken_at_ms, sha256: String::new() };
        let target = self.request.file.clone();
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err(Failure::coded("already_exists", format!("{} exists; restoring an archived copy writes a new file", self.file), Exit::DatabaseError));
        }
        let mut staging = target.clone().into_os_string();
        staging.push(STAGING_SUFFIX);
        let staging = PathBuf::from(staging);
        std::fs::copy(&source, &staging)
            .and_then(|_| std::fs::File::open(&staging)?.sync_all())
            .map_err(|error| Failure::database(format!("cannot copy {}: {error}", chosen.file), Exit::DatabaseError))?;
        std::fs::rename(&staging, &target).map_err(|error| Failure::database(format!("cannot move the restored copy into place: {error}"), Exit::DatabaseError))?;
        match self.format {
            Format::Json => self.out.text(&JsonObject::new().string("restored", &chosen.file).string("taken_at", &rfc3339(chosen.taken_at_ms)).string("into", &self.file_display).line()),
            _ => self.out.text(&format!("restored {} (taken {}) into {}\n", chosen.file, rfc3339(chosen.taken_at_ms), self.file)),
        }
        Ok(Exit::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_time, rfc3339};

    #[test]
    fn times_read_and_write_as_rfc_3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(parse_time("2026-10-31T12:30:15.250Z"), Some(1_793_449_815_250));
        assert_eq!(rfc3339(1_793_449_815_250), "2026-10-31T12:30:15.250Z");
        assert_eq!(parse_time("2026-10-31T14:30:15.25+02:00"), Some(1_793_449_815_250));
        assert_eq!(parse_time("1970-01-02"), Some(86_400_000));
        for bad in ["", "2026-13-01", "2026-10-31T25:00Z", "yesterday", "2026-10-31T12:00+2"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }
}
