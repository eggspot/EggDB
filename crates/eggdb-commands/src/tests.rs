use crate::{
    Annotations, ArgumentTarget, COMMANDS, COMMON_OPTIONS, CORE_VERBS, Category, CommandDefinition,
    ConfirmCheck, ERROR_CODES, Effect, Grant, KEPT_NAMES, NOUNS, SHIPPED_TOOL_NAMES, TOOLS,
    category_of, command, tool,
};

const WITHOUT_A_FORMAT: &[&str] = &["ui", "shell", "mcp"];
const DESCRIBED_BY_THE_MAIN_HELP: &[&str] = &["help", "version"];

fn is_snake_case(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('_')
        && !name.ends_with('_')
        && !name.contains("__")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_core_verb(word: &str) -> bool {
    CORE_VERBS.iter().any(|verb| verb.name == word)
}

fn takes_long_option(command: &CommandDefinition, long: &str) -> bool {
    command
        .options
        .iter()
        .chain(COMMON_OPTIONS)
        .any(|option| option.long == long)
}

#[test]
fn every_command_has_the_fields_its_slice_requires() {
    for command in COMMANDS {
        let name = command.name();
        let main_help_only = DESCRIBED_BY_THE_MAIN_HELP.contains(&name.as_str());
        assert!(!command.words.is_empty(), "{name}");
        assert!(!command.summary.is_empty(), "{name}");
        assert!(
            command.description.ends_with('.'),
            "{name}: the description is whole sentences"
        );
        assert!(
            command.usage.starts_with(&format!("eggdb {name}")),
            "{name}: {}",
            command.usage
        );
        if main_help_only {
            assert!(command.help.is_empty(), "{name}");
        } else {
            assert!(!command.synopsis.is_empty(), "{name}");
            assert!(!command.operands.is_empty(), "{name}");
            assert!(command.help.ends_with('\n'), "{name}");
            assert!(command.help.contains("\nExamples:\n"), "{name}");
        }
        assert_eq!(
            command.has_json(),
            !WITHOUT_A_FORMAT.contains(&name.as_str()),
            "{name}: every command that takes --format has a JSON form"
        );
        assert_eq!(
            command.outputs.is_empty(),
            WITHOUT_A_FORMAT.contains(&name.as_str()),
            "{name}: only a command without --format declares no output"
        );
        assert!(!command.errors.is_empty(), "{name}");
        assert!(!command.examples.is_empty(), "{name}");
        assert!(!command.surfaces.is_empty(), "{name}");
        for example in command.examples {
            assert!(
                example.contains(&format!("eggdb {name}")),
                "{name}: {example}"
            );
            assert!(
                main_help_only || command.help.contains(example),
                "{name}: the help shows every example: {example}"
            );
        }
        for option in command.options {
            assert!(!option.long.is_empty() && !option.help.is_empty(), "{name}");
            assert!(
                !COMMON_OPTIONS.iter().any(|common| common.long == option.long),
                "{name}: --{} is a common option",
                option.long
            );
        }
    }
}

#[test]
fn every_command_is_a_kept_name_or_a_noun_and_a_core_verb() {
    for command in COMMANDS {
        let fits = match command.words {
            [word] => KEPT_NAMES.contains(word) || is_core_verb(word),
            [noun, verb] => {
                NOUNS.contains(noun) && !KEPT_NAMES.contains(noun) && is_core_verb(verb)
            }
            _ => false,
        };
        assert!(fits, "{}", command.name());
    }
    for kept in KEPT_NAMES {
        assert!(
            command(&[*kept]).is_some(),
            "{kept} is a kept name, so it is a command in the registry"
        );
    }
    for noun in NOUNS {
        let verbs: Vec<&CommandDefinition> = COMMANDS
            .iter()
            .filter(|command| command.words.first() == Some(noun))
            .collect();
        assert!(verbs.len() > 1, "{noun} has its verbs");
        assert!(
            verbs.iter().all(|verb| verb.help == verbs[0].help),
            "the commands of {noun} share one help page"
        );
    }
    let mut names: Vec<String> = COMMANDS.iter().map(CommandDefinition::name).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), COMMANDS.len(), "a command is defined twice");
}

