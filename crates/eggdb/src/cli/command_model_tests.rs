use super::Terminal;
use super::args::CommandName;
use super::help;
use super::tests::{TempDirectory, run_with};
use eggdb_commands::{COMMANDS, CORE_VERBS, KEPT_NAMES, TOOLS};
use eggdb_sql::json::{Json, Node, parse_document};

fn piped(args: &[&str]) -> super::tests::Ran {
    run_with(args, b"", Terminal::default())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn json_lines(bytes: &[u8]) -> Vec<Node> {
    text(bytes)
        .lines()
        .map(|line| parse_document(line).unwrap_or_else(|error| panic!("{line}: {error:?}")))
        .collect()
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(member, _)| member == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn string_at(node: &Node, path: &[&str]) -> Option<String> {
    let mut node = node;
    for step in path {
        node = member(node, step)?;
    }
    match &node.value {
        Json::String(text) | Json::Number(text) => Some(text.clone()),
        Json::Boolean(value) => Some(value.to_string()),
        _ => None,
    }
}

fn items<'n>(node: &'n Node, name: &str) -> &'n [Node] {
    match member(node, name).map(|node| &node.value) {
        Some(Json::Array(items)) => items,
        other => panic!("{name} is not an array: {other:?}"),
    }
}

fn one_json_error(stderr: &[u8]) -> Node {
    let mut errors = json_lines(stderr);
    assert_eq!(errors.len(), 1, "{}", text(stderr));
    errors.remove(0)
}

fn sample(directory: &TempDirectory) -> String {
    let file = directory.file("app.db");
    for (key, value) in [("user:1", "Ada"), ("user:2", "Grace"), ("other", "x")] {
        let created = piped(&["put", "--create", &file, key, value]);
        assert_eq!(created.exit, 0, "{}", text(&created.stderr));
    }
    file
}

#[test]
fn json_is_the_same_as_format_json_and_the_last_format_wins() {
    let directory = TempDirectory::new("json-alias");
    let file = sample(&directory);
    let out = directory.file("out.jsonl");
    let on_a_terminal = Terminal {
        stdout: true,
        ..Terminal::default()
    };
    for args in [
        vec!["get", file.as_str(), "user:1"],
        vec!["scan", file.as_str(), "--prefix", "user:"],
        vec!["check", file.as_str()],
        vec!["stats", file.as_str()],
    ] {
        let mut long = args.clone();
        long.extend(["--format", "json"]);
        let mut short = args.clone();
        short.push("--json");
        let long = run_with(&long, b"", on_a_terminal);
        let short = run_with(&short, b"", on_a_terminal);
        assert_eq!(long.exit, 0, "{args:?}: {}", text(&long.stderr));
        assert_eq!(text(&long.stdout), text(&short.stdout), "{args:?}");
        assert!(!json_lines(&short.stdout).is_empty(), "{args:?}");
    }
    let exported = piped(&["export", &file, &out, "--json"]);
    assert_eq!(exported.exit, 0, "{}", text(&exported.stderr));
    assert_eq!(
        string_at(&json_lines(&exported.stdout)[0], &["entries"]).as_deref(),
        Some("3")
    );
    let last_wins = run_with(&["get", &file, "user:1", "--json", "--format", "text"], b"", on_a_terminal);
    assert_eq!(text(&last_wins.stdout), "Ada\n");
    let json_last = run_with(&["get", &file, "user:1", "--format", "text", "--json"], b"", on_a_terminal);
    assert_eq!(text(&json_last.stdout), "{\"key\":\"user:1\",\"value\":\"Ada\"}\n");
}

