use super::classify::{Action, Effect, Ignored, Skip};
use super::column_rewrite::alias_type_definition;
use super::script::DifferenceNote;
use super::words::{Token, Words, plain_qualified_name};
use crate::error::{SqlError, SqlState};

const HEAD_TOKENS: usize = 64;
const BATCH_LOGIC: &str = "batch logic (IF, WHILE, DECLARE, BEGIN ... END) is restored only as existence guards (IF [NOT] EXISTS (...), IF OBJECT_ID(...) IS [NOT] NULL) around one statement";
const LOGIC_WORDS: [&str; 9] = [
    "if", "while", "declare", "begin", "goto", "return", "throw", "raiserror", "try",
];
const NOT_SUPPORTED_YET: &str = "not supported yet";

const RUNNABLE_SET_OPTIONS: [&str; 15] = [
    "ansi_nulls",
    "quoted_identifier",
    "ansi_padding",
    "ansi_warnings",
    "arithabort",
    "concat_null_yields_null",
    "ansi_null_dflt_on",
    "nocount",
    "numeric_roundabort",
    "textsize",
    "dateformat",
    "language",
    "lock_timeout",
    "transaction",
    "xact_abort",
];

const ON_ONLY_OPTIONS: [&str; 7] = [
    "quoted_identifier",
    "ansi_nulls",
    "ansi_padding",
    "ansi_warnings",
    "arithabort",
    "concat_null_yields_null",
    "ansi_null_dflt_on",
];

const NOTED_WHEN_OFF: [&str; 3] = ["ansi_nulls", "quoted_identifier", "ansi_padding"];

pub(crate) fn sqlserver_table_key(name: &str, default_schema: &str) -> String {
    let folded = name.to_lowercase();
    match folded.split_once('.') {
        Some(("dbo", table)) => format!("{default_schema}.{table}"),
        Some(_) => folded,
        None => format!("{default_schema}.{folded}"),
    }
}

pub(crate) fn sqlserver_name_key(name: &str) -> String {
    let folded = name.to_lowercase();
    match folded.strip_prefix("dbo.") {
        Some(rest) => rest.to_owned(),
        None => folded,
    }
}

pub(crate) fn sqlcmd_command_reason(name: &str) -> &'static str {
    match name {
        "setvar" => "used for $(name)",
        "on error" => "eggdb restore's own flags decide",
        "connect" => "one database per file",
        "quit" | "exit" => "the script ends here",
        _ => "never run",
    }
}

pub(crate) fn refused_as_not_supported(error: &SqlError) -> bool {
    error.state() == SqlState::FEATURE_NOT_SUPPORTED
        || matches!(error.vendor_code(), Some(195 | 2715 | 40517))
}

fn head(text: &str) -> Vec<Token> {
    Words::sqlserver(text).take(HEAD_TOKENS).collect()
}

fn word_at(tokens: &[Token], at: usize) -> Option<&str> {
    match tokens.get(at) {
        Some(Token::Word(word)) => Some(word),
        _ => None,
    }
}

fn is(tokens: &[Token], at: usize, word: &str) -> bool {
    word_at(tokens, at) == Some(word)
}

fn name_at(tokens: &[Token], at: &mut usize) -> String {
    plain_qualified_name(tokens, at).unwrap_or_default()
}

fn kind_after_create(tokens: &[Token]) -> (usize, Option<&str>) {
    let mut at = 1;
    if is(tokens, at, "or") && is(tokens, at + 1, "alter") {
        at += 2;
    }
    (at, word_at(tokens, at))
}

pub(crate) fn takes_rest_of_batch(text: &str) -> bool {
    let tokens = head(text);
    match word_at(&tokens, 0) {
        Some("create" | "alter") => matches!(
            kind_after_create(&tokens).1,
            Some("procedure" | "proc" | "function" | "trigger" | "view")
        ),
        Some("if" | "while" | "declare" | "merge" | "try") => true,
        Some("begin") => !matches!(word_at(&tokens, 1), Some("tran" | "transaction")),
        _ => false,
    }
}

fn skip(kind: &'static str, name: String, reason: &'static str) -> Action {
    Action::Skip(Skip {
        kind,
        name,
        reason,
        detail: None,
        is_table: false,
        names_an_object: true,
    })
}

