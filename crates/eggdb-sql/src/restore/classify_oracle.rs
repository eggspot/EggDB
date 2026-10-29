use super::classify::{Action, Effect, Ignored, Skip, ToggledConstraint};
use super::script::DifferenceNote;
use super::words::{Token, Words, plain_qualified_name};
use crate::error::SqlError;
use std::ops::Range;

const HEAD_TOKENS: usize = 96;
const PLSQL_LATER: &str = "PL/SQL comes after v0.2.0";
const NOT_SUPPORTED_YET: &str = "not supported yet";
const NOT_IN_EGGDB: &str = "not in EggDB yet";
pub(crate) const MAX_BLOCK_STATEMENTS: usize = 1_000_000;

pub(crate) const STORAGE_SETTINGS: &str = "storage settings";
pub(crate) const PLSQL_SETTINGS: &str = "PL/SQL settings";
pub(crate) const USERS: &str = "users (schema created, login not)";
pub(crate) const QUERIES: &str = "queries";
pub(crate) const NOTHING_TO_DROP: &str = "nothing to drop";

const STORAGE_WORDS: [&str; 22] = [
    "parallel",
    "noparallel",
    "logging",
    "nologging",
    "shrink",
    "move",
    "allocate",
    "deallocate",
    "cache",
    "nocache",
    "monitoring",
    "nomonitoring",
    "pctfree",
    "pctused",
    "initrans",
    "storage",
    "compress",
    "nocompress",
    "row",
    "result_cache",
    "inmemory",
    "no",
];

pub(crate) fn oracle_table_key(name: &str, default_schema: &str) -> String {
    if name.contains('.') {
        name.to_owned()
    } else {
        format!("{default_schema}.{name}")
    }
}

pub(crate) fn oracle_name_key(name: &str) -> String {
    name.to_owned()
}

pub(crate) fn sqlplus_command_reason(name: &str) -> &'static str {
    match name {
        "set define" => "used for substitution variables",
        "define" | "undefine" => "used for substitution variables",
        "accept" => "never prompts; the variable keeps its DEFAULT or stays undefined",
        "connect" => "objects after it go into the user's schema; no login is made",
        "whenever" => "eggdb restore's own flags decide",
        "exit" => "the script ends here",
        "include" => "followed",
        "/" => "not run again: the statement before it already ran",
        "." => "not run (ended by .)",
        "unterminated" => "not run: no ; or / at the end of the script",
        "unknown" => "unknown SQL*Plus command",
        "spool" => "never writes a file",
        "host" => "never runs a program",
        _ => "never run",
    }
}

pub(crate) fn refused_as_not_supported(error: &SqlError) -> bool {
    error.state() == crate::error::SqlState::FEATURE_NOT_SUPPORTED
        || matches!(error.vendor_code(), Some(3001))
}

