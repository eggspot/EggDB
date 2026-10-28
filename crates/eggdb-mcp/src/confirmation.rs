use crate::json_out::JsonOut;
use crate::tools::MOST_LISTED_OBJECTS;
use eggdb_sql::DestructiveStatement;

const MOST_SHOWN_SQL_CHARACTERS: usize = 2000;

pub(crate) fn elicitation_params(
    database: &str,
    statement: &DestructiveStatement,
    sql: &str,
    form_mode: bool,
) -> String {
    let database = visible_name(database);
    let mut out = JsonOut::new();
    out.begin_object();
    if form_mode {
        out.key("mode").string("form");
    }
    out.key("message")
        .string(&message(&database, statement, sql))
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
        .string(&format!("Run it on {database}"))
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
    let objects: Vec<String> = statement
        .objects
        .iter()
        .take(MOST_LISTED_OBJECTS)
        .map(|object| visible_name(object))
        .collect();
    let mut named = objects.join(", ");
    if statement.objects.len() > MOST_LISTED_OBJECTS {
        named.push_str(&format!(
            ", and {} more",
            statement.objects.len() - MOST_LISTED_OBJECTS
        ));
    }
    let mut text = format!(
        "The AI agent asks to run {} on {database}: {named}",
        statement.kind.label()
    );
    if statement.cascade {
        text.push_str(", with CASCADE (what depends on them goes too)");
    }
    text.push_str(".\n");
    if statement
        .objects
        .iter()
        .any(|object| mixes_alphabets(object))
    {
        text.push_str(
            "Some of these names mix alphabets: two names that look the same may be different tables.\n",
        );
    }
    let escaped = visible(sql);
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
        "It runs now, in its own transaction, and cannot be undone through the agent; only a backup brings the data back. Run it?",
    );
    text
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

pub(crate) fn visible(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        if is_hidden_or_control(character) {
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
            '`' | '[' | ']' | '(' | ')' | '<' | '>' | '*' | '_' | '#' | '!' | '|' | '~' | '\\'
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
    use eggdb_sql::{DestructiveKind, DestructiveStatement};

    fn dropped(objects: &[&str], cascade: bool) -> DestructiveStatement {
        DestructiveStatement {
            kind: DestructiveKind::DropTable,
            objects: objects.iter().map(|object| (*object).to_owned()).collect(),
            cascade,
        }
    }

    #[test]
    fn the_message_names_the_kind_objects_and_cascade_before_the_sql() {
        let text = message(
            "app.db",
            &dropped(&["users", "orders"], true),
            "DROP TABLE users, orders CASCADE",
        );
        assert!(text.starts_with(
            "The AI agent asks to run DROP TABLE on app.db: users, orders, with CASCADE"
        ));
        assert!(text.contains("```sql\nDROP TABLE users, orders CASCADE\n```"));
        assert!(text.ends_with("Run it?"));
    }

    #[test]
    fn hidden_characters_are_escaped_before_the_cut_and_the_fence_cannot_be_closed() {
        let sql = format!(
            "DROP TABLE t -- \u{202e}evil\n```\n{}\u{2028}",
            "x".repeat(3000)
        );
        let text = message("app.db", &dropped(&["t"], false), &sql);
        assert!(!text.contains('\u{202e}') && !text.contains('\u{2028}'));
        assert!(text.contains("\\u{202e}evil\\u{000a}```\\u{000a}"));
        assert!(text.contains("````sql\n"));
        let escaped = visible(&sql).chars().count();
        assert!(text.contains(&format!(
            "({} more characters not shown)",
            thousands(escaped - 2000)
        )));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.contains(&"````"));
    }

    #[test]
    fn many_objects_are_listed_ten_then_counted_and_names_cannot_add_markdown() {
        let names: Vec<String> = (0..13).map(|number| format!("t{number}")).collect();
        let named: Vec<&str> = names.iter().map(String::as_str).collect();
        let text = message("app.db", &dropped(&named, false), "DROP TABLE ...");
        assert!(text.contains("t9, and 3 more."));
        let text = message(
            "app.db",
            &dropped(&["`x`](http://e.example)\n"], false),
            "DROP TABLE x",
        );
        assert!(text.contains("\\`x\\`\\]\\(http://e.example\\)\\\\u{000a}"));
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