fn skip_statement(text: &str, reason: &'static str) -> Action {
    Action::Skip(Skip {
        kind: "statement",
        name: super::shortened(text),
        reason,
        detail: None,
        is_table: false,
        names_an_object: false,
    })
}

fn ignore(kind: &'static str) -> Action {
    Action::Ignore(Ignored::Other(kind))
}

fn skip_balanced(words: &mut Words<'_>) -> Option<()> {
    if !words.next()?.is_symbol(b'(') {
        return None;
    }
    let mut depth = 1usize;
    while depth > 0 {
        let token = words.next()?;
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth -= 1;
        }
    }
    Some(())
}

fn without_block(body: &str) -> Option<&str> {
    let mut words = Words::sqlserver(body);
    if !words.next()?.is_word("begin") {
        return Some(body);
    }
    let after_begin = words.offset();
    if words
        .next()
        .is_some_and(|token| token.is_word("tran") || token.is_word("transaction"))
    {
        return None;
    }
    let inside = body
        .get(after_begin..)?
        .trim_end()
        .trim_end_matches(';')
        .trim_end();
    let cut = inside.len().checked_sub(3)?;
    let (rest, last) = (inside.get(..cut)?, inside.get(cut..)?);
    let separated = rest
        .chars()
        .last()
        .is_none_or(|character| !(character.is_alphanumeric() || character == '_'));
    (last.eq_ignore_ascii_case("end") && separated).then(|| rest.trim())
}

fn guarded_statement(text: &str) -> Option<&str> {
    let mut words = Words::sqlserver(text);
    if !words.next()?.is_word("if") {
        return None;
    }
    let mut token = words.next()?;
    if token.is_word("not") {
        token = words.next()?;
    }
    if token.is_word("exists") {
        skip_balanced(&mut words)?;
    } else if token.is_word("object_id") {
        skip_balanced(&mut words)?;
        if !words.next()?.is_word("is") {
            return None;
        }
        let mut token = words.next()?;
        if token.is_word("not") {
            token = words.next()?;
        }
        if !token.is_word("null") {
            return None;
        }
    } else {
        return None;
    }
    let inner = without_block(text.get(words.offset()..)?.trim())?;
    let first = Words::sqlserver(inner).next();
    match first {
        Some(Token::Word(word)) if !LOGIC_WORDS.contains(&word.as_str()) => Some(inner),
        _ => None,
    }
}

fn guard(text: &str, known_parameter: &dyn Fn(&str) -> bool) -> Action {
    match guarded_statement(text) {
        Some(inner) => classify_sqlserver(inner, known_parameter),
        None => skip_statement(text, BATCH_LOGIC),
    }
}

fn executed_text(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let after = lower.find("sp_executesql")? + "sp_executesql".len();
    let rest = text.get(after..)?;
    let quote = rest.find('\'')?;
    let mut statement = String::new();
    let mut characters = rest.get(quote + 1..)?.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\'' {
            statement.push(character);
            continue;
        }
        if characters.peek() != Some(&'\'') {
            return Some(statement);
        }
        characters.next();
        statement.push('\'');
    }
    None
}

pub(crate) fn classify_sqlserver(text: &str, known_parameter: &dyn Fn(&str) -> bool) -> Action {
    let tokens = head(text);
    let Some(first) = word_at(&tokens, 0) else {
        return Action::Run(Effect::Other);
    };
    match first {
        "use" => ignore("USE"),
        "grant" => Action::Ignore(Ignored::Grant),
        "revoke" => Action::Ignore(Ignored::Revoke),
        "deny" => ignore("DENY"),
        "print" => ignore("PRINT"),
        "commit" | "rollback" | "save" => ignore("BEGIN and COMMIT"),
        "begin"
            if matches!(
                word_at(&tokens, 1),
                Some("tran" | "transaction" | "distributed")
            ) =>
        {
            ignore("BEGIN and COMMIT")
        }
        "if" => guard(text, known_parameter),
        "begin" | "while" | "declare" | "goto" | "return" | "throw" | "raiserror" | "try" => {
            skip_statement(text, BATCH_LOGIC)
        }
        "set" => set(&tokens),
        "create" => create(&tokens, text),
        "alter" => alter(&tokens, text),
        "drop" => Action::Run(Effect::OtherDdl),
        "insert" => {
            let mut at = 1;
            if is(&tokens, at, "into") {
                at += 1;
            }
            match plain_qualified_name(&tokens, &mut at) {
                Some(name) => Action::Run(Effect::Insert(name)),
                None => Action::Run(Effect::Data),
            }
        }
        "update" if is(&tokens, 1, "statistics") => ignore("UPDATE STATISTICS"),
        "update" | "delete" | "truncate" | "select" | "with" => Action::Run(Effect::Data),
        "exec" | "execute" => exec(&tokens, text, known_parameter),
        "enable" | "disable" if is(&tokens, 1, "trigger") => Action::Run(Effect::OtherDdl),
        _ => skip_statement(text, NOT_SUPPORTED_YET),
    }
}

