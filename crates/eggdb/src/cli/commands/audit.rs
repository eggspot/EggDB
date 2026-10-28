use super::Session;
use crate::cli::args::Format;
use crate::cli::failure::{Exit, Failure};
use crate::cli::output::Style;
use crate::cli::render;
use eggdb_mcp::audit::{
    AuditEntry, AuditEvent, AuditLine, AuditTotals, audit_path_for, read_audit,
};

const LINE_COLUMNS: usize = 120;
const TIME_COLUMNS: usize = 23;
const TOOL_COLUMNS: usize = 14;
const OUTCOME_COLUMNS: usize = 17;
const ROWS_COLUMNS: usize = 5;

impl Session<'_> {
    pub(super) fn audit(&mut self) -> Result<Exit, Failure> {
        let path = audit_path_for(&self.request.file);
        let shown = render::text(path.display().to_string().as_bytes());
        let limit = self
            .request
            .scan
            .limit
            .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
        let json = self.format == Format::Json;
        let mut shown_calls = 0u64;
        let mut shown_lines = 0u64;
        let mut printed = Ok(());
        let read = read_audit(&path, limit, &mut |line| {
            shown_lines += 1;
            match (&line, json) {
                (AuditLine::Record { raw, .. }, true) => {
                    self.out.text(raw);
                    self.out.text("\n");
                }
                (AuditLine::Unreadable { number }, true) => self.messages.text(&format!(
                    "eggdb: {shown} line {number} is not a record; skipped\n"
                )),
                (_, false) => {
                    if matches!(&line, AuditLine::Record { entry, .. } if entry.event == AuditEvent::Call)
                    {
                        shown_calls += 1;
                    }
                    self.audit_line(&line);
                }
            }
            printed = self.out.line_done();
            printed.is_ok()
        });
        printed?;
        let totals = read.map_err(|error| {
            let reason = match error.kind() {
                std::io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
                _ => error.to_string(),
            };
            Failure::database(
                format!("cannot read {shown}: {reason}"),
                Exit::DatabaseError,
            )
        })?;
        let Some(totals) = totals else {
            if std::fs::symlink_metadata(&self.request.file).is_err() {
                return Err(Failure::database(
                    format!("cannot open {}: no such file. Check the path.", self.file),
                    Exit::DatabaseError,
                ));
            }
            self.messages
                .text(&format!("no MCP calls yet ({shown} does not exist)\n"));
            return Ok(Exit::Done);
        };
        if json {
            return Ok(Exit::Done);
        }
        if shown_lines == 0 && totals.calls == 0 && totals.unreadable == 0 {
            self.messages.text("no MCP calls yet\n");
            return Ok(Exit::Done);
        }
        self.messages
            .styled(Style::Dim, &summary(&totals, shown_calls));
        self.messages.text("\n");
        Ok(Exit::Done)
    }

    fn audit_line(&mut self, line: &AuditLine) {
        let entry = match line {
            AuditLine::Unreadable { number } => {
                let place = format!("line {number}");
                self.out.text(&format!(
                    "{place:<TIME_COLUMNS$}  unreadable: not a JSON object; skipped\n"
                ));
                return;
            }
            AuditLine::Record { entry, .. } => entry,
        };
        let time = shown_time(&entry.time);
        match entry.event {
            AuditEvent::ServerStarted => {
                let sentence = format!(
                    "{time}  server started: eggdb {}, pid {}, read-only, time limit {} s",
                    printable(entry.version.as_deref().unwrap_or("?")),
                    entry.pid.unwrap_or(0),
                    entry.time_limit_ms.unwrap_or(0) / 1000
                );
                self.out.styled(Style::Dim, &cut(&sentence));
                self.out.text("\n");
            }
            AuditEvent::TimeLimitExceeded => {
                let sentence = format!(
                    "{time}  time limit exceeded: {} ran a second past its limit; the server exited",
                    printable(entry.tool.as_deref().unwrap_or("a call"))
                );
                self.out.styled(Style::Dim, &cut(&sentence));
                self.out.text("\n");
            }
            AuditEvent::Call => self.call_line(&time, entry),
        }
    }

    fn call_line(&mut self, time: &str, entry: &AuditEntry) {
        let duration = format!("{:>6} ms", entry.duration_ms.unwrap_or(0));
        let tool = printable(entry.tool.as_deref().unwrap_or("?"));
        let outcome = match (&entry.error_code, &entry.sqlstate) {
            (None, _) => "ok".to_owned(),
            (Some(code), Some(state)) => format!("{} {}", printable(code), printable(state)),
            (Some(code), None) => printable(code),
        };
        let rows = match (&entry.error_code, entry.rows) {
            (Some(_), _) | (None, None) => "-".to_owned(),
            (None, Some(rows)) if entry.truncated => format!("{rows}+"),
            (None, Some(rows)) => rows.to_string(),
        };
        let arguments = printable(entry.arguments.as_deref().unwrap_or(""));
        let head = format!("{time}  {duration}  ");
        let tool_cell = format!("{tool:<TOOL_COLUMNS$}");
        let outcome_cell = format!("{outcome:<OUTCOME_COLUMNS$}");
        let tail = format!("  {rows:>ROWS_COLUMNS$}  {arguments}");
        let used =
            head.chars().count() + tool_cell.chars().count() + 2 + outcome_cell.chars().count();
        let room = LINE_COLUMNS.saturating_sub(used);
        self.out.text(&head);
        self.out.styled(Style::Bold, &tool_cell);
        self.out.text("  ");
        if entry.error_code.is_some() {
            self.out.styled(Style::Red, &outcome_cell);
        } else {
            self.out.text(&outcome_cell);
        }
        self.out.text(&cut_to(&tail, room));
        self.out.text("\n");
    }
}