#[test]
fn every_error_code_has_one_category_and_every_command_s_codes_are_known() {
    let mut codes: Vec<&str> = ERROR_CODES.iter().map(|known| known.code).collect();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), ERROR_CODES.len(), "an error code is listed twice");
    for known in ERROR_CODES {
        assert!(is_snake_case(known.code), "{}", known.code);
        assert!(!known.when.is_empty(), "{}", known.code);
    }
    for command in COMMANDS {
        let mut seen: Vec<&str> = Vec::new();
        for error in command.errors {
            assert!(
                category_of(error.code).is_some(),
                "{}: {} is not in ERROR_CODES",
                command.name(),
                error.code
            );
            assert!(
                (1..=5).contains(&error.exit),
                "{}: {} exits {}",
                command.name(),
                error.code,
                error.exit
            );
            assert!(!seen.contains(&error.code), "{}: {}", command.name(), error.code);
            seen.push(error.code);
        }
        assert_eq!(command.exit_for("invalid_arguments"), Some(2));
    }
    for known in ERROR_CODES {
        let mut exits: Vec<u8> = COMMANDS
            .iter()
            .filter_map(|command| command.exit_for(known.code))
            .collect();
        exits.sort_unstable();
        exits.dedup();
        assert!(
            exits.len() <= 1,
            "{} exits {exits:?}: one code, one exit in every command",
            known.code
        );
    }
    assert_eq!(category_of("database_busy"), Some(Category::Busy));
    assert_eq!(category_of("declined_by_user"), Some(Category::Declined));
    assert_eq!(category_of("no_such_code"), None);
}

#[test]
fn categories_and_verbs_are_distinct_snake_case_names() {
    let mut categories: Vec<&str> = Category::ALL.iter().map(|category| category.name()).collect();
    assert!(categories.iter().all(|name| is_snake_case(name)));
    categories.sort_unstable();
    categories.dedup();
    assert_eq!(categories.len(), Category::ALL.len());
    let mut verbs: Vec<&str> = CORE_VERBS.iter().map(|verb| verb.name).collect();
    verbs.sort_unstable();
    verbs.dedup();
    assert_eq!(verbs.len(), CORE_VERBS.len());
    for name in ["list", "show", "get", "sample", "explain", "export", "delete", "run"] {
        assert!(is_core_verb(name), "{name}");
    }
}

#[test]
fn new_tool_names_join_their_command_s_words_and_shipped_names_come_from_the_list() {
    let mut names: Vec<&str> = TOOLS.iter().map(|tool| tool.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), TOOLS.len(), "a tool is defined twice");
    for tool in TOOLS {
        assert!(is_snake_case(tool.name), "{}", tool.name);
        let joined = tool.command.map(|words| words.join("_"));
        assert!(
            SHIPPED_TOOL_NAMES.contains(&tool.name) || joined.as_deref() == Some(tool.name),
            "{} is neither shipped nor its command's words joined by _",
            tool.name
        );
    }
    for shipped in SHIPPED_TOOL_NAMES {
        assert!(tool(shipped).is_some(), "the shipped tool {shipped} is gone");
    }
}

#[test]
fn every_tool_s_annotations_follow_from_its_fields() {
    for tool in TOOLS {
        let annotations = tool.annotations();
        assert_eq!(annotations.read_only, tool.max_effect == Effect::Read, "{}", tool.name);
        assert_eq!(
            annotations.destructive,
            matches!(tool.max_effect, Effect::Destroy | Effect::Control),
            "{}",
            tool.name
        );
        assert_eq!(annotations.idempotent, tool.idempotent, "{}", tool.name);
        assert_eq!(annotations.open_world, tool.open_world, "{}", tool.name);
        if tool.confirm.is_some() {
            assert!(
                matches!(tool.max_effect, Effect::Destroy | Effect::Control),
                "{} confirms but cannot destroy",
                tool.name
            );
        }
        if tool.max_effect != Effect::Read {
            assert_ne!(tool.grant, Grant::Nothing, "{} writes without a grant", tool.name);
        }
        assert!(tool.offered(true), "{}", tool.name);
        assert_eq!(tool.offered(false), tool.grant == Grant::Nothing, "{}", tool.name);
    }
}

