use super::output::{Output, Style};
use super::render::{self, JsonObject, TimeStyle};
use eggdb::{BackupReport, CheckReport, CommitStamp, CompactReport, Stats};
use std::time::Duration;

const LABEL_WIDTH: usize = 16;
const DAMAGE_LABEL_WIDTH: usize = 18;
const REPAIR_ISSUES: &str = "https://github.com/eggspot/EggDB/issues";

pub(crate) struct Damage<'a> {
    pub(crate) file: &'a str,
    pub(crate) file_display: &'a str,
    pub(crate) checked_commit: Option<CommitStamp>,
    pub(crate) problem: &'a str,
}

pub(crate) fn check_text(out: &mut Output<'_>, file: &str, stats: &Stats, report: &CheckReport) {
    checked_line(out, file, Some(report.stamp));
    let page_size = render::page_size(stats.page_size);
    let file_line = format!(
        "{}, {} of {page_size}",
        render::size(stats.file_bytes),
        render::pages(stats.page_count)
    );
    label(out, "file", &file_line, Style::Bold);
    label(out, "entries", &render::count(report.entries), Style::Bold);
    label(
        out,
        "key bytes",
        &render::size(report.key_bytes),
        Style::Bold,
    );
    label(
        out,
        "value bytes",
        &render::size(report.value_bytes),
        Style::Bold,
    );
    let tree = format!(
        "height {}, {}",
        report.tree_height,
        render::pages(report.tree_pages)
    );
    label(out, "tree", &tree, Style::Bold);
    let overflow_pages = match report.overflow_pages {
        1 => "1 overflow page".to_owned(),
        n => format!("{} overflow pages", render::count(n)),
    };
    let large = format!(
        "{}, on {overflow_pages}",
        render::count(report.overflow_values)
    );
    label(out, "large values", &large, Style::Bold);
    label(
        out,
        "free pages",
        &render::count(report.free_pages),
        Style::Bold,
    );
    label(
        out,
        "log pages",
        &render::count(report.log_pages),
        Style::Bold,
    );
    let leaked = report.leaked_pages;
    if leaked == 0 {
        label(out, "leaked pages", "0", Style::Bold);
        verdict_ok(
            out,
            "every page passed its checksum, every key is in order, and every page of the file is accounted for.",
        );
    } else {
        let count = render::count(leaked);
        let value = format!(
            "{count} (harmless: used by nothing, never reused; eggdb compact will reclaim them)"
        );
        label(out, "leaked pages", &value, Style::Yellow);
        let pages = if leaked == 1 {
            "1 page is".to_owned()
        } else {
            format!("{count} pages are")
        };
        verdict_ok(
            out,
            &format!(
                "every page passed its checksum, every key is in order, and {pages} in use by nothing."
            ),
        );
    }
}

pub(crate) fn check_json(file: &str, stats: &Stats, report: &CheckReport) -> String {
    let (stamp, time) = stamp_fields(report.stamp);
    JsonObject::new()
        .boolean("ok", true)
        .string("file", file)
        .optional_string("commit_stamp", stamp.as_deref())
        .optional_string("commit_time", time.as_deref())
        .number("page_size", stats.page_size as u64)
        .number("pages", stats.page_count)
        .number("file_bytes", stats.file_bytes)
        .number("entries", report.entries)
        .number("key_bytes", report.key_bytes)
        .number("value_bytes", report.value_bytes)
        .number("tree_height", u64::from(report.tree_height))
        .number("tree_pages", report.tree_pages)
        .number("overflow_values", report.overflow_values)
        .number("overflow_pages", report.overflow_pages)
        .number("free_pages", report.free_pages)
        .number("log_pages", report.log_pages)
        .number("leaked_pages", report.leaked_pages)
        .line()
}

pub(crate) fn damage_text(out: &mut Output<'_>, damage: &Damage<'_>) {
    checked_line(out, damage.file, damage.checked_commit);
    out.styled(Style::Red, "damaged:");
    out.text(" ");
    out.text(damage.problem);
    out.text("\n");
    let checked = if damage.checked_commit.is_some() {
        "every page read before the damage passed its checksum; the check stopped at the damage"
    } else {
        "opening the file read its header, free list and log; it stopped at the damage"
    };
    damage_label(out, "what was checked", checked);
    let what_to_do = what_to_do_lines(damage.file);
    damage_label(out, "what to do", &what_to_do[0]);
    for line in &what_to_do[1..] {
        out.text(&" ".repeat(2 + DAMAGE_LABEL_WIDTH));
        out.text(line);
        out.text("\n");
    }
}

pub(crate) fn damage_json(damage: &Damage<'_>) -> String {
    let mut object = JsonObject::new()
        .boolean("ok", false)
        .string("file", damage.file_display)
        .string("problem", damage.problem);
    if let Some(page) = page_named_in(damage.problem) {
        object = object.number("page", page);
    }
    object
        .string(
            "what_to_do",
            &what_to_do_lines(damage.file_display).join(" "),
        )
        .line()
}

