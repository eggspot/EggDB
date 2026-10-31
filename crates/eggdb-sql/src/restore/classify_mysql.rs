use super::classify::{Action, Effect, Ignored};
use super::words::{Token, WordRules, Words, plain_qualified_name};

const HEAD_TOKENS: usize = 48;
const ONE_DATABASE: &str = "CREATE DATABASE and USE (one database per file)";

pub(crate) fn mysql_table_key(name: &str, default_schema: &str) -> String {
    format!("{default_schema}.{}", mysql_name_key(name))
}

pub(crate) fn mysql_name_key(name: &str) -> String {
    name.rsplit_once('.').map_or(name, |(_, last)| last).to_owned()
}

pub(crate) fn mysql_command_reason(_name: &str) -> &'static str {
    "never run"
}

fn head(text: &str) -> Vec<Token> {
    Words::with_rules(text, WordRules::MYSQL).take(HEAD_TOKENS).collect()
}

fn is(tokens: &[Token], at: usize, word: &str) -> bool {
    tokens.get(at).is_some_and(|token| token.is_word(word))
}

fn name_after(tokens: &[Token], mut at: usize) -> String {
    if is(tokens, at, "if") {
        at += 1;
        while is(tokens, at, "not") || is(tokens, at, "exists") {
            at += 1;
        }
    }
    plain_qualified_name(tokens, &mut at).unwrap_or_default()
}

fn skip_create_modifiers(tokens: &[Token], mut at: usize) -> usize {
    loop {
        if is(tokens, at, "or") && is(tokens, at + 1, "replace") {
            at += 2;
        } else if is(tokens, at, "algorithm") && tokens.get(at + 1).is_some_and(|token| token.is_symbol(b'=')) {
            at += 3;
        } else if is(tokens, at, "definer") && tokens.get(at + 1).is_some_and(|token| token.is_symbol(b'=')) {
            at += 3;
            if tokens.get(at).is_some_and(|token| token.is_symbol(b'@')) {
                at += 2;
            }
            if is(tokens, at - 1, "current_user") && tokens.get(at).is_some_and(|token| token.is_symbol(b'(')) {
                at += 2;
            }
        } else if is(tokens, at, "sql") && is(tokens, at + 1, "security") {
            at += 3;
        } else if ["temporary", "unique", "fulltext", "spatial", "aggregate", "online", "offline"].iter().any(|word| is(tokens, at, word)) {
            at += 1;
        } else {
            return at;
        }
    }
}

fn create(tokens: &[Token]) -> Action {
    let at = skip_create_modifiers(tokens, 1);
    let Some(Token::Word(kind)) = tokens.get(at) else {
        return Action::Run(Effect::OtherDdl);
    };
    match kind.as_str() {
        "table" => Action::Run(Effect::Table { name: name_after(tokens, at + 1), constraints: 0 }),
        "view" => Action::Run(Effect::View(name_after(tokens, at + 1))),
        "index" => Action::Run(Effect::Index(name_after(tokens, at + 1))),
        "database" | "schema" => Action::Ignore(Ignored::Other(ONE_DATABASE)),
        "user" | "role" => Action::Ignore(Ignored::Other("CREATE USER and CREATE ROLE")),
        "trigger" | "procedure" | "function" | "event" => {
            let routine_kind = match kind.as_str() {
                "trigger" => "trigger",
                "procedure" => "procedure",
                "function" => "function",
                _ => "event",
            };
            Action::Run(Effect::Routine { kind: routine_kind, name: name_after(tokens, at + 1) })
        }
        _ => Action::Run(Effect::OtherDdl),
    }
}

fn set(tokens: &[Token]) -> Action {
    let mut at = 1;
    while ["session", "local"].iter().any(|word| is(tokens, at, word)) {
        at += 1;
    }
    if ["global", "persist", "persist_only"].iter().any(|word| is(tokens, at, word)) {
        return Action::Ignore(Ignored::Set(name_after(tokens, at + 1)));
    }
    let system = tokens.get(at).is_some_and(|token| token.is_symbol(b'@')) && tokens.get(at + 1).is_some_and(|token| token.is_symbol(b'@'));
    if system {
        at += 2;
        let scope = tokens.get(at).and_then(Token::plain_name_part).map(str::to_ascii_lowercase);
        if tokens.get(at + 1).is_some_and(|token| token.is_symbol(b'.')) {
            if matches!(scope.as_deref(), Some("global" | "persist")) {
                return Action::Ignore(Ignored::Set(name_after(tokens, at + 2)));
            }
            at += 2;
        }
    }
    let name = tokens.get(at).and_then(Token::plain_name_part).map(str::to_ascii_lowercase).unwrap_or_default();
    match name.as_str() {
        "sql_log_bin" | "gtid_next" | "gtid_purged" => Action::Ignore(Ignored::Set(name)),
        _ => Action::Run(Effect::Setting),
    }
}

pub(crate) fn classify_mysql(text: &str, _known_parameter: &dyn Fn(&str) -> bool) -> Action {
    let tokens = head(text);
    let Some(Token::Word(first)) = tokens.first() else {
        return Action::Run(Effect::Other);
    };
    match first.as_str() {
        "create" => create(&tokens),
        "use" => Action::Ignore(Ignored::Other(ONE_DATABASE)),
        "drop" if is(&tokens, 1, "database") || is(&tokens, 1, "schema") => Action::Ignore(Ignored::Other(ONE_DATABASE)),
        "drop" if is(&tokens, 1, "user") || is(&tokens, 1, "role") => Action::Ignore(Ignored::Other("DROP USER and DROP ROLE")),
        "drop" => Action::Run(Effect::Drop),
        "insert" | "replace" => {
            let mut at = 1;
            while ["low_priority", "delayed", "high_priority", "ignore", "into"].iter().any(|word| is(&tokens, at, word)) {
                at += 1;
            }
            match plain_qualified_name(&tokens, &mut at) {
                Some(table) => Action::Run(Effect::Insert(table)),
                None => Action::Run(Effect::Data),
            }
        }
        "lock" | "unlock" => Action::Ignore(Ignored::Other("LOCK TABLES and UNLOCK TABLES")),
        "alter" if is(&tokens, 1, "table") && tokens.iter().skip(3).take(4).any(|token| token.is_word("keys")) => {
            Action::Ignore(Ignored::Other("ALTER TABLE ... DISABLE KEYS and ENABLE KEYS"))
        }
        "alter" if is(&tokens, 1, "database") || is(&tokens, 1, "schema") => Action::Ignore(Ignored::Other(ONE_DATABASE)),
        "alter" => Action::Run(Effect::AlterTable { constraints: 0, identity_sequences: Vec::new() }),
        "set" => set(&tokens),
        "grant" => Action::Ignore(Ignored::Grant),
        "revoke" => Action::Ignore(Ignored::Revoke),
        "start" | "begin" | "commit" | "rollback" => Action::Ignore(Ignored::Other("BEGIN and COMMIT")),
        "flush" | "change" | "reset" => Action::Ignore(Ignored::Other("FLUSH, CHANGE REPLICATION and RESET (server administration)")),
        "update" | "delete" | "truncate" | "select" | "with" | "load" => Action::Run(Effect::Data),
        _ => Action::Run(Effect::Other),
    }
}