fn set(tokens: &[Token]) -> Action {
    let Some(option) = word_at(tokens, 1) else {
        return Action::Run(Effect::Setting);
    };
    if option == "identity_insert" {
        let mut at = 2;
        return Action::Run(Effect::IdentityInsert(name_at(tokens, &mut at)));
    }
    if !RUNNABLE_SET_OPTIONS.contains(&option) {
        return Action::Ignore(Ignored::Set(option.to_ascii_uppercase()));
    }
    let off = tokens.iter().rev().find_map(|token| match token {
        Token::Word(word) => Some(word == "off"),
        _ => None,
    });
    let on_only = tokens.iter().any(
        |token| matches!(token, Token::Word(word) if ON_ONLY_OPTIONS.contains(&word.as_str())),
    );
    if off == Some(true) && on_only {
        return Action::Ignore(Ignored::Set(option.to_ascii_uppercase()));
    }
    Action::Run(Effect::Setting)
}

fn exec(tokens: &[Token], text: &str, known_parameter: &dyn Fn(&str) -> bool) -> Action {
    let mut at = 1;
    if matches!(tokens.get(at), Some(Token::Symbol(b'@'))) {
        return skip_statement(text, "procedures come later");
    }
    let name = name_at(tokens, &mut at).to_lowercase();
    let procedure = name.rsplit('.').next().unwrap_or(&name);
    match procedure {
        "sp_executesql" => match executed_text(text) {
            Some(executed)
                if Words::sqlserver(&executed).next().is_some_and(|token| {
                    matches!(&token, Token::Word(word) if !LOGIC_WORDS.contains(&word.as_str())
                        && word != "exec" && word != "execute")
                }) =>
            {
                classify_sqlserver(&executed, known_parameter)
            }
            _ => skip_statement(text, BATCH_LOGIC),
        },
        "sp_addextendedproperty" | "sp_updateextendedproperty" | "sp_dropextendedproperty"
            if text.to_ascii_lowercase().contains("ms_description") =>
        {
            Action::Run(Effect::Comment)
        }
        "sp_addextendedproperty" | "sp_updateextendedproperty" | "sp_dropextendedproperty" => {
            ignore("extended properties")
        }
        "sp_db_vardecimal_storage_format" | "sp_fulltext_database" => ignore("database settings"),
        "sp_addrolemember" | "sp_droprolemember" => skip(
            "role membership",
            String::new(),
            "roles come later; EggDB has one user per server",
        ),
        "sp_bindrule" | "sp_bindefault" | "sp_unbindrule" | "sp_unbindefault" => skip(
            "bound rule or default",
            String::new(),
            "rules and bound defaults come later",
        ),
        _ => skip_statement(text, "procedures come later"),
    }
}

fn trigger(tokens: &[Token], at: usize) -> Action {
    let mut name_end = at;
    let name = name_at(tokens, &mut name_end);
    if is(tokens, name_end, "on")
        && matches!(word_at(tokens, name_end + 1), Some("database" | "all"))
    {
        return skip("DDL trigger", name, "DDL triggers come later");
    }
    Action::Run(Effect::OtherDdl)
}