#[test]
fn the_shipped_tools_keep_their_annotations() {
    let reading = Annotations {
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    };
    for name in SHIPPED_TOOL_NAMES {
        let Some(tool) = tool(name) else {
            panic!("{name}");
        };
        let expected = if *name == "write_sql" {
            Annotations {
                read_only: false,
                destructive: true,
                idempotent: false,
                open_world: false,
            }
        } else {
            reading
        };
        assert_eq!(tool.annotations(), expected, "{name}");
    }
    let write = tool("write_sql").map(|tool| (tool.grant, tool.confirm));
    assert_eq!(
        write,
        Some((Grant::AllowWrite, Some(ConfirmCheck::DestructiveStatement)))
    );
}

#[test]
fn every_tool_maps_onto_its_cli_twin() {
    for tool in TOOLS {
        let Some(words) = tool.command else {
            panic!("{} has no CLI twin: from slice 2 every tool has one", tool.name);
        };
        let Some(twin) = command(words) else {
            panic!("{}'s command is not in the registry", tool.name);
        };
        assert!(
            twin.max_effect != Effect::Read || tool.max_effect == Effect::Read,
            "{}: a tool may not do more than its twin",
            tool.name
        );
        for fixed in tool.fixed {
            if let Some(long) = fixed.strip_prefix("--") {
                assert!(takes_long_option(twin, long), "{}: {fixed}", tool.name);
            }
        }
        for argument in tool.arguments {
            match argument.maps_to {
                ArgumentTarget::Option(long) | ArgumentTarget::OptionHex(long) => assert!(
                    takes_long_option(twin, long),
                    "{}: {} maps to --{long}",
                    tool.name,
                    argument.name
                ),
                ArgumentTarget::Operand(name) | ArgumentTarget::OperandHex(name) => assert!(
                    twin.operands.iter().any(|operand| operand.name == name),
                    "{}: {} maps to <{name}>",
                    tool.name,
                    argument.name
                ),
                ArgumentTarget::NoTwin => {
                    panic!("{}: {} has a twin but maps nowhere", tool.name, argument.name)
                }
                ArgumentTarget::Stdin | ArgumentTarget::CallLimit | ArgumentTarget::Confirmation => {}
            }
        }
        for field in tool.envelope {
            assert!(
                tool.result.iter().any(|result| result.name == *field),
                "{}: {field} is not a result field",
                tool.name
            );
        }
    }
}

#[test]
fn lookups_find_commands_by_words_and_tools_by_name() {
    assert_eq!(command(&["get"]).map(|found| found.usage), Some("eggdb get <file> <key> [-C <name>]"));
    assert!(command(&["frob"]).is_none());
    assert_eq!(command(&["delete"]).map(CommandDefinition::name), Some("del".to_owned()));
    assert_eq!(
        command(&["collection", "del"]).map(CommandDefinition::name),
        Some("collection delete".to_owned())
    );
    assert_eq!(
        command(&["table", "sample"]).map(|found| found.max_effect),
        Some(Effect::Read)
    );
    assert!(command(&["table", "drop"]).is_none());
    assert_eq!(
        tool("run_sql").map(|found| found.fixed),
        Some(&["--format", "json", "--read-only"][..])
    );
    assert_eq!(tool("kv_scan").map(|found| found.title), Some("Scan keys"));
    assert!(tool("drop_table").is_none());
    let run_sql = tool("run_sql").map(|found| (found.description_for(false), found.description_for(true)));
    let Some((read_only, writing)) = run_sql else {
        panic!("run_sql");
    };
    assert!(read_only.contains("read-only EggDB database"));
    assert!(writing.contains("send them with write_sql"));
}
