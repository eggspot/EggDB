use super::words::{Token, WordRules, Words, qualified_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    Run(Effect),
    Skip(Skip),
    Ignore(Ignored),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skip {
    pub(crate) kind: &'static str,
    pub(crate) name: String,
    pub(crate) reason: &'static str,
    pub(crate) detail: Option<&'static str>,
    pub(crate) is_table: bool,
    pub(crate) names_an_object: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ignored {
    OwnerTo,
    Grant,
    Revoke,
    Set(String),
    Other(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToggledConstraint {
    Named(String),
    PrimaryKey,
    Unique,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    Schema(String),
    Table {
        name: String,
        constraints: u64,
    },
    View(String),
    Sequence(String),
    Index(String),
    Type(String),
    Domain(String),
    AlterTable {
        constraints: u64,
        identity_sequences: Vec<String>,
    },
    AddConstraint {
        table: String,
        name: Option<String>,
    },
    SetDefault {
        object: String,
        default: String,
    },
    Comment,
    Copy(String),
    Insert(String),
    IdentityInsert(String),
    SequenceCall(String),
    Setting,
    SessionSetting {
        current_schema: Option<String>,
    },
    User(String),
    AnonymousBlock {
        statements: Vec<std::ops::Range<usize>>,
    },
    ToggleConstraint {
        table: String,
        target: ToggledConstraint,
        enable: bool,
        novalidate: bool,
    },
    DisabledConstraint {
        table: String,
        name: String,
    },
    Drop,
    Data,
    OtherDdl,
    Other,
}

impl Effect {
    pub(crate) fn is_ddl(&self) -> bool {
        matches!(
            self,
            Self::Schema(_)
                | Self::Table { .. }
                | Self::View(_)
                | Self::Sequence(_)
                | Self::Index(_)
                | Self::Type(_)
                | Self::Domain(_)
                | Self::AlterTable { .. }
                | Self::AddConstraint { .. }
                | Self::SetDefault { .. }
                | Self::Comment
                | Self::Drop
                | Self::OtherDdl
        )
    }

    pub(crate) fn created(&self) -> Option<(&'static str, &str)> {
        match self {
            Self::Schema(name) => Some(("schema", name)),
            Self::Table { name, .. } => Some(("table", name)),
            Self::View(name) => Some(("view", name)),
            Self::Sequence(name) => Some(("sequence", name)),
            Self::Index(name) if !name.is_empty() => Some(("index", name)),
            Self::Type(name) => Some(("type", name)),
            Self::Domain(name) => Some(("domain", name)),
            _ => None,
        }
    }

    pub(crate) fn data_target(&self) -> Option<&str> {
        match self {
            Self::Copy(table) | Self::Insert(table) | Self::IdentityInsert(table) => Some(table),
            _ => None,
        }
    }
}

pub(crate) fn classify(text: &str, known_parameter: &dyn Fn(&str) -> bool) -> Action {
    let mut words = Words::new(text);
    let Some(Token::Word(first)) = words.next() else {
        return Action::Run(Effect::Other);
    };
    match first.as_str() {
        "grant" => Action::Ignore(Ignored::Grant),
        "revoke" => Action::Ignore(Ignored::Revoke),
        "security" => Action::Ignore(Ignored::Other("SECURITY LABEL")),
        "begin" | "commit" | "end" | "rollback" | "abort" | "start" => {
            Action::Ignore(Ignored::Other("BEGIN and COMMIT"))
        }
        "set" => set(words, known_parameter),
        "reset" => Action::Run(Effect::Setting),
        "create" => create(&words.collect::<Vec<_>>()),
        "alter" => alter(&words.collect::<Vec<_>>()),
        "comment" => comment(&words.take(4).collect::<Vec<_>>()),
        "copy" => data_statement(words, Effect::Copy),
        "insert" => {
            let mut words = words;
            if !words.next().is_some_and(|token| token.is_word("into")) {
                return Action::Run(Effect::Data);
            }
            data_statement(words, Effect::Insert)
        }
        "drop" => drop_statement(&words.take(6).collect::<Vec<_>>()),
        "refresh" | "cluster" | "reindex" => Action::Run(Effect::OtherDdl),
        "update" => {
            if updates_catalog(words) {
                Action::Ignore(Ignored::Other("changes to pg_catalog.pg_database"))
            } else {
                Action::Run(Effect::Data)
            }
        }
        "select" => match sequence_call(text) {
            Some(sequence) => Action::Run(Effect::SequenceCall(sequence)),
            None => Action::Run(Effect::Data),
        },
        "delete" | "truncate" | "with" | "values" => Action::Run(Effect::Data),
        _ => Action::Run(Effect::Other),
    }
}

fn updates_catalog(words: Words<'_>) -> bool {
    let head: Vec<Token> = words.take(3).collect();
    head.first().is_some_and(|token| token.is_word("pg_catalog"))
        && head.get(1).is_some_and(|token| token.is_symbol(b'.'))
        && head.get(2).is_some_and(|token| token.is_word("pg_database"))
}

fn drop_statement(tokens: &[Token]) -> Action {
    let mut at = 1;
    if tokens.get(at).is_some_and(|token| token.is_word("if")) {
        at += 2;
    }
    let named = |word: &str| tokens.get(at).is_some_and(|token| token.is_word(word));
    match tokens.first() {
        Some(kind) if kind.is_word("schema") && named("public") => {
            Action::Ignore(Ignored::Other("DROP SCHEMA public"))
        }
        Some(kind) if kind.is_word("extension") && named("plpgsql") => {
            Action::Ignore(Ignored::Other("DROP EXTENSION plpgsql"))
        }
        Some(kind) if kind.is_word("database") => Action::Ignore(Ignored::Other("DROP DATABASE")),
        Some(kind) if kind.is_word("role") || kind.is_word("user") || kind.is_word("group") => {
            Action::Ignore(Ignored::Other("DROP ROLE"))
        }
        _ => Action::Run(Effect::Drop),
    }
}

fn comment(tokens: &[Token]) -> Action {
    let on_extension = tokens.first().is_some_and(|token| token.is_word("on"))
        && tokens
            .get(1)
            .is_some_and(|token| token.is_word("extension"));
    if on_extension && !tokens.get(2).is_some_and(|token| token.is_word("plpgsql")) {
        return Action::Ignore(Ignored::Other("COMMENT ON EXTENSION"));
    }
    Action::Run(Effect::Comment)
}

fn data_statement(words: Words<'_>, effect: fn(String) -> Effect) -> Action {
    let head: Vec<Token> = words.take(32).collect();
    let mut at = 0;
    match qualified_name(&head, &mut at) {
        Some(name) => Action::Run(effect(name)),
        None => Action::Run(Effect::Data),
    }
}

fn set(mut words: Words<'_>, known_parameter: &dyn Fn(&str) -> bool) -> Action {
    let mut token = words.next();
    if token
        .as_ref()
        .is_some_and(|token| token.is_word("session") || token.is_word("local"))
    {
        token = words.next();
    }
    let Some(token) = token else {
        return Action::Run(Effect::Setting);
    };
    if token.is_word("authorization") {
        return Action::Ignore(Ignored::Other("SET SESSION AUTHORIZATION"));
    }
    if token.is_word("role") {
        return Action::Ignore(Ignored::Other("SET ROLE"));
    }
    if [
        "time",
        "constraints",
        "transaction",
        "characteristics",
        "names",
    ]
    .iter()
    .any(|word| token.is_word(word))
    {
        return Action::Run(Effect::Setting);
    }
    let mut name = match token {
        Token::Word(word) => word,
        Token::Quoted(name) => name,
        Token::Symbol(_) | Token::Literal => return Action::Run(Effect::Setting),
    };
    let rest: Vec<Token> = words.take(3).collect();
    if rest.first().is_some_and(|token| token.is_symbol(b'.')) {
        if let Some(part) = rest.get(1).and_then(Token::name_part) {
            name = format!("{name}.{part}");
        }
        return Action::Ignore(Ignored::Set(name));
    }
    if known_parameter(&name) {
        Action::Run(Effect::Setting)
    } else {
        Action::Ignore(Ignored::Set(name))
    }
}

fn skip_words(tokens: &[Token], at: &mut usize, words: &[&str]) {
    while tokens
        .get(*at)
        .is_some_and(|token| words.iter().any(|word| token.is_word(word)))
    {
        *at += 1;
    }
}

fn skip_if_not_exists(tokens: &[Token], at: &mut usize) {
    if tokens.get(*at).is_some_and(|token| token.is_word("if")) {
        *at += 1;
        skip_words(tokens, at, &["not", "exists"]);
    }
}

fn name_at(tokens: &[Token], at: &mut usize) -> String {
    qualified_name(tokens, at).unwrap_or_default()
}

fn routine_name(tokens: &[Token], at: &mut usize) -> String {
    let name = name_at(tokens, at);
    if !tokens.get(*at).is_some_and(|token| token.is_symbol(b'(')) {
        return name;
    }
    if tokens
        .get(*at + 1)
        .is_some_and(|token| token.is_symbol(b')'))
    {
        format!("{name}()")
    } else {
        format!("{name}(…)")
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

fn skip_change(kind: &'static str, table: String, reason: &'static str) -> Action {
    Action::Skip(Skip {
        kind,
        name: table,
        reason,
        detail: None,
        is_table: false,
        names_an_object: false,
    })
}

fn skipped_table(name: String, detail: &'static str, reason: &'static str) -> Action {
    Action::Skip(Skip {
        kind: "table",
        name,
        reason,
        detail: Some(detail),
        is_table: true,
        names_an_object: true,
    })
}

fn create(tokens: &[Token]) -> Action {
    let mut at = 0;
    skip_words(
        tokens,
        &mut at,
        &[
            "or",
            "replace",
            "unique",
            "temp",
            "temporary",
            "unlogged",
            "global",
            "local",
            "trusted",
            "procedural",
            "default",
            "recursive",
        ],
    );
    let Some(Token::Word(kind)) = tokens.get(at) else {
        return Action::Run(Effect::OtherDdl);
    };
    at += 1;
    match kind.as_str() {
        "schema" => {
            skip_if_not_exists(tokens, &mut at);
            let name = name_at(tokens, &mut at);
            if name == "public" {
                return Action::Ignore(Ignored::Other("CREATE SCHEMA public"));
            }
            Action::Run(Effect::Schema(name))
        }
        "table" => create_table(tokens, at),
        "view" => {
            skip_if_not_exists(tokens, &mut at);
            Action::Run(Effect::View(name_at(tokens, &mut at)))
        }
        "sequence" => {
            skip_if_not_exists(tokens, &mut at);
            Action::Run(Effect::Sequence(name_at(tokens, &mut at)))
        }
        "index" => {
            skip_words(tokens, &mut at, &["concurrently"]);
            skip_if_not_exists(tokens, &mut at);
            if tokens.get(at).is_some_and(|token| token.is_word("on")) {
                return Action::Run(Effect::Index(String::new()));
            }
            Action::Run(Effect::Index(name_at(tokens, &mut at)))
        }
        "extension" => {
            skip_if_not_exists(tokens, &mut at);
            let name = name_at(tokens, &mut at);
            if name == "plpgsql" {
                Action::Run(Effect::OtherDdl)
            } else {
                skip(
                    "extension",
                    name,
                    "extensions other than plpgsql come later",
                )
            }
        }
        "function" => skip(
            "function",
            routine_name(tokens, &mut at),
            "functions come later",
        ),
        "procedure" => skip(
            "procedure",
            routine_name(tokens, &mut at),
            "procedures come later",
        ),
        "aggregate" => skip(
            "aggregate",
            routine_name(tokens, &mut at),
            "user-defined aggregates come later",
        ),
        "trigger" => on_table(tokens, at, "trigger", "on", "triggers come later"),
        "constraint" if tokens.get(at).is_some_and(|token| token.is_word("trigger")) => {
            on_table(tokens, at + 1, "trigger", "on", "triggers come later")
        }
        "event" if tokens.get(at).is_some_and(|token| token.is_word("trigger")) => {
            at += 1;
            skip(
                "event trigger",
                name_at(tokens, &mut at),
                "event triggers come later",
            )
        }
        "rule" => on_table(tokens, at, "rule", "to", "rules come later"),
        "policy" => on_table(
            tokens,
            at,
            "policy",
            "on",
            "row-level security policies come later",
        ),
        "materialized" => {
            at += 1;
            skip_if_not_exists(tokens, &mut at);
            skip(
                "materialized view",
                name_at(tokens, &mut at),
                "materialized views come later",
            )
        }
        "type" => Action::Run(Effect::Type(name_at(tokens, &mut at))),
        "domain" => Action::Run(Effect::Domain(name_at(tokens, &mut at))),
        "publication" | "subscription" => skip(
            if kind == "publication" {
                "publication"
            } else {
                "subscription"
            },
            name_at(tokens, &mut at),
            "logical replication comes later",
        ),
        "foreign" => {
            let table = tokens.get(at).is_some_and(|token| token.is_word("table"));
            skip_words(tokens, &mut at, &["table", "data", "wrapper"]);
            skip_if_not_exists(tokens, &mut at);
            skip(
                if table {
                    "foreign table"
                } else {
                    "foreign-data wrapper"
                },
                name_at(tokens, &mut at),
                "foreign data comes later",
            )
        }
        "server" => {
            skip_if_not_exists(tokens, &mut at);
            skip(
                "server",
                name_at(tokens, &mut at),
                "foreign data comes later",
            )
        }
        "user" if tokens.get(at).is_some_and(|token| token.is_word("mapping")) => {
            skip("user mapping", String::new(), "foreign data comes later")
        }
        "collation" | "conversion" | "cast" | "operator" | "text" | "language" | "access"
        | "statistics" | "transform" | "tablespace" | "role" | "user" | "group" | "database" => {
            let kind = other_kind(kind);
            skip_words(tokens, &mut at, &["search", "class", "family", "method"]);
            skip_words(
                tokens,
                &mut at,
                &["parser", "dictionary", "template", "configuration"],
            );
            skip_if_not_exists(tokens, &mut at);
            skip(kind, name_at(tokens, &mut at), other_reason(kind))
        }
        _ => Action::Run(Effect::OtherDdl),
    }
}

fn other_kind(word: &str) -> &'static str {
    match word {
        "collation" => "collation",
        "conversion" => "conversion",
        "cast" => "cast",
        "operator" => "operator",
        "text" => "text search object",
        "language" => "language",
        "access" => "access method",
        "statistics" => "statistics",
        "transform" => "transform",
        "tablespace" => "tablespace",
        "database" => "database",
        _ => "role",
    }
}

fn other_reason(kind: &str) -> &'static str {
    match kind {
        "role" => "roles come later; EggDB has one user per server",
        "database" => "one database per file",
        "tablespace" => "EggDB keeps a database in one file",
        "language" => "procedural languages come later",
        _ => "not in EggDB yet",
    }
}

fn on_table(
    tokens: &[Token],
    mut at: usize,
    kind: &'static str,
    keyword: &str,
    reason: &'static str,
) -> Action {
    let name = name_at(tokens, &mut at);
    let table = tokens
        .iter()
        .skip(at)
        .position(|token| token.is_word(keyword))
        .and_then(|offset| {
            let mut table_at = at + offset + 1;
            qualified_name(tokens, &mut table_at)
        });
    let shown = match table {
        Some(table) => format!("{name} on {table}"),
        None => name,
    };
    skip(kind, shown, reason)
}

fn create_table(tokens: &[Token], mut at: usize) -> Action {
    skip_if_not_exists(tokens, &mut at);
    let name = name_at(tokens, &mut at);
    if tokens
        .get(at)
        .is_some_and(|token| token.is_word("partition"))
        && tokens.get(at + 1).is_some_and(|token| token.is_word("of"))
    {
        return skipped_table(name, "partition", "partitioned tables come later");
    }
    if tokens.get(at).is_some_and(|token| token.is_word("of")) {
        return skipped_table(name, "typed", "typed tables come later");
    }
    let mut depth = 0u32;
    let mut constraints = 0u64;
    let mut previous: Option<&Token> = None;
    for token in &tokens[at..] {
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 1 && token.is_word("constraint") {
            constraints += 1;
        } else if depth == 0 {
            if token.is_word("inherits") {
                return skipped_table(name, "inherited", "table inheritance comes later");
            }
            if token.is_word("by") && previous.is_some_and(|previous| previous.is_word("partition"))
            {
                return skipped_table(name, "partitioned", "partitioned tables come later");
            }
        }
        previous = Some(token);
    }
    Action::Run(Effect::Table { name, constraints })
}

fn alter(tokens: &[Token]) -> Action {
    let mut words = tokens.len();
    while words > 0 && tokens[words - 1].is_symbol(b';') {
        words -= 1;
    }
    if words >= 3
        && tokens[words - 3].is_word("owner")
        && tokens[words - 2].is_word("to")
        && tokens[words - 1].name_part().is_some()
    {
        return Action::Ignore(Ignored::OwnerTo);
    }
    let Some(Token::Word(kind)) = tokens.first() else {
        return Action::Run(Effect::OtherDdl);
    };
    match kind.as_str() {
        "default" => Action::Ignore(Ignored::Other("ALTER DEFAULT PRIVILEGES")),
        "database" | "role" | "user" | "group" | "system" => {
            Action::Ignore(Ignored::Other("ALTER DATABASE and ALTER ROLE"))
        }
        "table" | "index" | "materialized" | "foreign" => alter_table(tokens),
        _ => Action::Run(Effect::OtherDdl),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlterKind {
    Storage,
    Trigger,
    RowSecurity,
    Partition,
    Inheritance,
    Change,
}

fn starts_with_words(tokens: &[Token], words: &[&str]) -> bool {
    words.len() <= tokens.len()
        && words
            .iter()
            .zip(tokens)
            .all(|(word, token)| token.is_word(word))
}

fn after_column(action: &[Token]) -> &[Token] {
    let mut at = 1;
    if action.get(at).is_some_and(|token| token.is_word("column")) {
        at += 1;
    }
    action.get(at + 1..).unwrap_or_default()
}

fn action_kind(action: &[Token]) -> AlterKind {
    let set_options = |rest: &[Token]| {
        (rest
            .first()
            .is_some_and(|token| token.is_word("set") || token.is_word("reset")))
            && rest.get(1).is_some_and(|token| token.is_symbol(b'('))
    };
    if starts_with_words(action, &["replica", "identity"])
        || starts_with_words(action, &["cluster", "on"])
        || starts_with_words(action, &["set", "without"])
        || starts_with_words(action, &["set", "tablespace"])
        || starts_with_words(action, &["set", "access", "method"])
        || starts_with_words(action, &["set", "logged"])
        || starts_with_words(action, &["set", "unlogged"])
        || set_options(action)
    {
        return AlterKind::Storage;
    }
    if action.first().is_some_and(|token| token.is_word("alter")) {
        let rest = after_column(action);
        if ["statistics", "storage", "compression"]
            .iter()
            .any(|word| starts_with_words(rest, &["set", word]))
            || set_options(rest)
        {
            return AlterKind::Storage;
        }
        return AlterKind::Change;
    }
    let toggles = ["enable", "disable"]
        .iter()
        .any(|word| action.first().is_some_and(|token| token.is_word(word)));
    let switched = if toggles { &action[1..] } else { action };
    if toggles
        && (starts_with_words(switched, &["trigger"])
            || starts_with_words(switched, &["always", "trigger"])
            || starts_with_words(switched, &["replica", "trigger"])
            || starts_with_words(switched, &["rule"])
            || starts_with_words(switched, &["always", "rule"])
            || starts_with_words(switched, &["replica", "rule"]))
    {
        return AlterKind::Trigger;
    }
    let forced = if starts_with_words(action, &["no", "force"]) {
        &action[2..]
    } else if starts_with_words(action, &["force"]) {
        &action[1..]
    } else {
        switched
    };
    if starts_with_words(forced, &["row", "level", "security"]) {
        return AlterKind::RowSecurity;
    }
    if starts_with_words(action, &["attach", "partition"])
        || starts_with_words(action, &["detach", "partition"])
    {
        return AlterKind::Partition;
    }
    if starts_with_words(action, &["inherit"]) || starts_with_words(action, &["no", "inherit"]) {
        return AlterKind::Inheritance;
    }
    AlterKind::Change
}

fn drops(action: &[Token]) -> bool {
    starts_with_words(action, &["drop"])
        || (action.first().is_some_and(|token| token.is_word("alter"))
            && starts_with_words(after_column(action), &["drop"]))
}

fn identity_sequence(action: &[Token]) -> Option<String> {
    let rest = after_column(action);
    if !starts_with_words(rest, &["add", "generated"]) {
        return None;
    }
    let at = rest
        .windows(2)
        .position(|pair| pair[0].is_word("sequence") && pair[1].is_word("name"))?;
    let mut name_at = at + 2;
    qualified_name(rest, &mut name_at)
}

fn alter_table(tokens: &[Token]) -> Action {
    let is_table = tokens.first().is_some_and(|token| token.is_word("table"));
    let mut at = 1;
    skip_words(tokens, &mut at, &["view", "table"]);
    if starts_with_words(&tokens[at.min(tokens.len())..], &["if", "exists"]) {
        at += 2;
    }
    skip_words(tokens, &mut at, &["only"]);
    let table = qualified_name(tokens, &mut at).unwrap_or_default();
    let mut actions: Vec<&[Token]> = Vec::new();
    let mut depth = 0u32;
    let mut start = at;
    let body = tokens.get(at..).unwrap_or_default();
    for (offset, token) in body.iter().enumerate() {
        let index = at + offset;
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && (token.is_symbol(b',') || token.is_symbol(b';')) {
            actions.push(&tokens[start..index]);
            start = index + 1;
        }
    }
    if start < tokens.len() {
        actions.push(&tokens[start..]);
    }
    actions.retain(|action| !action.is_empty());
    let kinds: Vec<AlterKind> = actions.iter().map(|action| action_kind(action)).collect();
    if kinds.contains(&AlterKind::RowSecurity) {
        return skip_change(
            "row-level security",
            table,
            "row-level security policies come later",
        );
    }
    if kinds.contains(&AlterKind::Partition) {
        return skip_change("partition", table, "partitioned tables come later");
    }
    if kinds.contains(&AlterKind::Inheritance) {
        return skip_change("inheritance", table, "table inheritance comes later");
    }
    if !kinds.is_empty() && kinds.iter().all(|kind| *kind == AlterKind::Storage) {
        return Action::Ignore(Ignored::Other("storage settings"));
    }
    if !kinds.is_empty() && kinds.iter().all(|kind| *kind == AlterKind::Trigger) {
        return Action::Ignore(Ignored::Other("ENABLE and DISABLE TRIGGER"));
    }
    if is_table && !actions.is_empty() && actions.iter().all(|action| drops(action)) {
        return Action::Run(Effect::Drop);
    }
    if !is_table {
        return Action::Run(Effect::OtherDdl);
    }
    let mut constraints = 0u64;
    let mut sequences = Vec::new();
    for action in &actions {
        if action.first().is_some_and(|token| token.is_word("add"))
            && action.get(1).is_some_and(|next| {
                [
                    "constraint",
                    "primary",
                    "unique",
                    "foreign",
                    "check",
                    "exclude",
                ]
                .iter()
                .any(|word| next.is_word(word))
            })
        {
            constraints += 1;
        }
        if let Some(sequence) = identity_sequence(action) {
            sequences.push(sequence);
        }
    }
    Action::Run(Effect::AlterTable {
        constraints,
        identity_sequences: sequences,
    })
}

pub(crate) fn sequence_call(text: &str) -> Option<String> {
    let tokens: Vec<Token> = Words::new(text).take(8).collect();
    let mut at = 1;
    if !tokens.first().is_some_and(|token| token.is_word("select")) {
        return None;
    }
    if starts_with_words(&tokens[1..], &["pg_catalog"])
        && tokens.get(2).is_some_and(|token| token.is_symbol(b'.'))
    {
        at = 3;
    }
    let function = tokens.get(at)?;
    if !["setval", "nextval", "currval"]
        .iter()
        .any(|name| function.is_word(name))
        || !tokens.get(at + 1)?.is_symbol(b'(')
    {
        return None;
    }
    let mut words = Words::new(text);
    words.find(|token| token.is_symbol(b'('))?;
    words.literal_text()
}

pub(crate) fn insert_rows(text: &str, rules: WordRules) -> u64 {
    let mut depth = 0u32;
    let mut after_values = false;
    let mut rows = 0u64;
    for token in Words::with_rules(text, rules) {
        if token.is_symbol(b'(') {
            if depth == 0 && after_values {
                rows += 1;
            }
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && token.is_word("values") {
            after_values = true;
        }
    }
    rows
}