#[test]
fn errors_are_one_json_line_only_when_json_was_asked_for() {
    let directory = TempDirectory::new("json-errors");
    let file = sample(&directory);
    let missing = directory.file("missing.db");
    let as_text = piped(&["get", &missing, "k"]);
    assert_eq!(as_text.exit, 3);
    assert!(
        text(&as_text.stderr).starts_with(&format!("eggdb: cannot open {missing}: no such file.")),
        "{}",
        text(&as_text.stderr)
    );
    let as_json = piped(&["get", &missing, "k", "--json"]);
    assert_eq!(as_json.exit, 3);
    assert!(as_json.stdout.is_empty());
    let error = one_json_error(&as_json.stderr);
    assert_eq!(string_at(&error, &["error", "code"]).as_deref(), Some("not_found"));
    assert_eq!(string_at(&error, &["error", "category"]).as_deref(), Some("not_found"));
    let message = string_at(&error, &["error", "message"]).unwrap_or_default();
    assert!(
        text(&as_text.stderr).starts_with(&format!("eggdb: {message}\n")),
        "the JSON message is the text message: {message}"
    );
    assert!(string_at(&error, &["error", "fix_command"]).is_none());

    let write = piped(&["put", &missing, "k", "v", "--format", "json"]);
    let error = one_json_error(&write.stderr);
    assert_eq!(
        string_at(&error, &["error", "fix_command"]),
        Some(format!("eggdb create {missing}"))
    );

    let usage = piped(&["get", &file, "user:1", "--frob", "--json"]);
    assert_eq!(usage.exit, 2);
    let error = one_json_error(&usage.stderr);
    assert_eq!(string_at(&error, &["error", "code"]).as_deref(), Some("invalid_arguments"));
    assert_eq!(string_at(&error, &["error", "category"]).as_deref(), Some("usage"));
    assert_eq!(
        string_at(&error, &["error", "message"]).as_deref(),
        Some("unknown option '--frob' for eggdb get")
    );
    assert_eq!(
        string_at(&error, &["error", "usage"]).as_deref(),
        Some("eggdb get <file> <key> [-C <name>]")
    );
    assert_eq!(
        string_at(&error, &["error", "fix_command"]).as_deref(),
        Some("eggdb help get")
    );
    let unknown = piped(&["frob", &file, "--json"]);
    assert_eq!(
        string_at(&one_json_error(&unknown.stderr), &["error", "fix_command"]).as_deref(),
        Some("eggdb help")
    );

    let no_format = piped(&["ui", &file, "--json"]);
    assert_eq!(no_format.exit, 2);
    assert!(
        text(&no_format.stderr).starts_with("eggdb: unknown option '--json' for eggdb ui\n"),
        "{}",
        text(&no_format.stderr)
    );

    let collection = piped(&["get", &file, "user:1", "-C", "nope", "--json"]);
    assert_eq!(collection.exit, 1);
    let error = one_json_error(&collection.stderr);
    assert_eq!(
        string_at(&error, &["error", "code"]).as_deref(),
        Some("collection_not_found")
    );
    assert_eq!(string_at(&error, &["error", "category"]).as_deref(), Some("not_found"));

    let out = directory.file("taken.jsonl");
    std::fs::write(&out, b"x").unwrap();
    let exists = piped(&["export", &file, &out, "--json"]);
    assert_eq!(exists.exit, 3);
    let error = one_json_error(&exists.stderr);
    assert_eq!(string_at(&error, &["error", "code"]).as_deref(), Some("already_exists"));
    assert_eq!(string_at(&error, &["error", "category"]).as_deref(), Some("exists"));

    let absent = piped(&["get", &file, "nobody", "--json"]);
    assert_eq!(absent.exit, 1);
    assert!(absent.stdout.is_empty() && absent.stderr.is_empty());
}

