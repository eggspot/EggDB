use crate::args;
use crate::database::{EggdbDb, EggdbOptions, open_options};
use crate::status::{Failure, guarded};
use eggdb::{CheckReport, Database, Options, Stats};
use std::ffi::{c_char, c_int};
use std::sync::Arc;
use std::time::Duration;

/// What [`eggdb_check`] found (`eggdb_check_report` in `eggdb.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EggdbCheckReport {
    /// The stamp of the last commit checked.
    pub stamp: u64,
    /// The entries in the tree.
    pub entries: u64,
    /// The bytes of every key.
    pub key_bytes: u64,
    /// The bytes of every value.
    pub value_bytes: u64,
    /// The tree's height.
    pub tree_height: u32,
    /// The tree's inner and leaf pages.
    pub tree_pages: u64,
    /// The values stored on overflow pages.
    pub overflow_values: u64,
    /// The overflow pages.
    pub overflow_pages: u64,
    /// The free pages.
    pub free_pages: u64,
    /// The log's pages.
    pub log_pages: u64,
    /// Pages nothing reaches; `eggdb_compact` gives them back.
    pub leaked_pages: u64,
}

/// What [`eggdb_stats`] reports (`eggdb_stats` in `eggdb.h`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EggdbStats {
    /// The page size in bytes.
    pub page_size: u64,
    /// The file's pages.
    pub page_count: u64,
    /// Its free pages.
    pub free_pages: u64,
    /// Its size in bytes.
    pub file_bytes: u64,
    /// The stamp of the last commit.
    pub last_commit: u64,
    /// The page cache's frames.
    pub cache_pages: u64,
    /// The frames holding a page.
    pub cached_pages: u64,
    /// Page reads the cache answered.
    pub cache_hits: u64,
    /// Page reads that went to the file.
    pub cache_misses: u64,
    /// The log's bytes since the last checkpoint.
    pub log_bytes_since_checkpoint: u64,
    /// 1 when the file has held keys with a time to live, else 0.
    pub expiring_keys: u32,
}

impl From<CheckReport> for EggdbCheckReport {
    fn from(report: CheckReport) -> Self {
        Self {
            stamp: report.stamp.bits(),
            entries: report.entries,
            key_bytes: report.key_bytes,
            value_bytes: report.value_bytes,
            tree_height: report.tree_height,
            tree_pages: report.tree_pages,
            overflow_values: report.overflow_values,
            overflow_pages: report.overflow_pages,
            free_pages: report.free_pages,
            log_pages: report.log_pages,
            leaked_pages: report.leaked_pages,
        }
    }
}

impl From<Stats> for EggdbStats {
    fn from(stats: Stats) -> Self {
        Self {
            page_size: stats.page_size as u64,
            page_count: stats.page_count,
            free_pages: stats.free_pages,
            file_bytes: stats.file_bytes,
            last_commit: stats.last_commit.bits(),
            cache_pages: stats.cache_pages as u64,
            cached_pages: stats.cached_pages as u64,
            cache_hits: stats.cache_hits,
            cache_misses: stats.cache_misses,
            log_bytes_since_checkpoint: stats.log_bytes_since_checkpoint,
            expiring_keys: u32::from(stats.expiring_keys),
        }
    }
}

/// Checks the whole file as `eggdb check` does: every page's checksum, the tree's order and
/// shape, and that no page is lost; `*report` is filled when it is not NULL (D1146). A damaged
/// file fails with `EGGDB_ERR_CORRUPT`.
///
/// # Safety
/// `db` is a handle from `eggdb_open`, and `report` NULL or writable memory for an
/// `eggdb_check_report`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_check(db: *mut EggdbDb, report: *mut EggdbCheckReport) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live handle from eggdb_open.
        let handle = unsafe { args::handle(db, "db") }?;
        let checked = handle.database.check()?;
        // SAFETY: eggdb.h asks for NULL or writable memory for one report.
        unsafe { args::write_if_wanted(report, EggdbCheckReport::from(checked)) };
        Ok(())
    })
}

/// The file's size and free space, the last commit, and what the page cache and the log have
/// done since the database opened, without reading a page (D1146).
///
/// # Safety
/// `db` is a handle from `eggdb_open`, and `stats` writable memory for an `eggdb_stats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_stats(db: *mut EggdbDb, stats: *mut EggdbStats) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for a live handle from eggdb_open.
        let handle = unsafe { args::handle(db, "db") }?;
        let measured = handle.database.stats()?;
        // SAFETY: eggdb.h asks for writable memory for one stats record.
        unsafe { args::write(stats, EggdbStats::from(measured), "stats") }
    })
}

