use super::{Access, Session, cache_for_value};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject, TimeStyle};
use eggdb::{Database, ErrorKind};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SHOWN_VALUE_BYTES: usize = 64;

struct Swap {
    found: Option<Vec<u8>>,
    swapped: bool,
}

impl Session<'_> {
    pub(super) fn ttl(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let db = self.open(Access::ReadOnly, None)?;
        let collection = self.request.collection.clone();
        let looked = db.read(|transaction| match &collection {
            Some(name) => {
                let named = transaction.collection(name)?;
                let present = named.get_with(&key, |_| ())?.is_some();
                Ok((present, named.time_to_live(&key)?))
            }
            None => {
                let present = transaction.get_with(&key, |_| ())?.is_some();
                Ok((present, transaction.time_to_live(&key)?))
            }
        });
        let (present, left) = looked.map_err(|error| {
            self.key_space_failure(
                &error,
                &format!("cannot read {} from {}", render::text(&key), self.file),
            )
        })?;
        drop(db);
        if !present {
            if self.format != Format::Json {
                self.error_message(&format!("key not found: {}", render::text(&key)));
            }
            return Ok(Exit::NotFound);
        }
        match self.format {
            Format::Json => self
                .out
                .text(&with_time_to_live(JsonObject::new().bytes("key", &key), left).line()),
            _ => {
                let shown = left.map_or_else(|| "never".to_owned(), eggdb_ui::format_time_left);
                self.out.text(&shown);
                self.out.text("\n");
            }
        }
        Ok(Exit::Done)
    }

    pub(super) fn expire(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let ttl = self.request.ttl.unwrap_or(Duration::from_secs(1));
        let db = self.open(Access::Write { create: false }, None)?;
        let expires = match &self.request.collection {
            Some(name) => db.collection(name).set_expiry(&key, ttl),
            None => db.set_expiry(&key, ttl),
        };
        let expires = expires.map_err(|error| self.change_failure(&error, &key))?;
        self.close(db)?;
        if self.format == Format::Json {
            let object = JsonObject::new()
                .bytes("key", &key)
                .boolean("expires", expires);
            let object = if expires {
                with_time_to_live(object, Some(ttl))
            } else {
                object
            };
            self.out.text(&object.line());
        } else if !expires {
            self.error_message(&format!(
                "nothing to expire: {} is not in {}",
                render::text(&key),
                self.file
            ));
        }
        Ok(if expires { Exit::Done } else { Exit::NotFound })
    }

    pub(super) fn persist(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let db = self.open(Access::Write { create: false }, None)?;
        let present = persist_in(&db, self.request.collection.as_deref(), &key)
            .map_err(|error| self.change_failure(&error, &key))?;
        self.close(db)?;
        if self.format == Format::Json {
            self.out.text(
                &JsonObject::new()
                    .bytes("key", &key)
                    .boolean("persisted", present)
                    .line(),
            );
        } else if !present {
            self.error_message(&format!(
                "nothing to persist: {} is not in {}",
                render::text(&key),
                self.file
            ));
        }
        Ok(if present { Exit::Done } else { Exit::NotFound })
    }

    pub(super) fn counter(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let step = self.request.step.unwrap_or(1);
        let magnitude = i64::try_from(step).unwrap_or(i64::MAX);
        let decrement = self.request.command == CommandName::Decr;
        let by = if decrement { -magnitude } else { magnitude };
        let db = self.open(Access::Write { create: false }, None)?;
        let counted = match &self.request.collection {
            Some(name) => db.collection(name).increment(&key, by),
            None => db.increment(&key, by),
        };
        let count = match counted {
            Ok(count) => count,
            Err(error) if error.kind() == ErrorKind::NotAnInteger => {
                let current = match &self.request.collection {
                    Some(name) => db.collection(name).get(&key),
                    None => db.get(&key),
                };
                let current = current.ok().flatten().unwrap_or_default();
                return Err(self.not_a_counter(&key, &current, step, decrement));
            }
            Err(error) => return Err(self.change_failure(&error, &key)),
        };
        self.close(db)?;
        let value = count.to_string();
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .bytes("key", &key)
                    .string("value", &value)
                    .line(),
            ),
            _ => {
                self.out.text(&value);
                self.out.text("\n");
            }
        }
        Ok(Exit::Done)
    }

    pub(super) fn compare_and_set(&mut self) -> Result<Exit, Failure> {
        let key = self.resolve(self.request.key.clone())?;
        let expected = match self.request.swap.expected.clone() {
            Some(argument) => Some(self.resolve(Some(argument))?),
            None => None,
        };
        let new = match self.request.value.clone() {
            Some(argument) => Some(self.resolve(Some(argument))?),
            None => None,
        };
        let cache = cache_for_value(new.as_ref().map_or(0, Vec::len));
        let db = self.open(Access::Write { create: false }, Some(cache))?;
        let swap = swap_in(
            &db,
            self.request.collection.as_deref(),
            &key,
            expected.as_deref(),
            new.as_deref(),
        )
        .map_err(|error| self.change_failure(&error, &key))?;
        self.close(db)?;
        if self.format == Format::Json {
            let object = JsonObject::new()
                .bytes("key", &key)
                .boolean("swapped", swap.swapped);
            let object = match (&swap.found, swap.swapped) {
                (Some(found), false) => object.bytes("value", found),
                _ => object,
            };
            self.out.text(&object.line());
        } else if !swap.swapped {
            let message = self.not_swapped(&key, swap.found.as_deref(), expected.as_deref());
            self.error_message(&message);
        }
        Ok(if swap.swapped {
            Exit::Done
        } else {
            Exit::NotFound
        })
    }

    fn not_swapped(&self, key: &[u8], found: Option<&[u8]>, expected: Option<&[u8]>) -> String {
        let key = render::text(key);
        let file = &self.file;
        match (found, expected) {
            (None, _) => format!("not swapped: {key} is not in {file}. Nothing changed."),
            (Some(found), None) => format!(
                "not swapped: {key} is already in {file} (holding {}). Nothing changed.",
                shown_value(found)
            ),
            (Some(found), Some(expected)) => format!(
                "not swapped: {key} in {file} holds {}, not {}. Nothing changed.",
                shown_value(found),
                shown_value(expected)
            ),
        }
    }

    fn not_a_counter(&self, key: &[u8], current: &[u8], step: u64, decrement: bool) -> Failure {
        let (verb, sign) = if decrement {
            ("decrement", '-')
        } else {
            ("increment", '+')
        };
        let shown_key = render::text(key);
        let file = &self.file;
        let range = format!("{} to {}", i64::MIN, i64::MAX);
        if is_counter(current) {
            return Failure::database(
                format!(
                    "cannot {verb} {shown_key} in {file}: {} {sign} {step} is outside {range}. Nothing changed.",
                    render::text(current)
                ),
                Exit::DatabaseError,
            );
        }
        let in_collection = self
            .request
            .collection
            .as_ref()
            .map_or_else(String::new, |name| format!(" -C {name}"));
        let start = format!("eggdb put {}{in_collection} {shown_key} 0", self.file);
        Failure::Database {
            message: format!(
                "cannot {verb} {shown_key} in {file}: its value is not a whole number ({}). Counters are whole numbers from {range}, stored as text; start one with `{start}`",
                shown_value(current)
            ),
            try_command: Some(start),
            exit: Exit::DatabaseError,
            code: "database_failed",
        }
    }

    fn change_failure(&self, error: &eggdb::Error, key: &[u8]) -> Failure {
        self.key_space_failure(
            error,
            &format!("cannot change {} in {}", render::text(key), self.file),
        )
    }
}