#[test]
fn sql_prints_one_json_line_per_statement_and_keeps_its_text_in_a_pipe() {
    let directory = TempDirectory::new("sql-json");
    let file = directory.file("app.db");
    let ran = piped(&[
        "sql",
        "--create",
        &file,
        "--json",
        "-c",
        "CREATE TABLE users (id integer PRIMARY KEY, name text); INSERT INTO users VALUES (1, 'Ada'), (2, NULL); SELECT id, name FROM users ORDER BY id",
    ]);
    assert_eq!(ran.exit, 0, "{}", text(&ran.stderr));
    let results = json_lines(&ran.stdout);
    assert_eq!(results.len(), 3, "{}", text(&ran.stdout));
    assert_eq!(string_at(&results[1], &["tag"]).as_deref(), Some("INSERT 0 2"));
    assert!(member(&results[1], "rows").is_none());
    let selected = &results[2];
    assert_eq!(string_at(selected, &["row_count"]).as_deref(), Some("2"));
    let columns = items(selected, "columns");
    assert_eq!(string_at(&columns[1], &["name"]).as_deref(), Some("name"));
    let rows = items(selected, "rows");
    let Json::Array(first) = &rows[0].value else {
        panic!("{:?}", rows[0]);
    };
    assert_eq!(first[0].value, Json::Number("1".to_owned()));
    assert_eq!(first[1].value, Json::String("Ada".to_owned()));
    let Json::Array(second) = &rows[1].value else {
        panic!("{:?}", rows[1]);
    };
    assert_eq!(second[1].value, Json::Null);

    let as_text = piped(&["sql", &file, "-c", "SELECT name FROM users WHERE id = 1"]);
    let shown = text(&as_text.stdout);
    assert!(shown.starts_with(" name") && shown.ends_with("(1 row)\n\n"), "{shown}");

    let failed = piped(&["sql", &file, "--json", "-c", "SELECT * FROM nowhere"]);
    assert_eq!(failed.exit, 3);
    assert!(failed.stdout.is_empty());
    let error = one_json_error(&failed.stderr);
    assert_eq!(string_at(&error, &["error", "code"]).as_deref(), Some("sql_error"));
    assert_eq!(string_at(&error, &["error", "category"]).as_deref(), Some("failed"));
    assert_eq!(string_at(&error, &["error", "sqlstate"]).as_deref(), Some("42P01"));

    let hex = piped(&["sql", &file, "--format", "hex", "-c", "SELECT 1"]);
    assert_eq!(hex.exit, 2);
    assert!(
        text(&hex.stderr)
            .starts_with("eggdb: --format hex is for get and scan; eggdb sql prints text or json\n"),
        "{}",
        text(&hex.stderr)
    );
}

