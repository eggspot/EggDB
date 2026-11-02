## DNEW-R-31b Time zone abbreviations from PostgreSQL's `Default` set, POSIX rule strings as an inline zone, infinite time stamps and dates as PostgreSQL's sentinels — Decided by the build loop (by judgement and PostgreSQL's documentation, 2026-11-02); roadmap item 31b, refines D153

D153 left time zone abbreviations, POSIX rule strings and infinite time stamps to later. They
are built here.

**Abbreviations are PostgreSQL's `Default` set, as data.** The 194 entries of PostgreSQL's
`timezonesets/Default` file (facts: an abbreviation, an offset and a daylight flag, or the
region whose history an abbreviation follows) are a sorted table in
`eggdb-sql/src/time_zones/abbreviations.rs`. Time stamp input and `AT TIME ZONE` look an
abbreviation up before a region name, as PostgreSQL does; the `TimeZone` setting does not
accept them (`22023`), as PostgreSQL does not. An abbreviation that names a region (`MSK`,
`ART`) uses that region's offset at the time in question, which equals PostgreSQL's answer
whenever the region used the abbreviation then, and is the region's own offset otherwise.
PostgreSQL's other sets (`Australia`, `India`) are not offered.

**POSIX rule strings are an inline zone.** `TimeZone` and `AT TIME ZONE` read a POSIX `TZ`
string when the name is neither an offset nor a region (`EST5EDT,M3.2.0,M11.1.0`, `<+03>-3`),
with `Mm.w.d`, `Jn` and `n` day rules, an optional `/time` up to 167 hours, a daylight offset
one hour ahead by default and the United States' rules when none are written (PostgreSQL's
documented default). The zone is a small `Copy` value inside `TimeZone` (abbreviations of 3 to
10 characters), so a session can name any number of them without the process keeping them; the
offsets in force are computed from the rules of the years around each instant.

**Infinite time stamps and dates use PostgreSQL's sentinels, with no format change.** `infinity`
is `i64::MAX` microseconds and `-infinity` `i64::MIN` for `timestamp` and `timestamptz`, and
`i32::MAX`/`i32::MIN` days for `date`: the stored forms are unchanged, and only the decoder's
validity check widens to accept the two sentinels, as D153 did for `TIME` (an older binary
refuses a file holding one as damaged, never misreads it). They order after and before every
finite value by the existing comparisons and keys, and a date sentinel compares equal to the
time stamp sentinel. Arithmetic follows PostgreSQL 17: a finite interval or day count keeps an
infinity, an infinite interval makes a finite time stamp infinite, opposite infinities fail with
`22008`, a difference involving an infinity is an infinite interval (equal infinities fail), and
subtracting infinite dates fails. `to_char` returns NULL. Left out: `extract`, `date_part` and
`date_trunc` on infinite values (PostgreSQL returns `±Infinity` or NULL), casting an infinite time
stamp to `time` (NULL in PostgreSQL), and infinite values in the SQL Server and Oracle dialects'
own functions.
