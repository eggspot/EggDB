//! Paging cursors (D145): the text a list command prints and a list tool returns as
//! `next_cursor`, and takes back with `--after` or `after`.
//!
//! A cursor names the position just after the last item of a page, so the next page starts
//! strictly after it. Positions are what the list is ordered by (a row's place in its table,
//! a table's kind, schema and name), never a count, so a cursor stays valid while others
//! write. The text is `<list letter><version>.<hex>`: ASCII, safe in a shell and in JSON
//! without quoting. The hex holds a 32-bit check of what the list is of (the table for rows)
//! and then the position, so a cursor of another list or another table is refused rather than
//! read from an arbitrary place.

/// The longest cursor text read back: a position of 32 KiB, written in hex.
pub const LONGEST_CURSOR: usize = 4 + 8 + 2 * 32 * 1024;

const VERSION: char = '1';
const CHECK_BYTES: usize = 4;
const SHOWN_CURSOR_CHARACTERS: usize = 64;

/// The list a cursor continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorList {
    /// `eggdb table list`: the tables and views.
    Tables,
    /// `eggdb row list` and the `row_list` tool: one table's rows.
    Rows,
}

impl CursorList {
    /// The command whose pages the cursor continues: `eggdb table list` or `eggdb row list`.
    pub const fn command(self) -> &'static str {
        match self {
            Self::Tables => "eggdb table list",
            Self::Rows => "eggdb row list",
        }
    }

    const fn letter(self) -> char {
        match self {
            Self::Tables => 't',
            Self::Rows => 'r',
        }
    }

    fn of_letter(letter: char) -> Option<Self> {
        match letter {
            't' => Some(Self::Tables),
            'r' => Some(Self::Rows),
            _ => None,
        }
    }
}

/// Why a cursor cannot be read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorRefusal {
    /// It is not a cursor EggDB wrote: wrong shape, wrong version, or cut.
    Malformed,
    /// It continues this other list, such as `table list` where rows were asked for.
    OtherList(CursorList),
    /// It continues the rows of another table.
    OtherTable,
}

impl CursorRefusal {
    /// The message for this refusal, the same from the CLI and the MCP server: `option` is
    /// `--after` or `after`, `list` the list that refused it, `scope` the table's
    /// `schema.name` for rows, and `shown` the cursor as given, made safe to show and cut by
    /// [`shown_cursor`].
    pub fn message(self, option: &str, list: CursorList, scope: &str, shown: &str) -> String {
        match self {
            Self::Malformed => format!(
                "{option} needs the cursor printed at the end of the page before, not '{shown}'. Leave {option} out for the first page."
            ),
            Self::OtherList(other) => format!(
                "{option} got a cursor of {} ('{shown}'), which {} cannot use. Page each list with the cursors its own pages print.",
                other.command(),
                list.command()
            ),
            Self::OtherTable => format!(
                "{option} got a cursor of another table, not of {scope}. Page a table with the cursors its own pages print."
            ),
        }
    }
}

/// A cursor as a message shows it: cut after 64 characters, with `…` when it was cut.
pub fn shown_cursor(text: &str) -> String {
    let mut shown: String = text.chars().take(SHOWN_CURSOR_CHARACTERS).collect();
    if text.chars().nth(SHOWN_CURSOR_CHARACTERS).is_some() {
        shown.push('…');
    }
    shown
}

/// The cursor that continues `list` of `scope` (the table's `schema.name` for rows, empty for
/// the tables) after the item at `position`.
pub fn encode_cursor(list: CursorList, scope: &str, position: &[u8]) -> String {
    let mut text = String::with_capacity(3 + 2 * (CHECK_BYTES + position.len()));
    text.push(list.letter());
    text.push(VERSION);
    text.push('.');
    push_hex(&mut text, &scope_check(scope).to_be_bytes());
    push_hex(&mut text, position);
    text
}

/// Whether `text` is a cursor of `list`, before what it is the list of is known: a cursor
/// that is malformed or continues another list is refused here, before any file is opened.
pub fn check_cursor(list: CursorList, text: &str) -> Result<(), CursorRefusal> {
    parsed(text).and_then(|(given, _)| given_list(given, list)).map(|_| ())
}

