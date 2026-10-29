use super::Terminal;
use super::tests::{Ran, TempDirectory, run_with};

fn run(args: &[&str]) -> Ran {
    run_with(args, b"", Terminal::default())
}

fn on_terminal(args: &[&str]) -> Ran {
    let terminal = Terminal {
        stdin: true,
        stdout: true,
        stderr: true,
        stdout_color: false,
        stderr_color: false,
    };
    run_with(args, b"", terminal)
}

#[track_caller]
fn ok(args: &[&str]) -> Ran {
    let ran = run(args);
    assert_eq!(ran.exit, 0, "{args:?}: {}", stderr(&ran));
    ran
}

fn stdout(ran: &Ran) -> String {
    String::from_utf8_lossy(&ran.stdout).into_owned()
}

fn stderr(ran: &Ran) -> String {
    String::from_utf8_lossy(&ran.stderr).into_owned()
}

fn json(path: &str) -> String {
    path.replace('\\', "\\\\")
}

#[test]
fn collections_are_added_listed_used_and_deleted_from_the_command_line() {
    let directory = TempDirectory::new("collections-cli");
    let file = directory.file("app.db");
    ok(&["create", &file]);
    let listed = ok(&["collection", "list", &file]);
    assert_eq!(
        stdout(&listed),
        format!("{{\"file\":\"{}\",\"collections\":[]}}\n", json(&file))
    );
    let added = ok(&["collection", "add", &file, "sessions"]);
    assert_eq!(
        stdout(&added),
        format!(
            "{{\"file\":\"{}\",\"collection\":\"sessions\",\"added\":true}}\n",
            json(&file)
        )
    );
    let again = on_terminal(&["collection", "add", &file, "sessions"]);
    assert_eq!(again.exit, 1);
    assert!(
        stderr(&again).contains("nothing to add: sessions is already a collection in"),
        "{}",
        stderr(&again)
    );
    let text = on_terminal(&["collection", "add", &file, "cache"]);
    assert_eq!(text.exit, 0);
    assert!(stdout(&text).starts_with("added collection cache to "), "{}", stdout(&text));
    let names = on_terminal(&["collection", "list", &file]);
    assert_eq!(stdout(&names), "cache\nsessions\n");
    assert_eq!(stderr(&names), "2 collections\n");

    ok(&["put", &file, "k", "default"]);
    ok(&["put", &file, "-C", "sessions", "k", "in sessions"]);
    ok(&["put", &file, "--collection", "sessions", "s:2", "two"]);
    let got = ok(&["get", &file, "k", "-C", "sessions"]);
    assert_eq!(got.stdout, b"in sessions");
    assert_eq!(ok(&["get", &file, "k"]).stdout, b"default");
    let scanned = ok(&["scan", &file, "-C", "sessions", "-k"]);
    assert_eq!(stdout(&scanned), "{\"key\":\"k\"}\n{\"key\":\"s:2\"}\n");
    let default_scan = ok(&["scan", &file, "-k"]);
    assert_eq!(stdout(&default_scan), "{\"key\":\"k\"}\n");
    let deleted = ok(&["del", &file, "-C", "sessions", "s:2"]);
    assert!(stdout(&deleted).contains("\"deleted\":true"));
    let gone = run(&["del", &file, "-C", "sessions", "s:2"]);
    assert_eq!(gone.exit, 1);

    let dropped = on_terminal(&["collection", "delete", &file, "sessions"]);
    assert_eq!(dropped.exit, 0, "{}", stderr(&dropped));
    assert!(stdout(&dropped).starts_with("deleted collection sessions from "));
    let missing = on_terminal(&["collection", "del", &file, "sessions"]);
    assert_eq!(missing.exit, 1);
    assert!(
        stderr(&missing).contains("nothing to delete: sessions is not a collection in"),
        "{}",
        stderr(&missing)
    );
    assert_eq!(ok(&["get", &file, "k"]).stdout, b"default");
}