fn number(name: &str, value: &str) -> Result<u64, Failure> {
    value.trim().parse::<u64>().map_err(|_| {
        Failure::invalid_argument(format!("setting {name} takes a whole number, not '{value}'"))
    })
}

fn switch(name: &str, value: &str) -> Result<bool, Failure> {
    match value.trim() {
        "1" | "on" | "true" | "yes" => Ok(true),
        "0" | "off" | "false" | "no" => Ok(false),
        other => Err(Failure::invalid_argument(format!("setting {name} takes on or off, not '{other}'"))),
    }
}

fn address(name: &str, value: &str) -> Result<usize, Failure> {
    usize::try_from(number(name, value)?).map_err(|_| {
        Failure::invalid_argument(format!("setting {name} is more than this computer can address"))
    })
}

/// Applies `name=value` settings, separated by `;` or new lines, to `options`: the open options
/// `eggdb_options` has no field for, so new ones never change its layout (D1146).
pub(crate) fn with_settings(mut options: Options, settings: &str) -> Result<Options, Failure> {
    for setting in settings.split([';', '\n']).map(str::trim).filter(|setting| !setting.is_empty()) {
        let Some((name, value)) = setting.split_once('=') else {
            return Err(Failure::invalid_argument(format!("setting '{setting}' has no '='")));
        };
        let name = name.trim();
        options = match name {
            "lock_cache" => options.lock_cache(switch(name, value)?),
            "compress_values" => options.compress_values(switch(name, value)?),
            "cache_numa_node" => options.cache_numa_node(Some(u32::try_from(number(name, value)?).unwrap_or(u32::MAX))),
            "memory_limit_bytes" => options.memory_limit(address(name, value)?),
            "snapshot_memory_bytes" => options.snapshot_memory_bytes(address(name, value)?),
            "auto_checkpoint_log_bytes" => {
                let bytes = number(name, value)?;
                options.auto_checkpoint_log_bytes((bytes != 0).then_some(bytes))
            }
            "page_size_bytes" => options.page_size_bytes(address(name, value)?),
            "write_timeout_ms" => options.write_timeout(Duration::from_millis(number(name, value)?)),
            "busy_timeout_ms" => options.busy_timeout(Duration::from_millis(number(name, value)?)),
            "page_store_disk_bytes" => options.page_store_disk_bytes(number(name, value)?),
            other => {
                return Err(Failure::invalid_argument(format!(
                    "unknown setting '{other}'; known: lock_cache, compress_values, cache_numa_node, memory_limit_bytes, snapshot_memory_bytes, auto_checkpoint_log_bytes, page_size_bytes, write_timeout_ms, busy_timeout_ms, page_store_disk_bytes"
                )));
            }
        };
    }
    Ok(options)
}

/// Opens a database as [`crate::eggdb_open`] does, with `settings`, NULL or a NUL-terminated
/// list of `name=value` pairs, for the options `eggdb_options` has no field for (D1146).
///
/// # Safety
/// As for `eggdb_open`, and `settings` NULL or a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn eggdb_open_v2(
    path: *const c_char,
    options: *const EggdbOptions,
    settings: *const c_char,
    db: *mut *mut EggdbDb,
) -> c_int {
    guarded(|| {
        // SAFETY: eggdb.h asks for writable memory for one handle at `db`.
        unsafe { args::write(db, std::ptr::null_mut(), "db") }?;
        // SAFETY: eggdb.h asks for a NUL-terminated path.
        let path = unsafe { args::text(path, "path") }?;
        // SAFETY: eggdb.h asks for NULL or an `eggdb_options`.
        let base = open_options(unsafe { args::optional_struct(options) })?;
        let opened = if settings.is_null() {
            base
        } else {
            // SAFETY: eggdb.h asks for NULL or a NUL-terminated settings string.
            with_settings(base, unsafe { args::text(settings, "settings") }?)?
        };
        let database = if path == ":memory:" {
            Database::in_memory_with(opened)?
        } else {
            Database::open_with(path, opened)?
        };
        let handle = args::give(EggdbDb {
            database: Arc::new(database),
        });
        // SAFETY: as for the first write to `db`.
        unsafe { args::write(db, handle, "db") }
    })
}

#[cfg(test)]
mod tests {
    use super::with_settings;
    use eggdb::Options;

    #[test]
    fn settings_parse_by_name_and_refuse_what_they_do_not_know() {
        assert!(with_settings(Options::new(), "lock_cache=off; snapshot_memory_bytes=1048576\nwrite_timeout_ms=250").is_ok());
        assert!(with_settings(Options::new(), "unknown=1").is_err());
        assert!(with_settings(Options::new(), "lock_cache").is_err());
        assert!(with_settings(Options::new(), "compress_values=maybe").is_err());
    }
}