/// The position a cursor of `list` of `scope` names, or why it is refused. Any text is safe
/// to give: nothing here panics, and the result is only ever a start of a scan.
pub fn decode_cursor(list: CursorList, scope: &str, text: &str) -> Result<Vec<u8>, CursorRefusal> {
    let (given, bytes) = parsed(text)?;
    given_list(given, list)?;
    let Some((check, position)) = bytes.split_first_chunk::<CHECK_BYTES>() else {
        return Err(CursorRefusal::Malformed);
    };
    if u32::from_be_bytes(*check) != scope_check(scope) {
        return Err(match list {
            CursorList::Rows => CursorRefusal::OtherTable,
            CursorList::Tables => CursorRefusal::Malformed,
        });
    }
    Ok(position.to_vec())
}

fn given_list(given: CursorList, list: CursorList) -> Result<CursorList, CursorRefusal> {
    if given == list {
        Ok(given)
    } else {
        Err(CursorRefusal::OtherList(given))
    }
}

fn parsed(text: &str) -> Result<(CursorList, Vec<u8>), CursorRefusal> {
    if text.len() > LONGEST_CURSOR || !text.is_ascii() {
        return Err(CursorRefusal::Malformed);
    }
    let mut characters = text.chars();
    let given = characters
        .next()
        .and_then(CursorList::of_letter)
        .ok_or(CursorRefusal::Malformed)?;
    if characters.next() != Some(VERSION) || characters.next() != Some('.') {
        return Err(CursorRefusal::Malformed);
    }
    let bytes = from_hex(characters.as_str()).ok_or(CursorRefusal::Malformed)?;
    if bytes.len() < CHECK_BYTES {
        return Err(CursorRefusal::Malformed);
    }
    Ok((given, bytes))
}

fn scope_check(scope: &str) -> u32 {
    const OFFSET: u32 = 0x811c_9dc5;
    const PRIME: u32 = 0x0100_0193;
    scope
        .bytes()
        .fold(OFFSET, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(PRIME))
}

fn push_hex(text: &mut String, bytes: &[u8]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0xf)]));
    }
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((digit(pair[0])? << 4) | digit(pair[1])?))
        .collect()
}

/// The position of a table or view in `eggdb table list`'s order (tables first, then views,
/// each by schema and name), as a cursor carries it.
pub fn table_position(is_view: bool, schema: &str, name: &str) -> Vec<u8> {
    let mut position = Vec::with_capacity(5 + schema.len() + name.len());
    position.push(u8::from(is_view));
    position.extend_from_slice(&u32::try_from(schema.len()).unwrap_or(u32::MAX).to_be_bytes());
    position.extend_from_slice(schema.as_bytes());
    position.extend_from_slice(name.as_bytes());
    position
}

/// The table or view a position from [`table_position`] names: whether it is a view, its
/// schema and its name; `None` when the bytes are not such a position.
pub fn table_at(position: &[u8]) -> Option<(bool, String, String)> {
    let (&kind, rest) = position.split_first()?;
    let is_view = match kind {
        0 => false,
        1 => true,
        _ => return None,
    };
    let (length, rest) = rest.split_first_chunk::<4>()?;
    let length = usize::try_from(u32::from_be_bytes(*length)).ok()?;
    if length > rest.len() {
        return None;
    }
    let (schema, name) = rest.split_at(length);
    let schema = std::str::from_utf8(schema).ok()?.to_owned();
    let name = std::str::from_utf8(name).ok()?.to_owned();
    Some((is_view, schema, name))
}

#[cfg(test)]
mod tests {
    use super::{
        CursorList, CursorRefusal, LONGEST_CURSOR, check_cursor, decode_cursor, encode_cursor,
        shown_cursor, table_at, table_position,
    };