#[test]
fn a_missing_collection_is_not_found_and_names_the_command_that_adds_it() {
    let directory = TempDirectory::new("collections-missing");
    let file = directory.file("app.db");
    ok(&["create", &file]);
    let f = file.as_str();
    for (args, context) in [
        (vec!["get", f, "-C", "orders", "k"], "cannot read k from"),
        (vec!["del", f, "-C", "orders", "k"], "cannot delete k from"),
        (vec!["scan", f, "-C", "orders"], "cannot scan"),
        (vec!["export", f, "-C", "orders"], "cannot export"),
        (vec!["put", f, "-C", "orders", "k", "v"], "cannot store k in"),
    ] {
        let ran = run(&args);
        assert_eq!(ran.exit, 1, "{args:?}: {}", stderr(&ran));
        assert!(ran.stdout.is_empty(), "{args:?}");
        let message = stderr(&ran);
        assert!(message.contains(context), "{message}");
        assert!(
            message.contains(&format!(
                "no collection named orders. Add it with `eggdb collection add {file} orders`"
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!("  try: eggdb collection add {file} orders\n")),
            "{message}"
        );
    }
    let put = run(&["put", &file, "-C", "orders", "k", "v"]);
    assert!(stderr(&put).contains("or pass --create to put."), "{}", stderr(&put));
    ok(&["put", "--create", &file, "-C", "orders", "k", "v"]);
    assert_eq!(ok(&["get", &file, "-C", "orders", "k"]).stdout, b"v");
    let fresh = directory.file("fresh.db");
    ok(&["put", "--create", &fresh, "-C", "cache", "c:1", "hot"]);
    assert_eq!(ok(&["get", &fresh, "-C", "cache", "c:1"]).stdout, b"hot");
    let bad = run(&["get", &file, "-C", "two words", "k"]);
    assert_eq!(bad.exit, 2);
    assert!(
        stderr(&bad).contains(
            "-C needs a name of 1 to 128 bytes with no spaces or control characters, not 'two words'"
        ),
        "{}",
        stderr(&bad)
    );
}

#[test]
fn a_whole_file_export_carries_its_collections_into_an_import() {
    let directory = TempDirectory::new("collections-export");
    let file = directory.file("app.db");
    ok(&["create", &file]);
    ok(&["put", &file, "user:1", "Ada"]);
    ok(&["collection", "add", &file, "cache"]);
    ok(&["put", "--create", &file, "-C", "sessions", "s:1", "{\"user\":1}"]);
    ok(&["put", &file, "-C", "sessions", "0x00ff", "0xdead"]);
    let export = ok(&["export", &file]);
    let text = stdout(&export);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].contains("\"version\":2"), "{text}");
    assert_eq!(
        &lines[1..],
        [
            "{\"key\":\"user:1\",\"value\":\"Ada\"}",
            "{\"collection\":\"cache\"}",
            "{\"collection\":\"sessions\",\"key_hex\":\"00ff\",\"value_hex\":\"dead\"}",
            "{\"collection\":\"sessions\",\"key\":\"s:1\",\"value\":\"{\\\"user\\\":1}\"}",
            "{\"entries\":3}",
        ]
    );
    let copy = directory.file("copy.db");
    let imported = run_with(
        &["import", "--create", &copy],
        text.as_bytes(),
        Terminal::default(),
    );
    assert_eq!(imported.exit, 0, "{}", stderr(&imported));
    assert!(stdout(&imported).contains("\"collections_added\":2"), "{}", stdout(&imported));
    let names = ok(&["collection", "list", &copy]);
    assert!(stdout(&names).contains("[\"cache\",\"sessions\"]"), "{}", stdout(&names));
    assert_eq!(ok(&["get", &copy, "-C", "sessions", "s:1"]).stdout, b"{\"user\":1}");
    assert_eq!(ok(&["get", &copy, "user:1"]).stdout, b"Ada");
    let again = stdout(&ok(&["export", &copy]));
    assert_eq!(again.lines().skip(1).collect::<Vec<_>>(), lines[1..].to_vec());

    let only = ok(&["export", &file, "-C", "sessions"]);
    let only_text = stdout(&only);
    assert!(only_text.lines().next().unwrap_or_default().contains("\"version\":1"));
    assert!(!only_text.contains("\"collection\""), "{only_text}");
    let archive = directory.file("archive.db");
    let flattened = run_with(
        &["import", "--create", &archive, "-C", "archive"],
        text.as_bytes(),
        Terminal::default(),
    );
    assert_eq!(flattened.exit, 0, "{}", stderr(&flattened));
    assert!(stdout(&flattened).contains("\"imported\":3"), "{}", stdout(&flattened));
    let in_archive = ok(&["scan", &archive, "-C", "archive", "-k"]);
    assert_eq!(
        stdout(&in_archive),
        "{\"key_hex\":\"00ff\"}\n{\"key\":\"s:1\"}\n{\"key\":\"user:1\"}\n"
    );
    assert_eq!(ok(&["scan", &archive, "-k"]).stdout, b"");
    let names = ok(&["collection", "list", &archive]);
    assert!(stdout(&names).contains("[\"archive\"]"), "{}", stdout(&names));
}