fn create(tokens: &[Token], text: &str) -> Action {
    let (mut at, kind) = kind_after_create(tokens);
    let Some(kind) = kind else {
        return Action::Run(Effect::OtherDdl);
    };
    at += 1;
    match kind {
        "table" => {
            let name = name_at(tokens, &mut at);
            Action::Run(Effect::Table {
                name,
                constraints: table_constraints(text),
            })
        }
        "view" => Action::Run(Effect::View(name_at(tokens, &mut at))),
        "schema" => Action::Run(Effect::Schema(name_at(tokens, &mut at))),
        "unique" | "clustered" | "nonclustered" | "index" => index(tokens, at - 1),
        "primary" | "xml" | "spatial" | "columnstore" | "fulltext" => {
            while word_at(tokens, at).is_some_and(|word| word != "index") {
                at += 1;
            }
            at += 1;
            skip(
                "index",
                name_at(tokens, &mut at),
                "XML, spatial, columnstore and full-text indexes come later",
            )
        }
        "procedure" | "proc" => skip(
            "procedure",
            name_at(tokens, &mut at),
            "procedures come later",
        ),
        "function" => skip("function", name_at(tokens, &mut at), "functions come later"),
        "trigger" => trigger(tokens, at),
        "synonym" => skip("synonym", name_at(tokens, &mut at), "synonyms come later"),
        "type" if alias_type_definition(text).is_some() => ignore("alias types"),
        "type" => skip(
            "type",
            name_at(tokens, &mut at),
            "user-defined table types come later",
        ),
        "sequence" => Action::Run(Effect::Sequence(name_at(tokens, &mut at))),
        "user" | "login" | "role" | "application" => skip(
            if kind == "login" {
                "login"
            } else {
                "user or role"
            },
            name_at(tokens, &mut at),
            "roles come later; EggDB has one user per server",
        ),
        "rule" | "default" => skip(
            if kind == "rule" {
                "rule"
            } else {
                "bound default"
            },
            name_at(tokens, &mut at),
            "rules and bound defaults come later",
        ),
        "database" => ignore("CREATE DATABASE"),
        "partition" => ignore("partition functions and schemes"),
        "statistics" => ignore("CREATE STATISTICS"),
        "assembly" | "certificate" | "asymmetric" | "symmetric" | "master" | "credential"
        | "queue" | "service" | "contract" | "message" | "route" | "remote" | "broker"
        | "security" | "event" | "endpoint" | "server" | "external" | "workload" | "audit"
        | "resource" | "column" | "search" | "cryptographic" | "availability" => skip(
            "server object",
            format!("{kind} {}", name_after_kind_words(tokens, at)),
            NOT_SUPPORTED_YET,
        ),
        _ => skip(
            "object",
            format!("{kind} {}", name_after_kind_words(tokens, at)),
            NOT_SUPPORTED_YET,
        ),
    }
}

fn name_after_kind_words(tokens: &[Token], mut at: usize) -> String {
    while matches!(
        word_at(tokens, at),
        Some(
            "key"
                | "type"
                | "binding"
                | "priority"
                | "policy"
                | "catalog"
                | "stoplist"
                | "schema"
                | "collection"
                | "specification"
                | "notification"
                | "session"
                | "pool"
                | "group"
                | "table"
                | "data"
                | "source"
                | "file"
                | "format"
                | "library"
                | "language"
                | "list"
                | "property"
        )
    ) {
        at += 1;
    }
    name_at(tokens, &mut at)
}

fn index(tokens: &[Token], mut at: usize) -> Action {
    while matches!(
        word_at(tokens, at),
        Some("unique" | "clustered" | "nonclustered")
    ) {
        at += 1;
    }
    if !is(tokens, at, "index") {
        return skip(
            "index",
            String::new(),
            "XML, spatial, columnstore and full-text indexes come later",
        );
    }
    at += 1;
    Action::Run(Effect::Index(name_at(tokens, &mut at)))
}

fn table_constraints(text: &str) -> u64 {
    let mut depth = 0u32;
    let mut constraints = 0u64;
    for token in Words::sqlserver(text) {
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 1 && token.is_word("constraint") {
            constraints += 1;
        }
    }
    constraints
}