fn persist_in(db: &Database, collection: Option<&str>, key: &[u8]) -> eggdb::Result<bool> {
    let mut transaction = db.begin_write()?;
    let (present, cleared) = match collection {
        Some(name) => {
            let mut named = transaction.collection(name)?;
            let present = named.get_with(key, |_| ())?.is_some();
            (present, named.clear_expiry(key)?)
        }
        None => {
            let present = transaction.get_with(key, |_| ())?.is_some();
            (present, transaction.clear_expiry(key)?)
        }
    };
    if cleared {
        transaction.commit()?;
    } else {
        transaction.rollback()?;
    }
    Ok(present)
}

fn swap_in(
    db: &Database,
    collection: Option<&str>,
    key: &[u8],
    expected: Option<&[u8]>,
    new: Option<&[u8]>,
) -> eggdb::Result<Swap> {
    let mut transaction = db.begin_write()?;
    let swap = match collection {
        Some(name) => {
            let mut named = transaction.collection(name)?;
            let found = named.get(key)?;
            let swapped = named.compare_and_set(key, expected, new)?;
            Swap { found, swapped }
        }
        None => {
            let found = transaction.get(key)?;
            let swapped = transaction.compare_and_set(key, expected, new)?;
            Swap { found, swapped }
        }
    };
    if swap.swapped {
        transaction.commit()?;
    } else {
        transaction.rollback()?;
    }
    Ok(swap)
}