pub(crate) fn stats_text(out: &mut Output<'_>, file: &str, stats: &Stats) {
    out.text(file);
    out.text("\n");
    label(
        out,
        "page size",
        &render::page_size(stats.page_size),
        Style::Bold,
    );
    let pages = format!(
        "{} ({} free)",
        render::count(stats.page_count),
        render::count(stats.free_pages)
    );
    label(out, "pages", &pages, Style::Bold);
    label(
        out,
        "file size",
        &render::size(stats.file_bytes),
        Style::Bold,
    );
    label_start(out, "last commit", Style::Bold);
    commit(out, stats.last_commit);
    out.text("\n");
    let log_bytes = stats.log_bytes_since_checkpoint;
    let replay = if log_bytes == 0 {
        "the next open replays nothing"
    } else {
        "the next open replays it"
    };
    label(
        out,
        "log since checkpoint",
        &format!("{} ({replay})", render::size(log_bytes)),
        Style::Bold,
    );
}

pub(crate) fn stats_json(file: &str, stats: &Stats) -> String {
    let (stamp, time) = stamp_fields(stats.last_commit);
    JsonObject::new()
        .string("file", file)
        .number("page_size", stats.page_size as u64)
        .number("pages", stats.page_count)
        .number("free_pages", stats.free_pages)
        .number("file_bytes", stats.file_bytes)
        .optional_string("last_commit_stamp", stamp.as_deref())
        .optional_string("last_commit_time", time.as_deref())
        .number(
            "log_bytes_since_checkpoint",
            stats.log_bytes_since_checkpoint,
        )
        .line()
}

pub(crate) struct Copied<'a> {
    pub(crate) file: &'a str,
    pub(crate) copy: &'a str,
    pub(crate) page_size: usize,
    pub(crate) elapsed: Duration,
}

pub(crate) fn backup_text(out: &mut Output<'_>, copied: &Copied<'_>, report: &BackupReport) {
    out.text("backed up ");
    out.text(copied.file);
    out.text(" to ");
    out.text(copied.copy);
    out.text(" as of commit ");
    commit(out, report.stamp);
    out.text("\n");
    label(out, "entries", &render::count(report.entries), Style::Bold);
    let copy = format!(
        "{}, {} of {}",
        render::size(report.file_bytes),
        render::pages(report.page_count),
        render::page_size(copied.page_size)
    );
    label(out, "copy", &copy, Style::Bold);
    label(out, "time", &elapsed(copied.elapsed), Style::Bold);
    let counted = match report.entries {
        1 => "its 1 entry was counted again after writing and syncing.".to_owned(),
        n => format!(
            "its {} entries were counted again after writing and syncing.",
            render::count(n)
        ),
    };
    verdict_ok(out, &format!("{} is complete: {counted}", copied.copy));
}

pub(crate) fn backup_json(copied: &Copied<'_>, report: &BackupReport) -> String {
    let (stamp, time) = stamp_fields(report.stamp);
    JsonObject::new()
        .boolean("ok", true)
        .string("file", copied.file)
        .string("copy", copied.copy)
        .optional_string("commit_stamp", stamp.as_deref())
        .optional_string("commit_time", time.as_deref())
        .number("entries", report.entries)
        .number("page_size", copied.page_size as u64)
        .number("copy_pages", report.page_count)
        .number("copy_bytes", report.file_bytes)
        .number("elapsed_ms", elapsed_ms(copied.elapsed))
        .line()
}

pub(crate) fn compact_text(out: &mut Output<'_>, compacted: &Copied<'_>, report: &CompactReport) {
    out.text("compacted ");
    out.text(compacted.file);
    out.text(" as of commit ");
    commit(out, report.stamp);
    out.text("\n");
    label(out, "entries", &render::count(report.entries), Style::Bold);
    let before = format!(
        "{}, {} of {}",
        render::size(report.bytes_before),
        render::pages(report.pages_before),
        render::page_size(compacted.page_size)
    );
    label(out, "before", &before, Style::Bold);
    let after = format!(
        "{}, {}",
        render::size(report.bytes_after),
        render::pages(report.pages_after)
    );
    label(out, "after", &after, Style::Bold);
    let reclaimed = pages_reclaimed(report);
    let detail = format!(
        "{} ({} free, {} leaked)",
        render::pages(reclaimed),
        render::count(report.free_pages_before),
        render::count(report.leaked_pages_before)
    );
    label(out, "reclaimed", &detail, Style::Bold);
    label(out, "time", &elapsed(compacted.elapsed), Style::Bold);
    let verdict = if reclaimed == 0 {
        format!(
            "{} had no free or leaked pages; it was rewritten and swapped in whole all the same.",
            compacted.file
        )
    } else {
        format!(
            "{} was rewritten without free or leaked pages and swapped in whole; {} reclaimed.",
            compacted.file,
            render::pages(reclaimed)
        )
    };
    verdict_ok(out, &verdict);
}