fn alter(tokens: &[Token], text: &str) -> Action {
    let (mut at, kind) = kind_after_create(tokens);
    let Some(kind) = kind else {
        return Action::Run(Effect::OtherDdl);
    };
    at += 1;
    match kind {
        "table" => alter_table(tokens, at, text),
        "view" => Action::Run(Effect::View(name_at(tokens, &mut at))),
        "database" => ignore("ALTER DATABASE"),
        "index" => ignore("ALTER INDEX"),
        "sequence" => Action::Run(Effect::OtherDdl),
        "authorization" => ignore("ALTER AUTHORIZATION"),
        "procedure" | "proc" => skip(
            "procedure",
            name_at(tokens, &mut at),
            "procedures come later",
        ),
        "function" => skip("function", name_at(tokens, &mut at), "functions come later"),
        "trigger" => trigger(tokens, at),
        "role" | "user" | "login" | "application" => skip(
            "role membership",
            name_at(tokens, &mut at),
            "roles come later; EggDB has one user per server",
        ),
        _ => skip(
            "object",
            format!("{kind} {}", name_after_kind_words(tokens, at)),
            NOT_SUPPORTED_YET,
        ),
    }
}

fn alter_table(tokens: &[Token], mut at: usize, text: &str) -> Action {
    let table = name_at(tokens, &mut at);
    if is(tokens, at, "with") && matches!(word_at(tokens, at + 1), Some("check" | "nocheck")) {
        at += 2;
    }
    match word_at(tokens, at) {
        Some("nocheck") if is(tokens, at + 1, "constraint") => ignore("NOCHECK CONSTRAINT"),
        Some("check") if is(tokens, at + 1, "constraint") => Action::Run(Effect::OtherDdl),
        Some("enable" | "disable") if is(tokens, at + 1, "trigger") => {
            Action::Run(Effect::OtherDdl)
        }
        Some("set")
            if tokens
                .get(at + 1)
                .is_some_and(|token| token.is_symbol(b'(')) =>
        {
            ignore("storage settings")
        }
        Some("switch") => skip("partition", table, "partitioned tables come later"),
        Some("add") => {
            at += 1;
            let name = if is(tokens, at, "constraint") {
                at += 1;
                let name = name_at(tokens, &mut at);
                Some(name)
            } else {
                None
            };
            match word_at(tokens, at) {
                Some("default") => {
                    let column = tokens
                        .iter()
                        .skip(at)
                        .position(|token| token.is_word("for"))
                        .and_then(|offset| {
                            let mut column_at = at + offset + 1;
                            plain_qualified_name(tokens, &mut column_at)
                        })
                        .unwrap_or_default();
                    Action::Run(Effect::SetDefault {
                        object: format!("{table}.{column}"),
                        default: default_expression(text),
                    })
                }
                Some("primary" | "unique" | "foreign" | "check") => {
                    Action::Run(Effect::AddConstraint { table, name })
                }
                _ if name.is_some() => Action::Run(Effect::AddConstraint { table, name }),
                _ => Action::Run(Effect::AlterTable {
                    constraints: 0,
                    identity_sequences: Vec::new(),
                }),
            }
        }
        _ => Action::Run(Effect::AlterTable {
            constraints: 0,
            identity_sequences: Vec::new(),
        }),
    }
}

fn default_expression(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let Some(start) = find_word(&lower, "default") else {
        return String::new();
    };
    let after = start + "default".len();
    let end = find_word(&lower[after..], "for").map_or(text.len(), |offset| after + offset);
    text.get(after..end).unwrap_or("").trim().to_owned()
}