fn shown_time(time: &str) -> String {
    let shown = match (time.get(..10), time.get(11..23)) {
        (Some(date), Some(clock)) => format!("{date} {clock}"),
        _ => time.to_owned(),
    };
    printable(&shown)
}

fn is_hidden_or_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

fn printable(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' | '\r' | '\t' => shown.push(' '),
            hidden if is_hidden_or_control(hidden) => {
                shown.push_str(&format!("\\u{:04x}", u32::from(hidden)));
            }
            other => shown.push(other),
        }
    }
    shown
}

fn cut(line: &str) -> String {
    cut_to(line, LINE_COLUMNS)
}

fn cut_to(text: &str, columns: usize) -> String {
    if text.chars().count() <= columns {
        return text.to_owned();
    }
    let mut kept: String = text.chars().take(columns.saturating_sub(3)).collect();
    kept.push_str("...");
    kept
}

fn plural(count: u64, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

fn summary(totals: &AuditTotals, shown_calls: u64) -> String {
    let mut parts = Vec::new();
    if shown_calls < totals.calls {
        parts.push(format!(
            "last {shown_calls} of {}",
            plural(totals.calls, "call", "calls")
        ));
    } else {
        parts.push(plural(totals.calls, "call", "calls"));
    }
    parts.push(match totals.errors {
        0 => "no errors".to_owned(),
        errors => plural(errors, "error", "errors"),
    });
    if totals.unreadable > 0 {
        parts.push(plural(
            totals.unreadable,
            "unreadable line",
            "unreadable lines",
        ));
    }
    let mut summary = parts.join(", ");
    if !totals.clients.is_empty() {
        let clients: Vec<String> = totals
            .clients
            .iter()
            .map(|client| printable(client))
            .collect();
        let named = match clients.as_slice() {
            [only] => format!("{only} (the name the client gave)"),
            [rest @ .., last] => format!(
                "{} and {last} (the names the clients gave)",
                rest.join(", ")
            ),
            [] => String::new(),
        };
        summary.push_str(", by ");
        summary.push_str(&named);
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::{cut_to, printable, shown_time};

    #[test]
    fn hidden_and_control_characters_are_shown_escaped() {
        for hidden in [
            '\u{7}',
            '\u{1b}',
            '\u{85}',
            '\u{9b}',
            '\u{200b}',
            '\u{200d}',
            '\u{2060}',
            '\u{2064}',
            '\u{feff}',
            '\u{e0041}',
            '\u{e007f}',
            '\u{202e}',
            '\u{2066}',
        ] {
            let shown = printable(&format!("a{hidden}b"));
            assert!(!shown.contains(hidden), "{hidden:?}");
            assert!(shown.is_ascii(), "{shown}");
        }
        let time = shown_time("\u{1b}[2J\u{1b}]0;pwned\u{7}XXXXXXXXXX");
        assert!(
            time.is_ascii() && !time.contains('\u{1b}') && time.contains("\\u001b"),
            "{time}"
        );
    }

    #[test]
    fn logged_text_cannot_drive_the_terminal() {
        assert_eq!(
            printable("SELECT 1\n\tFROM t\u{1b}[2J\u{202e}x"),
            "SELECT 1  FROM t\\u001b[2J\\u202ex"
        );
        assert_eq!(
            shown_time("2026-10-28T09:14:07.123456Z"),
            "2026-10-28 09:14:07.123"
        );
        assert_eq!(cut_to("abcdef", 5), "ab...");
        assert_eq!(cut_to("abc", 5), "abc");
    }
}
