use crate::json_out::JsonOut;
use eggdb_sql::DestructiveStatement;

const MOST_SHOWN_SQL_CHARACTERS: usize = 2000;
const MOST_LISTED_NAME_BYTES: usize = 16 * 1024;

pub(crate) fn elicitation_params(
    database: &str,
    statement: &DestructiveStatement,
    sql: &str,
    form_mode: bool,
) -> String {
    let mut out = JsonOut::new();
    out.begin_object();
    if form_mode {
        out.key("mode").string("form");
    }
    out.key("message")
        .string(&message(&visible_name(database), statement, sql))
        .key("requestedSchema")
        .begin_object()
        .key("type")
        .string("object")
        .key("properties")
        .begin_object()
        .key("run")
        .begin_object()
        .key("type")
        .string("boolean")
        .key("title")
        .string(&format!("Run it on {}", visible(database)))
        .key("description")
        .string("Yes runs the statement now. No, or closing this, runs nothing, and the agent is told you declined.")
        .key("default")
        .boolean(false)
        .end_object()
        .end_object()
        .key("required")
        .begin_array()
        .string("run")
        .end_array()
        .end_object()
        .end_object();
    out.finish()
}

pub(crate) fn message(database: &str, statement: &DestructiveStatement, sql: &str) -> String {
    let parts: Vec<String> = statement
        .parts
        .iter()
        .map(|part| format!("{}: {}", part.kind.label(), named(&part.objects)))
        .collect();
    let mut text = format!(
        "The AI agent asks to run a destructive statement on {database}. {}",
        parts.join("; ")
    );
    if statement.cascade {
        text.push_str(", with CASCADE (what depends on them goes too)");
    }
    text.push_str(".\n");
    if statement.objects().any(mixes_alphabets) {
        text.push_str(
            "Some of these names mix alphabets: two names that look the same may be different tables.\n",
        );
    }
    let escaped = visible_keeping_lines(&with_long_blanks_counted(sql));
    let total = escaped.chars().count();
    let mut shown: String = escaped.chars().take(MOST_SHOWN_SQL_CHARACTERS).collect();
    if total > MOST_SHOWN_SQL_CHARACTERS {
        shown.push_str(&format!(
            "\n({} more characters not shown)",
            thousands(total - MOST_SHOWN_SQL_CHARACTERS)
        ));
    }
    let fence = "`".repeat(longest_backtick_run(&shown).max(2) + 1);
    text.push_str(&format!("\n{fence}sql\n{shown}\n{fence}\n\n"));
    text.push_str(
        "It runs now, in its own transaction, and cannot be undone through the agent; only a backup brings the data back. Names are as the statement gives them: an earlier rename can make a familiar name stand for another table. Run it?",
    );
    text
}

fn named(objects: &[String]) -> String {
    let mut listed = String::new();
    for (index, object) in objects.iter().enumerate() {
        if listed.len() > MOST_LISTED_NAME_BYTES {
            listed.push_str(&format!(
                ", and {} more names too many to show: decline unless you know the whole statement",
                objects.len() - index
            ));
            break;
        }
        if index > 0 {
            listed.push_str(", ");
        }
        listed.push_str(&visible_name(object));
    }
    listed
}

const LONGEST_SHOWN_BLANK_RUN: usize = 16;

pub(crate) fn with_long_blanks_counted(sql: &str) -> String {
    let characters: Vec<char> = sql.chars().collect();
    let mut shown = String::with_capacity(sql.len());
    let mut at = 0;
    while at < characters.len() {
        if !characters[at].is_whitespace() {
            shown.push(characters[at]);
            at += 1;
            continue;
        }
        let end = characters[at..]
            .iter()
            .position(|character| !character.is_whitespace())
            .map_or(characters.len(), |offset| at + offset);
        let run = &characters[at..end];
        if run.len() > LONGEST_SHOWN_BLANK_RUN {
            shown.push_str(&format!(" <{} blank characters> ", thousands(run.len())));
        } else {
            shown.extend(run);
        }
        at = end;
    }
    shown
}

fn is_hidden_or_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{115f}'
                | '\u{1160}'
                | '\u{17b4}'
                | '\u{17b5}'
                | '\u{180b}'..='\u{180f}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff0}'..='\u{fffb}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e0fff}'
        )
}

pub(crate) fn visible(text: &str) -> String {
    escaped(text, false)
}

fn visible_keeping_lines(text: &str) -> String {
    escaped(text, true)
}

fn escaped(text: &str, keep_lines: bool) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        let layout = keep_lines && matches!(character, '\n' | '\t');
        if is_hidden_or_control(character) && !layout {
            shown.push_str(&format!("\\u{{{:04x}}}", u32::from(character)));
        } else {
            shown.push(character);
        }
    }
    shown
}