#[test]
fn help_json_lists_exactly_the_registry() {
    for args in [
        &["help", "--json"][..],
        &["--help", "--json"],
        &["-h", "--format", "json"],
        &["--json"],
    ] {
        let ran = piped(args);
        assert_eq!(ran.exit, 0, "{args:?}: {}", text(&ran.stderr));
        let documents = json_lines(&ran.stdout);
        assert_eq!(documents.len(), 1, "{args:?}");
        let registry = &documents[0];
        let commands: Vec<String> = items(registry, "commands")
            .iter()
            .map(|command| {
                let words: Vec<String> = items(command, "words")
                    .iter()
                    .filter_map(|word| match &word.value {
                        Json::String(word) => Some(word.clone()),
                        _ => None,
                    })
                    .collect();
                words.join(" ")
            })
            .collect();
        let expected: Vec<String> = COMMANDS.iter().map(|command| command.name()).collect();
        assert_eq!(commands, expected, "{args:?}");
        let tools: Vec<String> = items(registry, "tools")
            .iter()
            .filter_map(|tool| string_at(tool, &["name"]))
            .collect();
        let expected: Vec<&str> = TOOLS.iter().map(|tool| tool.name).collect();
        assert_eq!(tools, expected, "{args:?}");
        assert!(items(registry, "other_commands").is_empty(), "{args:?}");
        assert_eq!(items(registry, "categories").len(), 13);
    }
    for args in [&["help", "get", "--json"][..], &["get", "--help", "--json"]] {
        let ran = piped(args);
        assert_eq!(ran.exit, 0);
        let command = &json_lines(&ran.stdout)[0];
        assert_eq!(
            string_at(command, &["usage"]).as_deref(),
            Some("eggdb get <file> <key> [-C <name>]")
        );
        assert_eq!(string_at(command, &["max_effect"]).as_deref(), Some("read"));
        assert_eq!(
            string_at(command, &["default_format", "pipe"]).as_deref(),
            Some("raw")
        );
    }
    let put = piped(&["help", "put", "--json"]);
    assert_eq!(put.exit, 0);
    let put = &json_lines(&put.stdout)[0];
    assert_eq!(
        string_at(put, &["usage"]),
        Some(help::usage_line(CommandName::Put))
    );
    assert_eq!(string_at(put, &["max_effect"]).as_deref(), Some("change"));
    assert!(member(put, "outputs").is_some());
    let table = piped(&["help", "table", "--json"]);
    assert_eq!(table.exit, 0, "{}", text(&table.stderr));
    let verbs: Vec<String> = json_lines(&table.stdout)
        .iter()
        .filter_map(|command| string_at(command, &["usage"]))
        .collect();
    assert_eq!(
        verbs,
        help::definitions(CommandName::Table)
            .iter()
            .map(|definition| definition.usage.to_owned())
            .collect::<Vec<_>>()
    );
    for args in [&["help", "table", "sample", "--json"][..], &["table", "sample", "--help", "--json"]] {
        let ran = piped(args);
        assert_eq!(ran.exit, 0, "{args:?}");
        let lines = json_lines(&ran.stdout);
        assert_eq!(lines.len(), 1, "{args:?}");
        assert_eq!(
            string_at(&lines[0], &["usage"]).as_deref(),
            Some("eggdb table sample <file> <table> [-n <count>]")
        );
    }
    let collection = piped(&["help", "collection", "del"]);
    assert_eq!(collection.exit, 0);
    assert_eq!(text(&collection.stdout), help::command_help(CommandName::Collection));
    let wrong_verb = piped(&["help", "table", "frob"]);
    assert_eq!(wrong_verb.exit, 2);
    assert!(text(&wrong_verb.stderr).starts_with("eggdb: table needs list, show or sample\n"));
    let ui = piped(&["help", "ui", "--json"]);
    let ui = &json_lines(&ui.stdout)[0];
    assert!(items(ui, "outputs").is_empty());
    assert_eq!(member(ui, "default_format").map(|node| &node.value), Some(&Json::Null));
    let version = piped(&["version", "--json"]);
    assert_eq!(
        text(&version.stdout),
        format!("{{\"version\":\"{}\"}}\n", eggdb::VERSION)
    );
    assert_eq!(text(&piped(&["version"]).stdout), format!("eggdb {}\n", eggdb::VERSION));
    let unknown = piped(&["help", "frob", "--json"]);
    assert_eq!(unknown.exit, 2);
    let error = one_json_error(&unknown.stderr);
    assert_eq!(string_at(&error, &["error", "code"]).as_deref(), Some("invalid_arguments"));
}

fn main_help_line(left: &str, summary: &str) -> String {
    if left.chars().count() < 30 {
        format!("\n  {left:<29} {summary}\n")
    } else {
        format!("\n  {left}\n{}{summary}\n", " ".repeat(32))
    }
}

#[test]
fn the_text_help_comes_from_the_registry() {
    for command in CommandName::ALL {
        let definitions = help::definitions(command);
        let Some(first) = definitions.first() else {
            panic!("{} is not in the registry", command.name());
        };
        let usages: Vec<&str> = definitions.iter().map(|definition| definition.usage).collect();
        assert_eq!(
            help::command_help(command),
            format!("Usage: {}\n\n{}", usages.join("\n       "), first.help)
        );
        let ran = piped(&["help", command.name()]);
        assert_eq!(text(&ran.stdout), help::command_help(command));
        let listed = match definitions.as_slice() {
            [definition] => main_help_line(
                format!("{:<6} {}", command.name(), definition.synopsis).trim_end(),
                definition.summary,
            ),
            verbs => {
                let verbs: Vec<&str> = verbs.iter().map(|verb| verb.words[1]).collect();
                format!("\n  {:<6} {} ", command.name(), verbs.join("|"))
            }
        };
        assert!(help::MAIN.contains(&listed), "the main help lists {listed:?}");
    }
    for name in ["help", "version"] {
        let Some(definition) = eggdb_commands::command(&[name]) else {
            panic!("{name} is not in the registry");
        };
        let listed = main_help_line(
            format!("{name:<6} {}", definition.synopsis).trim_end(),
            definition.summary,
        );
        assert!(help::MAIN.contains(&listed), "the main help lists {listed:?}");
    }
}