    #[test]
    fn a_cursor_reads_back_as_its_position_and_only_for_its_list_and_table() {
        let cursor = encode_cursor(CursorList::Rows, "public.orders", &[0x80, 0, 0, 7]);
        assert!(cursor.starts_with("r1."), "{cursor}");
        assert!(cursor.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'.'));
        assert_eq!(
            decode_cursor(CursorList::Rows, "public.orders", &cursor),
            Ok(vec![0x80, 0, 0, 7])
        );
        assert_eq!(
            decode_cursor(CursorList::Rows, "public.users", &cursor),
            Err(CursorRefusal::OtherTable)
        );
        assert_eq!(
            decode_cursor(CursorList::Tables, "", &cursor),
            Err(CursorRefusal::OtherList(CursorList::Rows))
        );
        assert_eq!(check_cursor(CursorList::Rows, &cursor), Ok(()));
        assert_eq!(
            check_cursor(CursorList::Tables, &cursor),
            Err(CursorRefusal::OtherList(CursorList::Rows))
        );
        let empty = encode_cursor(CursorList::Tables, "", &[]);
        assert_eq!(decode_cursor(CursorList::Tables, "", &empty), Ok(Vec::new()));
        assert_eq!(
            decode_cursor(CursorList::Tables, "", "t1.00000000"),
            Err(CursorRefusal::Malformed)
        );
    }

    #[test]
    fn the_messages_name_the_option_the_lists_and_the_table() {
        assert_eq!(
            CursorRefusal::Malformed.message("--after", CursorList::Rows, "public.orders", "42"),
            "--after needs the cursor printed at the end of the page before, not '42'. Leave --after out for the first page."
        );
        assert_eq!(
            CursorRefusal::OtherList(CursorList::Tables).message(
                "after",
                CursorList::Rows,
                "public.orders",
                "t1.00"
            ),
            "after got a cursor of eggdb table list ('t1.00'), which eggdb row list cannot use. Page each list with the cursors its own pages print."
        );
        assert_eq!(
            CursorRefusal::OtherTable.message("--after", CursorList::Rows, "public.orders", "r1.00"),
            "--after got a cursor of another table, not of public.orders. Page a table with the cursors its own pages print."
        );
        assert_eq!(shown_cursor("r1.ab"), "r1.ab");
        let long = format!("r1.{}", "a".repeat(100));
        assert_eq!(shown_cursor(&long), format!("r1.{}…", "a".repeat(61)));
    }

    #[test]
    fn any_other_text_is_malformed_and_nothing_panics() {
        let long = format!("r1.{}", "0".repeat(LONGEST_CURSOR));
        for text in [
            "", "r", "r1", "r1.", "r1.abc", "r1.zz00aabb", "r2.00000000", "x1.00000000",
            "r1-00000000", "r1.000000", "r1.0000000G", "R1.00000000", "r1.00000000\n",
            "r1.é0000000", long.as_str(),
        ] {
            assert_eq!(
                decode_cursor(CursorList::Rows, "", text),
                Err(CursorRefusal::Malformed),
                "{text:?}"
            );
        }
        let mut seed = 0x2545_f491_u32;
        for _ in 0..2000 {
            let length = (seed % 24) as usize;
            let text: String = (0..length)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    char::from(b"r1.0af9t-\x00"[(seed % 10) as usize])
                })
                .collect();
            let _ = decode_cursor(CursorList::Rows, "public.t", &text);
            let _ = decode_cursor(CursorList::Tables, "", &text);
        }
    }

    #[test]
    fn a_table_position_orders_tables_before_views_and_reads_back() {
        let table = table_position(false, "sales", "totals");
        let view = table_position(true, "app", "a");
        assert!(table < view);
        assert_eq!(table_at(&table), Some((false, "sales".to_owned(), "totals".to_owned())));
        assert_eq!(table_at(&view), Some((true, "app".to_owned(), "a".to_owned())));
        assert_eq!(table_at(&[]), None);
        assert_eq!(table_at(&[2, 0, 0, 0, 0]), None);
        assert_eq!(table_at(&[0, 0, 0, 0, 9, b'x']), None);
        assert_eq!(table_at(&[0, 0, 0]), None);
        assert_eq!(table_at(&[0, 0, 0, 0, 1, 0xff]), None);
    }
}