fn visible_name(name: &str) -> String {
    let mut shown = String::with_capacity(name.len());
    for character in visible(name).chars() {
        if matches!(
            character,
            '`' | '[' | ']' | '(' | ')' | '<' | '>' | '*' | '#' | '!' | '|' | '~' | '\\'
        ) {
            shown.push('\\');
        }
        shown.push(character);
    }
    shown
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alphabet {
    Latin,
    Greek,
    Cyrillic,
    Other,
}

fn alphabet(character: char) -> Option<Alphabet> {
    if !character.is_alphabetic() {
        return None;
    }
    Some(match u32::from(character) {
        0x41..=0x5a | 0x61..=0x7a | 0xc0..=0x24f | 0x1e00..=0x1eff => Alphabet::Latin,
        0x370..=0x3ff | 0x1f00..=0x1fff => Alphabet::Greek,
        0x400..=0x52f | 0x1c80..=0x1c8f | 0x2de0..=0x2dff | 0xa640..=0xa69f => Alphabet::Cyrillic,
        _ => Alphabet::Other,
    })
}

pub(crate) fn mixes_alphabets(name: &str) -> bool {
    let mut seen: Option<Alphabet> = None;
    for found in name.chars().filter_map(alphabet) {
        match seen {
            None => seen = Some(found),
            Some(first) if first != found => return true,
            Some(_) => {}
        }
    }
    false
}

fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for character in text.chars() {
        if character == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

fn thousands(number: usize) -> String {
    let digits = number.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::{message, mixes_alphabets, thousands, visible};
    use eggdb_sql::{DestructiveKind, DestructivePart, DestructiveStatement};

    fn dropped(objects: &[&str], cascade: bool) -> DestructiveStatement {
        DestructiveStatement {
            parts: vec![DestructivePart {
                kind: DestructiveKind::DropTable,
                objects: objects.iter().map(|object| (*object).to_owned()).collect(),
            }],
            cascade,
        }
    }

    #[test]
    fn the_message_names_each_kind_with_its_objects_and_cascade_before_the_sql() {
        let text = message(
            "app.db",
            &dropped(&["users", "order_items"], true),
            "DROP TABLE users, order_items CASCADE",
        );
        assert!(
            text.starts_with(
                "The AI agent asks to run a destructive statement on app.db. DROP TABLE: users, order_items, with CASCADE"
            ),
            "{text}"
        );
        assert!(text.contains("```sql\nDROP TABLE users, order_items CASCADE\n```"));
        assert!(text.ends_with("Run it?"));
        let mixed = eggdb_sql::destructive_statement(
            "ALTER TABLE m ALTER COLUMN b TYPE bigint, DROP COLUMN a",
            eggdb_sql::Dialect::PostgreSql,
        )
        .unwrap()
        .unwrap();
        let text = message(
            "app.db",
            &mixed,
            "ALTER TABLE m ALTER COLUMN b TYPE bigint, DROP COLUMN a",
        );
        assert!(
            text.contains("a change of a column's type: m.b; ALTER TABLE ... DROP COLUMN: m.a."),
            "{text}"
        );
    }

    #[test]
    fn hidden_characters_are_escaped_lines_kept_and_the_fence_cannot_be_closed() {
        let sql = format!(
            "DROP TABLE t -- \u{202e}evil\n```\n\t{}\u{2028}\u{00ad}\u{fe0f}",
            "x".repeat(3000)
        );
        let text = message("app.db", &dropped(&["t"], false), &sql);
        for hidden in ['\u{202e}', '\u{2028}', '\u{00ad}', '\u{fe0f}'] {
            assert!(!text.contains(hidden), "{hidden:?}");
        }
        assert!(text.contains("\\u{202e}evil\n```\n\txxx"));
        assert!(text.contains("````sql\n"));
        assert!(text.contains("more characters not shown)"));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.contains(&"````"));
    }

    #[test]
    fn every_object_is_named_even_when_blanks_push_it_past_the_shown_sql() {
        let mut names: Vec<String> = (1..=10).map(|number| format!("tmp{number}")).collect();
        names.push("users".to_owned());
        let named: Vec<&str> = names.iter().map(String::as_str).collect();
        let sql = format!(
            "DROP TABLE {},{}users",
            names[..10].join(", "),
            " ".repeat(3000)
        );
        let text = message("app.db", &dropped(&named, false), &sql);
        assert!(text.contains("tmp10, users."), "{text}");
        assert!(text.contains("<3,000 blank characters>"), "{text}");
        assert!(text.contains(" users\n"), "{text}");
    }

    #[test]
    fn names_cannot_add_markdown_or_lines() {
        let text = message(
            "app.db",
            &dropped(&["`x`](http://e.example)\n"], false),
            "DROP TABLE x",
        );
        assert!(
            text.contains("\\`x\\`\\]\\(http://e.example\\)\\\\u{000a}"),
            "{text}"
        );
    }

    #[test]
    fn names_mixing_alphabets_are_flagged() {
        assert!(mixes_alphabets("p\u{0430}yments"));
        assert!(!mixes_alphabets("payments_2026"));
        assert!(!mixes_alphabets(
            "\u{043f}\u{043b}\u{0430}\u{0442}\u{0435}\u{0436}\u{0438}"
        ));
        let text = message(
            "app.db",
            &dropped(&["p\u{0430}yments"], false),
            "DROP TABLE x",
        );
        assert!(text.contains("mix alphabets"));
        assert_eq!(visible("a\tb"), "a\\u{0009}b");
        assert_eq!(thousands(1204), "1,204");
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