pub(crate) fn compact_json(compacted: &Copied<'_>, report: &CompactReport) -> String {
    let (stamp, time) = stamp_fields(report.stamp);
    JsonObject::new()
        .boolean("ok", true)
        .string("file", compacted.file)
        .optional_string("commit_stamp", stamp.as_deref())
        .optional_string("commit_time", time.as_deref())
        .number("entries", report.entries)
        .number("page_size", compacted.page_size as u64)
        .number("pages_before", report.pages_before)
        .number("file_bytes_before", report.bytes_before)
        .number("pages_after", report.pages_after)
        .number("file_bytes_after", report.bytes_after)
        .number("free_pages_before", report.free_pages_before)
        .number("leaked_pages_before", report.leaked_pages_before)
        .number("pages_reclaimed", pages_reclaimed(report))
        .number("elapsed_ms", elapsed_ms(compacted.elapsed))
        .line()
}

fn pages_reclaimed(report: &CompactReport) -> u64 {
    report
        .free_pages_before
        .saturating_add(report.leaked_pages_before)
}

fn elapsed(time: Duration) -> String {
    let millis = elapsed_ms(time);
    if millis < 1000 {
        format!("{millis} ms")
    } else {
        format!("{}.{} s", millis / 1000, millis % 1000 / 100)
    }
}

fn elapsed_ms(time: Duration) -> u64 {
    u64::try_from(time.as_millis()).unwrap_or(u64::MAX)
}

fn checked_line(out: &mut Output<'_>, file: &str, stamp: Option<CommitStamp>) {
    out.text("checked ");
    out.text(file);
    if let Some(stamp) = stamp {
        out.text(" as of commit ");
        commit(out, stamp);
    }
    out.text("\n");
}

fn commit(out: &mut Output<'_>, stamp: CommitStamp) {
    if stamp == CommitStamp::NONE {
        out.text("none");
        return;
    }
    out.text(&render::utc_time(stamp.unix_micros(), TimeStyle::Readable));
    out.text(" ");
    out.styled(Style::Dim, &format!("(stamp {stamp})"));
}

fn stamp_fields(stamp: CommitStamp) -> (Option<String>, Option<String>) {
    if stamp == CommitStamp::NONE {
        return (None, None);
    }
    (
        Some(stamp.to_string()),
        Some(render::utc_time(stamp.unix_micros(), TimeStyle::Rfc3339)),
    )
}

fn verdict_ok(out: &mut Output<'_>, text: &str) {
    out.styled(Style::Green, "ok:");
    out.text(" ");
    out.text(text);
    out.text("\n");
}

pub(crate) fn label(out: &mut Output<'_>, name: &str, value: &str, style: Style) {
    label_start(out, name, style);
    out.text(value);
    out.text("\n");
}

fn label_start(out: &mut Output<'_>, name: &str, style: Style) {
    padded_label(out, name, style, LABEL_WIDTH);
}

fn damage_label(out: &mut Output<'_>, name: &str, value: &str) {
    padded_label(out, name, Style::Bold, DAMAGE_LABEL_WIDTH);
    out.text(value);
    out.text("\n");
}

fn padded_label(out: &mut Output<'_>, name: &str, style: Style, width: usize) {
    out.text("  ");
    out.styled(style, name);
    let padding = width.saturating_sub(name.len()).max(2);
    out.text(&" ".repeat(padding));
}

fn what_to_do_lines(file: &str) -> [String; 3] {
    [
        format!("EggDB does not repair files yet. Restore {file} from your latest backup."),
        "Keep the damaged file unchanged: `eggdb scan` may still read part of the data,".to_owned(),
        format!("and an issue at {REPAIR_ISSUES} with this output helps us."),
    ]
}

pub(crate) fn page_named_in(problem: &str) -> Option<u64> {
    problem.match_indices("page ").find_map(|(at, word)| {
        let rest = &problem[at + word.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        rest[..digits].parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::{elapsed, page_named_in};
    use std::time::Duration;

    #[test]
    fn times_under_a_second_are_milliseconds_and_longer_ones_tenths_of_seconds() {
        assert_eq!(elapsed(Duration::from_micros(412_900)), "412 ms");
        assert_eq!(elapsed(Duration::from_millis(999)), "999 ms");
        assert_eq!(elapsed(Duration::from_millis(1000)), "1.0 s");
        assert_eq!(elapsed(Duration::from_millis(2_349)), "2.3 s");
        assert_eq!(elapsed(Duration::from_secs(75)), "75.0 s");
    }

    #[test]
    fn the_damaged_page_is_found_in_the_message() {
        assert_eq!(
            page_named_in("the B+tree page 77 is damaged: a key is out of order"),
            Some(77)
        );
        assert_eq!(
            page_named_in("page 5 failed its checksum: it is damaged"),
            Some(5)
        );
        assert_eq!(
            page_named_in("the file is cut off: its header says 8192 bytes"),
            None
        );
        assert_eq!(page_named_in("a page size of x"), None);
    }
}