fn is_counter(value: &[u8]) -> bool {
    let digits = value.strip_prefix(b"-").unwrap_or(value);
    !digits.is_empty()
        && digits.iter().all(u8::is_ascii_digit)
        && (digits.len() == 1 || digits[0] != b'0')
        && value != b"-0"
        && std::str::from_utf8(value)
            .ok()
            .and_then(|text| text.parse::<i64>().ok())
            .is_some()
}

fn shown_value(value: &[u8]) -> String {
    match render::text_cut(value, SHOWN_VALUE_BYTES) {
        Some(cut) => format!("{cut} … ({} bytes)", render::count(value.len() as u64)),
        None => render::text(value),
    }
}

pub(super) fn with_time_to_live(object: JsonObject, left: Option<Duration>) -> JsonObject {
    match left {
        Some(left) => object
            .number(
                "ttl_ms",
                u64::try_from(left.as_millis()).unwrap_or(u64::MAX),
            )
            .string("expires_at", &expires_at(left)),
        None => object.raw("ttl_ms", "null").raw("expires_at", "null"),
    }
}

fn expires_at(left: Duration) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_micros()).unwrap_or(u64::MAX));
    let left = u64::try_from(left.as_micros()).unwrap_or(u64::MAX);
    let micros = render::utc_time(now.saturating_add(left), TimeStyle::Rfc3339);
    let without_micros = micros
        .strip_suffix('Z')
        .and_then(|time| time.get(..time.len().saturating_sub(3)))
        .unwrap_or(&micros);
    format!("{without_micros}Z")
}

#[cfg(test)]
mod tests {
    use super::{expires_at, is_counter, shown_value};
    use std::time::Duration;

    #[test]
    fn an_expiry_time_is_utc_with_milliseconds() {
        let at = expires_at(Duration::from_secs(60));
        assert_eq!(at.len(), "2026-10-30T09:44:07.123Z".len(), "{at}");
        assert!(at.ends_with('Z') && at.as_bytes()[10] == b'T', "{at}");
        assert_eq!(at.as_bytes()[19], b'.');
    }

    #[test]
    fn counters_are_recognised_as_the_engine_reads_them() {
        for good in ["0", "41", "-7", "9223372036854775807"] {
            assert!(is_counter(good.as_bytes()), "{good}");
        }
        for bad in ["", "Ada", "01", "-0", "+1", "9223372036854775808"] {
            assert!(!is_counter(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn long_values_are_cut_at_64_bytes() {
        assert_eq!(shown_value(b"worker-7"), "worker-7");
        let long = "x".repeat(100);
        assert_eq!(
            shown_value(long.as_bytes()),
            format!("{} … (100 bytes)", "x".repeat(64))
        );
    }
}