fn head(text: &str) -> Vec<Token> {
    Words::oracle(text).take(HEAD_TOKENS).collect()
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

fn skip_statement(kind: &'static str, text: &str, reason: &'static str) -> Action {
    Action::Skip(Skip {
        kind,
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

pub(crate) fn classify_oracle(text: &str, _known_parameter: &dyn Fn(&str) -> bool) -> Action {
    if text.trim_start().starts_with("<<") {
        return skip_statement("PL/SQL block", text, PLSQL_LATER);
    }
    let tokens = head(text);
    let Some(first) = word_at(&tokens, 0) else {
        if tokens.first().is_some_and(|token| token.is_symbol(b'(')) {
            return ignore(QUERIES);
        }
        return skip_statement("statement", text, NOT_SUPPORTED_YET);
    };
    match first {
        "grant" => Action::Ignore(Ignored::Grant),
        "revoke" => Action::Ignore(Ignored::Revoke),
        "commit" => ignore("COMMIT"),
        "savepoint" => ignore("SAVEPOINT"),
        "rollback" => ignore("ROLLBACK"),
        "set" => ignore("SET TRANSACTION, ROLE and CONSTRAINTS"),
        "select" => ignore(QUERIES),
        "with" if matches!(word_at(&tokens, 1), Some("function" | "procedure")) => {
            skip_statement("PL/SQL block", text, PLSQL_LATER)
        }
        "with" => ignore(QUERIES),
        "insert" => {
            if is(&tokens, 1, "all") || is(&tokens, 1, "first") {
                return Action::Run(Effect::Data);
            }
            let mut at = 1;
            if is(&tokens, at, "into") {
                at += 1;
            }
            match plain_qualified_name(&tokens, &mut at) {
                Some(name) => Action::Run(Effect::Insert(name)),
                None => Action::Run(Effect::Data),
            }
        }
        "update" | "delete" | "merge" | "truncate" => Action::Run(Effect::Data),
        "comment" => Action::Run(Effect::Comment),
        "create" => create(&tokens, text),
        "alter" => alter(&tokens, text),
        "drop" => drop(&tokens),
        "rename" => Action::Run(Effect::OtherDdl),
        "analyze" => ignore("ANALYZE"),
        "audit" | "noaudit" => ignore("AUDIT and NOAUDIT"),
        "purge" => ignore("PURGE"),
        "lock" => ignore("LOCK TABLE"),
        "explain" => ignore(QUERIES),
        "declare" => skip_statement("PL/SQL block", text, PLSQL_LATER),
        "begin" => match anonymous_block_statements(text) {
            Some(statements) => Action::Run(Effect::AnonymousBlock { statements }),
            None => skip_statement("PL/SQL block", text, PLSQL_LATER),
        },
        "exec" | "execute" | "call" => skip_statement("PL/SQL call", text, PLSQL_LATER),
        _ => skip_statement("statement", text, NOT_SUPPORTED_YET),
    }
}

fn kind_after_create(tokens: &[Token]) -> usize {
    let mut at = 1;
    while matches!(
        word_at(tokens, at),
        Some(
            "or" | "replace"
                | "editionable"
                | "noneditionable"
                | "editioning"
                | "force"
                | "noforce"
                | "no"
                | "and"
                | "resolve"
                | "compile"
                | "shared"
                | "sharing"
                | "metadata"
                | "data"
                | "none"
        )
    ) || tokens.get(at).is_some_and(|token| token.is_symbol(b'='))
    {
        at += 1;
    }
    at
}

fn create(tokens: &[Token], text: &str) -> Action {
    let mut at = kind_after_create(tokens);
    let Some(kind) = word_at(tokens, at) else {
        return skip_statement("statement", text, NOT_SUPPORTED_YET);
    };
    at += 1;
    match kind {
        "table" => Action::Run(Effect::Table {
            name: name_at(tokens, &mut at),
            constraints: table_constraints(text),
        }),
        "global" | "private" if is(tokens, at, "temporary") && is(tokens, at + 1, "table") => {
            at += 2;
            Action::Run(Effect::Table {
                name: name_at(tokens, &mut at),
                constraints: table_constraints(text),
            })
        }
        "view" => Action::Run(Effect::View(name_at(tokens, &mut at))),
        "unique" | "bitmap" | "multivalue" if is(tokens, at, "index") => {
            at += 1;
            Action::Run(Effect::Index(name_at(tokens, &mut at)))
        }
        "index" => Action::Run(Effect::Index(name_at(tokens, &mut at))),
        "sequence" => Action::Run(Effect::Sequence(name_at(tokens, &mut at))),
        "user" => Action::Run(Effect::User(name_at(tokens, &mut at))),
        "procedure" => skip("procedure", name_at(tokens, &mut at), PLSQL_LATER),
        "function" => skip("function", name_at(tokens, &mut at), PLSQL_LATER),
        "package" if is(tokens, at, "body") => {
            at += 1;
            skip("package body", name_at(tokens, &mut at), PLSQL_LATER)
        }
        "package" => skip("package", name_at(tokens, &mut at), PLSQL_LATER),
        "trigger" => skip("trigger", name_at(tokens, &mut at), PLSQL_LATER),
        "type" if is(tokens, at, "body") => {
            at += 1;
            skip("type body", name_at(tokens, &mut at), PLSQL_LATER)
        }
        "type" => skip("type", name_at(tokens, &mut at), PLSQL_LATER),
        "library" => skip("library", name_at(tokens, &mut at), PLSQL_LATER),
        "java" => {
            while matches!(
                word_at(tokens, at),
                Some("source" | "class" | "resource" | "named")
            ) {
                at += 1;
            }
            skip("java source", name_at(tokens, &mut at), PLSQL_LATER)
        }
        "public" if is(tokens, at, "synonym") => {
            at += 1;
            skip("synonym", name_at(tokens, &mut at), "synonyms come later")
        }
        "public" if is(tokens, at, "database") && is(tokens, at + 1, "link") => {
            at += 2;
            skip("database link", name_at(tokens, &mut at), NOT_IN_EGGDB)
        }
        "synonym" => skip("synonym", name_at(tokens, &mut at), "synonyms come later"),
        "database" if is(tokens, at, "link") => {
            at += 1;
            skip("database link", name_at(tokens, &mut at), NOT_IN_EGGDB)
        }
        "materialized" if is(tokens, at, "view") && is(tokens, at + 1, "log") => {
            at += 3;
            skip(
                "materialized view log",
                name_at(tokens, &mut at),
                "materialized views come later",
            )
        }
        "materialized" if is(tokens, at, "view") => {
            at += 1;
            skip(
                "materialized view",
                name_at(tokens, &mut at),
                "materialized views come later",
            )
        }
        "directory" | "tablespace" | "role" | "profile" | "context" | "cluster" | "dimension"
        | "operator" | "indextype" | "edition" | "audit" | "schema" | "database" | "restore"
        | "pluggable" | "spfile" | "pfile" | "flashback" | "lockdown" | "outline" | "rollback"
        | "attribute" | "analytic" | "hierarchy" | "domain" | "property" | "mle" => {
            let kind = other_kind(kind);
            while matches!(
                word_at(tokens, at),
                Some("policy" | "segment" | "dimension" | "view" | "archive" | "point")
            ) {
                at += 1;
            }
            skip(kind, name_at(tokens, &mut at), NOT_IN_EGGDB)
        }
        _ => skip_statement("statement", text, NOT_SUPPORTED_YET),
    }
}

fn other_kind(word: &str) -> &'static str {
    match word {
        "directory" => "directory",
        "tablespace" => "tablespace",
        "role" => "role",
        "profile" => "profile",
        "context" => "context",
        "cluster" => "cluster",
        "dimension" => "dimension",
        "operator" => "operator",
        "indextype" => "indextype",
        "edition" => "edition",
        "audit" => "audit policy",
        "schema" => "schema",
        "database" => "database",
        _ => "object",
    }
}

fn table_constraints(text: &str) -> u64 {
    let mut depth = 0u32;
    let mut constraints = 0u64;
    let mut foreign_in_element = false;
    for token in Words::oracle(text) {
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 1 && token.is_symbol(b',') {
            foreign_in_element = false;
        } else if depth == 1 {
            match &token {
                Token::Word(word) if word == "foreign" => {
                    foreign_in_element = true;
                    constraints += 1;
                }
                Token::Word(word) if word == "references" && !foreign_in_element => {
                    constraints += 1;
                }
                Token::Word(word) if matches!(word.as_str(), "primary" | "unique" | "check") => {
                    constraints += 1;
                }
                _ => {}
            }
        }
    }
    constraints
}

fn drop(tokens: &[Token]) -> Action {
    match word_at(tokens, 1) {
        Some("table" | "view" | "sequence" | "index") => Action::Run(Effect::Drop),
        Some("user") => ignore(USERS),
        Some(
            "materialized" | "public" | "synonym" | "database" | "directory" | "procedure"
            | "function" | "package" | "trigger" | "type" | "library" | "java" | "role" | "profile"
            | "context" | "tablespace" | "cluster" | "dimension" | "operator" | "indextype"
            | "edition" | "audit",
        ) => ignore(NOTHING_TO_DROP),
        _ => Action::Run(Effect::OtherDdl),
    }
}

fn alter(tokens: &[Token], text: &str) -> Action {
    match word_at(tokens, 1) {
        Some("session") => Action::Run(Effect::SessionSetting {
            current_schema: current_schema_of(tokens),
        }),
        Some("table") => {
            let all: Vec<Token> = Words::oracle(text).collect();
            alter_table(&all, text)
        }
        Some("index") => {
            if tokens.iter().any(|token| token.is_word("rename")) {
                Action::Run(Effect::OtherDdl)
            } else {
                ignore(STORAGE_SETTINGS)
            }
        }
        Some(
            "trigger" | "procedure" | "function" | "package" | "type" | "library" | "java" | "view",
        ) if tokens.iter().any(|token| {
            token.is_word("compile")
                || token.is_word("enable")
                || token.is_word("disable")
                || token.is_word("editionable")
                || token.is_word("noneditionable")
        }) =>
        {
            ignore(PLSQL_SETTINGS)
        }
        Some("trigger" | "procedure" | "function" | "package" | "type") => ignore(PLSQL_SETTINGS),
        Some("user") => ignore(USERS),
        Some("sequence" | "view") => Action::Run(Effect::OtherDdl),
        Some("database" | "system" | "pluggable") => ignore("ALTER DATABASE and ALTER SYSTEM"),
        _ => skip_statement("statement", text, NOT_SUPPORTED_YET),
    }
}

fn current_schema_of(tokens: &[Token]) -> Option<String> {
    let at = tokens
        .iter()
        .position(|token| token.is_word("current_schema"))?;
    let mut value_at = at + 1;
    if tokens
        .get(value_at)
        .is_some_and(|token| token.is_symbol(b'='))
    {
        value_at += 1;
    }
    tokens
        .get(value_at)
        .and_then(Token::plain_name_part)
        .map(str::to_owned)
}

fn after_validation(tokens: &[Token], mut at: usize) -> usize {
    while matches!(word_at(tokens, at), Some("validate" | "novalidate")) {
        at += 1;
    }
    at
}

fn toggled(tokens: &[Token], at: usize) -> Option<(ToggledConstraint, usize)> {
    match word_at(tokens, at)? {
        "constraint" => {
            let mut name_at = at + 1;
            let name = plain_qualified_name(tokens, &mut name_at)?;
            Some((ToggledConstraint::Named(name), name_at))
        }
        "primary" if is(tokens, at + 1, "key") => Some((ToggledConstraint::PrimaryKey, at + 2)),
        "unique" => Some((ToggledConstraint::Unique, at + 1)),
        _ => None,
    }
}

fn alter_table(tokens: &[Token], text: &str) -> Action {
    let mut at = 2;
    let table = name_at(tokens, &mut at);
    let Some(action) = word_at(tokens, at) else {
        return Action::Run(Effect::AlterTable {
            constraints: 0,
            identity_sequences: Vec::new(),
        });
    };
    let novalidate_in = |from: usize| {
        tokens[from..]
            .iter()
            .any(|token| token.is_word("novalidate"))
    };
    match action {
        "enable" | "disable" => {
            let enable = action == "enable";
            let target_at = after_validation(tokens, at + 1);
            if is(tokens, target_at, "all") && is(tokens, target_at + 1, "triggers") {
                return ignore(PLSQL_SETTINGS);
            }
            if matches!(word_at(tokens, target_at), Some("row" | "table")) {
                return ignore(STORAGE_SETTINGS);
            }
            match toggled(tokens, target_at) {
                Some((target, _)) => Action::Run(Effect::ToggleConstraint {
                    table,
                    target,
                    enable,
                    novalidate: enable && novalidate_in(at + 1),
                }),
                None => skip_statement("statement", text, NOT_SUPPORTED_YET),
            }
        }
        "modify" => {
            if let Some((target, after)) = toggled(tokens, at + 1) {
                let enable = tokens[after..].iter().any(|token| token.is_word("enable"));
                let disable = tokens[after..].iter().any(|token| token.is_word("disable"));
                if enable || disable {
                    return Action::Run(Effect::ToggleConstraint {
                        table,
                        target,
                        enable: !disable,
                        novalidate: !disable && novalidate_in(after),
                    });
                }
                return ignore("constraint states");
            }
            if is(tokens, at + 1, "partition") || is(tokens, at + 1, "subpartition") {
                return skip_statement("partition", text, "partitioned tables come later");
            }
            Action::Run(Effect::AlterTable {
                constraints: 0,
                identity_sequences: Vec::new(),
            })
        }
        "add" => add(tokens, at + 1, table, text),
        word if STORAGE_WORDS.contains(&word) => ignore(STORAGE_SETTINGS),
        _ => Action::Run(Effect::AlterTable {
            constraints: 0,
            identity_sequences: Vec::new(),
        }),
    }
}

fn starts_constraint(tokens: &[Token], at: usize) -> bool {
    matches!(
        word_at(tokens, at),
        Some("constraint" | "primary" | "unique" | "foreign" | "check")
    )
}

fn disabled_at_top(tokens: &[Token], from: usize) -> bool {
    let mut depth = 0u32;
    for token in &tokens[from.min(tokens.len())..] {
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && token.is_word("disable") {
            return true;
        }
    }
    false
}

fn add(tokens: &[Token], at: usize, table: String, text: &str) -> Action {
    if is(tokens, at, "supplemental") {
        return ignore(STORAGE_SETTINGS);
    }
    if starts_constraint(tokens, at) {
        let name = if is(tokens, at, "constraint") {
            let mut name_at = at + 1;
            plain_qualified_name(tokens, &mut name_at)
        } else {
            None
        };
        if disabled_at_top(tokens, at) {
            return Action::Run(Effect::DisabledConstraint {
                table,
                name: name.unwrap_or_default(),
            });
        }
        return Action::Run(Effect::AddConstraint { table, name });
    }
    if tokens.get(at).is_some_and(|token| token.is_symbol(b'(')) {
        let constraints = parenthesized_constraints(tokens, at);
        if disabled_at_top(tokens, at) && constraints > 0 {
            return skip_statement(
                "statement",
                text,
                "constraints disabled in ADD (...) come later",
            );
        }
        return Action::Run(Effect::AlterTable {
            constraints,
            identity_sequences: Vec::new(),
        });
    }
    Action::Run(Effect::AlterTable {
        constraints: 0,
        identity_sequences: Vec::new(),
    })
}

fn parenthesized_constraints(tokens: &[Token], open: usize) -> u64 {
    let mut depth = 0u32;
    let mut constraints = 0u64;
    let mut element_start = true;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if token.is_symbol(b'(') {
            depth += 1;
            if depth == 1 {
                element_start = true;
                continue;
            }
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 1 && token.is_symbol(b',') {
            element_start = true;
            continue;
        }
        if depth == 1 && element_start {
            if starts_constraint(tokens, index) {
                constraints += 1;
            }
            element_start = false;
        }
    }
    constraints
}

pub(crate) fn oracle_differences(text: &str) -> Vec<DifferenceNote> {
    let tokens = head(text);
    match (word_at(&tokens, 0), word_at(&tokens, 1)) {
        (Some("rollback"), _) => vec![DifferenceNote::new(
            "ROLLBACK",
            "the statements before it were already committed, each on its own",
        )],
        (Some("create"), _) if partitioned(text) => {
            let mut at = kind_after_create(&tokens);
            while matches!(
                word_at(&tokens, at),
                Some("global" | "private" | "temporary")
            ) {
                at += 1;
            }
            at += 1;
            vec![DifferenceNote::new(
                name_at(&tokens, &mut at),
                "partitioning not kept: one unpartitioned table here",
            )]
        }
        _ => Vec::new(),
    }
}

fn partitioned(text: &str) -> bool {
    let mut depth = 0u32;
    let mut previous_partition = false;
    let mut seen_table = false;
    for token in Words::oracle(text) {
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 0 {
            if token.is_word("table") {
                seen_table = true;
            }
            if seen_table && previous_partition && token.is_word("by") {
                return true;
            }
            previous_partition = token.is_word("partition");
            continue;
        }
        previous_partition = false;
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lex {
    Normal,
    String,
    QQuote(u8),
    Quoted,
    Comment,
}

fn top_level_pieces(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut lex = Lex::Normal;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        match lex {
            Lex::Normal => match byte {
                b'-' if next == Some(b'-') => {
                    while at < bytes.len() && bytes[at] != b'\n' {
                        at += 1;
                    }
                    continue;
                }
                b'/' if next == Some(b'*') => {
                    lex = Lex::Comment;
                    at += 1;
                }
                b'\'' => lex = Lex::String,
                b'"' => lex = Lex::Quoted,
                b'q' | b'Q' | b'n' | b'N' => {
                    let before = at.checked_sub(1).map(|index| bytes[index]);
                    let starts_word = before.is_none_or(|byte| {
                        !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#'))
                    });
                    let quote_at =
                        if matches!(byte, b'n' | b'N') && matches!(next, Some(b'q' | b'Q')) {
                            at + 2
                        } else if matches!(byte, b'q' | b'Q') {
                            at + 1
                        } else {
                            usize::MAX
                        };
                    if starts_word
                        && bytes.get(quote_at) == Some(&b'\'')
                        && let Some(&open) = bytes.get(quote_at + 1)
                    {
                        let close = match open {
                            b'[' => b']',
                            b'{' => b'}',
                            b'<' => b'>',
                            b'(' => b')',
                            other => other,
                        };
                        lex = Lex::QQuote(close);
                        at = quote_at + 1;
                    }
                }
                b';' => {
                    pieces.push(start..at);
                    start = at + 1;
                }
                _ => {}
            },
            Lex::String => {
                if byte == b'\'' {
                    if next == Some(b'\'') {
                        at += 1;
                    } else {
                        lex = Lex::Normal;
                    }
                }
            }
            Lex::QQuote(close) => {
                if byte == close && next == Some(b'\'') {
                    lex = Lex::Normal;
                    at += 1;
                }
            }
            Lex::Quoted => {
                if byte == b'"' {
                    lex = Lex::Normal;
                }
            }
            Lex::Comment => {
                if byte == b'*' && next == Some(b'/') {
                    lex = Lex::Normal;
                    at += 1;
                }
            }
        }
        at += 1;
    }
    pieces.push(start..bytes.len());
    pieces
}

fn trimmed_range(text: &str, range: Range<usize>) -> Range<usize> {
    let piece = &text[range.clone()];
    let leading = piece.len() - piece.trim_start().len();
    let trailing = piece.len() - piece.trim_end().len();
    (range.start + leading)..(range.end - trailing).max(range.start + leading)
}

fn plsql_only(tokens: &[Token]) -> bool {
    tokens.iter().any(|token| {
        token.is_symbol(b':')
            || token.is_symbol(b'%')
            || token.is_word("returning")
            || token.is_word("bulk")
            || token.is_word("forall")
    })
}

pub(crate) fn anonymous_block_statements(text: &str) -> Option<Vec<Range<usize>>> {
    let mut words = Words::oracle(text);
    if !words.next().is_some_and(|token| token.is_word("begin")) {
        return None;
    }
    let begin = text.to_ascii_lowercase().find("begin")?;
    let body_start = begin + "begin".len();
    let body = &text[body_start..];
    let pieces = top_level_pieces(body);
    let mut statements = Vec::new();
    let mut ended = false;
    for piece in pieces {
        let range = trimmed_range(body, piece);
        if range.is_empty() {
            continue;
        }
        if ended {
            return None;
        }
        let piece_text = &body[range.clone()];
        let tokens: Vec<Token> = Words::oracle(piece_text).collect();
        let first = word_at(&tokens, 0);
        match first {
            Some("end") => {
                if tokens.len() > 2
                    || tokens
                        .get(1)
                        .is_some_and(|token| token.plain_name_part().is_none())
                {
                    return None;
                }
                ended = true;
                continue;
            }
            Some("null") if tokens.len() == 1 => continue,
            Some("insert" | "update" | "delete" | "merge" | "commit") => {
                if plsql_only(&tokens) {
                    return None;
                }
            }
            _ => return None,
        }
        if statements.len() >= MAX_BLOCK_STATEMENTS {
            return None;
        }
        statements.push((range.start + body_start)..(range.end + body_start));
    }
    ended.then_some(statements)
}

#[cfg(test)]
pub(crate) fn block_texts(text: &str) -> Option<Vec<&str>> {
    anonymous_block_statements(text)
        .map(|ranges| ranges.into_iter().map(|range| &text[range]).collect())
}