#[test]
fn every_command_is_a_kept_name_or_a_noun_and_its_core_verbs() {
    let is_core_verb = |word: &str| CORE_VERBS.iter().any(|core| core.name == word);
    for command in CommandName::ALL {
        let name = command.name();
        let fits = match command {
            CommandName::Collection | CommandName::Table => help::definitions(command)
                .iter()
                .all(|definition| definition.words.len() == 2 && is_core_verb(definition.words[1])),
            _ => KEPT_NAMES.contains(&name) || is_core_verb(name),
        };
        assert!(fits, "{name} is neither a kept name nor a noun with core verbs");
    }
    for kept in KEPT_NAMES {
        assert!(
            ["help", "version"].contains(kept)
                || CommandName::ALL.iter().any(|command| command.name() == *kept),
            "{kept} is kept but is no command"
        );
    }
}

#[test]
fn the_command_model_doc_lists_every_command_tool_and_error_code() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/ux/command-model.md");
    let doc = std::fs::read_to_string(&path).unwrap();
    for command in COMMANDS {
        assert!(doc.contains(&format!("| `{}` |", command.name())), "{}", command.name());
    }
    for tool in TOOLS {
        assert!(doc.contains(&format!("| `{}` |", tool.name)), "{}", tool.name);
    }
    for known in eggdb_commands::ERROR_CODES {
        assert!(
            doc.contains(&format!("| `{}` | `{}` |", known.code, known.category.name())),
            "{} {}",
            known.code,
            known.category.name()
        );
    }
    for kept in KEPT_NAMES {
        assert!(doc.contains(&format!("`{kept}`")), "{kept}");
    }
}

pub(super) fn words_of(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for character in line.chars() {
        match (quote, character) {
            (Some(open), character) if character == open => quote = None,
            (Some(_), character) => word.push(character),
            (None, '"' | '\'') => {
                quote = Some(character);
                in_word = true;
            }
            (None, character) if character.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, character) => {
                word.push(character);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

pub(super) fn is_one_eggdb_command(example: &str) -> bool {
    example.starts_with("eggdb ")
        && !["|", "<", ">", "$(", "&&"]
            .iter()
            .any(|shell| example.contains(shell))
}

#[test]
fn every_example_is_a_command_line_the_parser_takes() {
    let mut parsed = 0;
    for definition in COMMANDS {
        for example in definition.examples {
            let example = match example.find("eggdb ") {
                Some(start) => &example[start..],
                None => panic!("{}: {example}", definition.name()),
            };
            let Some(line) = example.split(" | ").next().filter(|line| is_one_eggdb_command(line))
            else {
                continue;
            };
            let words = words_of(line);
            let args: Vec<std::ffi::OsString> =
                words[1..].iter().map(std::ffi::OsString::from).collect();
            match super::args::parse(&args) {
                Ok(_) => parsed += 1,
                Err(failure) => panic!("{}: {line}: {failure:?}", definition.name()),
            }
        }
    }
    assert!(parsed > COMMANDS.len(), "{parsed} examples parsed");
}

#[test]
fn every_command_of_the_parser_is_in_the_registry_with_its_json_form() {
    for command in CommandName::ALL {
        let definitions = help::definitions(command);
        assert!(!definitions.is_empty(), "{} is not in the registry", command.name());
        for definition in definitions {
            assert_eq!(
                definition.has_json(),
                command.takes_format_option(),
                "{}",
                definition.name()
            );
        }
    }
    for definition in COMMANDS {
        let first = definition.words[0];
        assert!(
            ["help", "version"].contains(&first) || CommandName::named(first).is_some(),
            "{} is in the registry but the parser does not know it",
            definition.name()
        );
    }
}