fn find_word(text: &str, word: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text.get(from..)?.find(word) {
        let at = from + offset;
        let before = at.checked_sub(1).map(|index| bytes[index]);
        let after = bytes.get(at + word.len()).copied();
        let boundary = |byte: Option<u8>| {
            byte.is_none_or(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
        };
        if boundary(before) && boundary(after) {
            return Some(at);
        }
        from = at + word.len();
    }
    None
}

pub(crate) fn sqlserver_differences(text: &str) -> Vec<DifferenceNote> {
    let tokens = head(text);
    match (word_at(&tokens, 0), word_at(&tokens, 1)) {
        (Some("create"), Some("table")) => table_differences(text),
        (Some("create"), Some("type")) => alias_type_differences(text),
        (Some("create"), Some("unique" | "clustered" | "nonclustered" | "index")) => {
            index_differences(text)
        }
        (Some("set"), Some(option)) if NOTED_WHEN_OFF.contains(&option) => {
            let off = tokens.iter().any(|token| token.is_word("off"));
            if !off {
                return Vec::new();
            }
            vec![DifferenceNote::new(
                format!("SET {}", option.to_ascii_uppercase()),
                "OFF in the source; EggDB always runs with it ON",
            )]
        }
        (Some("alter"), Some("table")) => {
            let nocheck = tokens
                .windows(2)
                .any(|pair| pair[0].is_word("nocheck") && pair[1].is_word("constraint"));
            if !nocheck {
                return Vec::new();
            }
            let mut at = 2;
            let table = name_at(&tokens, &mut at);
            vec![DifferenceNote::new(
                table,
                "a constraint disabled in the source (NOCHECK CONSTRAINT) is enforced here",
            )]
        }
        _ => Vec::new(),
    }
}

fn table_differences(text: &str) -> Vec<DifferenceNote> {
    let tokens: Vec<Token> = Words::sqlserver(text).collect();
    let mut at = 2;
    let table = name_at(&tokens, &mut at);
    let mut notes = Vec::new();
    let mut collations: Vec<String> = Vec::new();
    let mut depth = 0u32;
    let mut column: Option<String> = None;
    let mut expect_column = false;
    for token in &tokens[at.min(tokens.len())..] {
        if token.is_symbol(b'(') {
            depth += 1;
            if depth == 1 {
                expect_column = true;
            }
            continue;
        }
        if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == 1 && token.is_symbol(b',') {
            expect_column = true;
            continue;
        }
        if depth != 1 {
            continue;
        }
        if expect_column {
            expect_column = false;
            column = token.plain_name_part().map(str::to_owned);
            continue;
        }
        if let Token::Word(word) | Token::Quoted(word) = token {
            let lower = word.to_ascii_lowercase();
            if lower == "datetimeoffset"
                && let Some(column) = &column
            {
                notes.push(DifferenceNote::new(
                    format!("{table}.{column}"),
                    "values are stored in UTC; their offsets are lost",
                ));
            }
        }
    }
    for pair in tokens.windows(2) {
        if !pair[0].is_word("collate") {
            continue;
        }
        if let Some(name) = pair[1].plain_name_part() {
            let upper = name.to_ascii_uppercase();
            if !(upper.ends_with("_BIN") || upper.ends_with("_BIN2"))
                && !collations.contains(&upper)
            {
                collations.push(upper);
            }
        }
    }
    for collation in collations {
        notes.push(DifferenceNote::new(
            table.clone(),
            format!("COLLATE {collation}: text compares and sorts by code point here"),
        ));
    }
    notes
}

fn alias_type_differences(text: &str) -> Vec<DifferenceNote> {
    let Some((name, alias)) = alias_type_definition(text) else {
        return Vec::new();
    };
    let nullability = if alias.not_null { " NOT NULL" } else { "" };
    vec![DifferenceNote::new(
        format!("type {name}"),
        format!(
            "an alias type: its columns are created as {}{nullability}",
            alias.base
        ),
    )]
}

fn index_differences(text: &str) -> Vec<DifferenceNote> {
    let tokens: Vec<Token> = Words::sqlserver(text).collect();
    let mut at = 1;
    let unique = tokens.iter().take(4).any(|token| token.is_word("unique"));
    while matches!(
        word_at(&tokens, at),
        Some("unique" | "clustered" | "nonclustered")
    ) {
        at += 1;
    }
    at += 1;
    let name = name_at(&tokens, &mut at);
    let mut depth = 0u32;
    let mut lost = Vec::new();
    for token in &tokens[at.min(tokens.len())..] {
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && token.is_word("include") && !lost.contains(&"INCLUDE columns") {
            lost.push("INCLUDE columns");
        } else if depth == 0 && token.is_word("where") && !unique {
            lost.push("the filter (a full index here)");
        } else if depth == 1 && token.is_word("desc") && !lost.contains(&"DESC order") {
            lost.push("DESC order");
        }
    }
    if lost.is_empty() {
        return Vec::new();
    }
    vec![DifferenceNote::new(
        format!("index {name}"),
        format!("{} not kept", lost.join(", ")),
    )]
}
