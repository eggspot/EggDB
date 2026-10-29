use super::Terminal;
use super::tests::{Ran, TempDirectory, run_with};
use std::time::Duration;

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

fn created(directory: &TempDirectory) -> String {
    let file = directory.file("app.db");
    ok(&["create", &file]);
    file
}

fn number_after(text: &str, field: &str) -> u64 {
    let start = text.find(field).unwrap_or_else(|| panic!("{field} in {text}")) + field.len();
    text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

#[test]
fn a_time_to_live_is_set_read_changed_and_removed_from_the_command_line() {
    let directory = TempDirectory::new("expiry-cli-ttl");
    let file = created(&directory);
    let stored = ok(&["put", &file, "s:1", "alice", "--ttl", "30m"]);
    let line = stdout(&stored);
    assert!(line.starts_with("{\"key\":\"s:1\",\"stored\":true,\"ttl_ms\":1800000,\"expires_at\":\""), "{line}");
    let left = on_terminal(&["ttl", &file, "s:1"]);
    assert_eq!(left.exit, 0);
    let shown = stdout(&left);
    let parsed = eggdb_ui::parse_time_to_live(shown.trim_end()).unwrap();
    assert!(parsed <= Duration::from_secs(1800) && parsed >= Duration::from_secs(1790), "{shown}");
    let json = stdout(&ok(&["ttl", &file, "s:1"]));
    let ttl_ms = number_after(&json, "\"ttl_ms\":");
    assert!((1_790_000..=1_800_000).contains(&ttl_ms), "{json}");
    assert!(json.contains("\"expires_at\":\"20"), "{json}");

    ok(&["put", &file, "user:1", "Ada"]);
    assert_eq!(stdout(&on_terminal(&["ttl", &file, "user:1"])), "never\n");
    assert_eq!(
        stdout(&ok(&["ttl", &file, "user:1"])),
        "{\"key\":\"user:1\",\"ttl_ms\":null,\"expires_at\":null}\n"
    );
    let expired = ok(&["expire", &file, "user:1", "7d"]);
    assert!(stdout(&expired).starts_with("{\"key\":\"user:1\",\"expires\":true,\"ttl_ms\":604800000,"));
    let persisted = ok(&["persist", &file, "user:1"]);
    assert_eq!(stdout(&persisted), "{\"key\":\"user:1\",\"persisted\":true}\n");
    assert_eq!(stdout(&on_terminal(&["ttl", &file, "user:1"])), "never\n");
    assert_eq!(on_terminal(&["persist", &file, "user:1"]).exit, 0);

    let absent = on_terminal(&["ttl", &file, "nobody"]);
    assert_eq!((absent.exit, stderr(&absent)), (1, "eggdb: key not found: nobody\n".to_owned()));
    let nothing = on_terminal(&["expire", &file, "nobody", "30s"]);
    assert_eq!(nothing.exit, 1);
    assert!(stderr(&nothing).contains("nothing to expire: nobody is not in "), "{}", stderr(&nothing));
    assert_eq!(stdout(&run(&["expire", &file, "nobody", "30s"])), "{\"key\":\"nobody\",\"expires\":false}\n");
    let not_persisted = on_terminal(&["persist", &file, "nobody"]);
    assert_eq!(not_persisted.exit, 1);
    assert!(stderr(&not_persisted).contains("nothing to persist: nobody is not in "));

    ok(&["put", &file, "s:1", "bob"]);
    assert_eq!(stdout(&on_terminal(&["ttl", &file, "s:1"])), "never\n");
}

#[test]
fn counters_count_and_refuse_what_is_not_a_number() {
    let directory = TempDirectory::new("expiry-cli-counters");
    let file = created(&directory);
    assert_eq!(stdout(&on_terminal(&["incr", &file, "hits"])), "1\n");
    assert_eq!(stdout(&on_terminal(&["incr", &file, "hits", "10"])), "11\n");
    assert_eq!(stdout(&on_terminal(&["decr", &file, "hits", "2"])), "9\n");
    assert_eq!(stdout(&ok(&["incr", &file, "hits"])), "{\"key\":\"hits\",\"value\":\"10\"}\n");
    assert_eq!(ok(&["get", &file, "hits"]).stdout, b"10");

    ok(&["put", &file, "name", "Ada"]);
    let refused = on_terminal(&["incr", &file, "name"]);
    assert_eq!(refused.exit, 3);
    assert!(stderr(&refused).contains("cannot increment name in "), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("its value is not a whole number (Ada)"));
    assert!(stderr(&refused).contains(" 0"));
    ok(&["put", &file, "big", "9223372036854775807"]);
    let overflow = on_terminal(&["incr", &file, "big"]);
    assert_eq!(overflow.exit, 3);
    assert!(stderr(&overflow).contains("9223372036854775807 + 1 is outside"), "{}", stderr(&overflow));
    assert!(stderr(&on_terminal(&["decr", &file, "name"])).contains("cannot decrement name"));

    ok(&["collection", "add", &file, "stats"]);
    assert_eq!(stdout(&on_terminal(&["incr", &file, "-C", "stats", "page:home", "3"])), "3\n");
    assert_eq!(run(&["get", &file, "page:home"]).exit, 1);
    assert_eq!(ok(&["get", &file, "-C", "stats", "page:home"]).stdout, b"3");
}

#[test]
fn compare_and_set_swaps_only_the_expected_value() {
    let directory = TempDirectory::new("expiry-cli-cas");
    let file = created(&directory);
    assert_eq!(ok(&["cas", &file, "lock", "--if-absent", "worker-7"]).stdout, b"{\"key\":\"lock\",\"swapped\":true}\n");
    let taken = on_terminal(&["cas", &file, "lock", "--if-absent", "worker-8"]);
    assert_eq!(taken.exit, 1);
    assert!(stderr(&taken).contains("not swapped: lock is already in "), "{}", stderr(&taken));
    assert!(stderr(&taken).contains("(holding worker-7). Nothing changed."));
    assert_eq!(
        stdout(&run(&["cas", &file, "lock", "worker-8", "worker-9"])),
        "{\"key\":\"lock\",\"swapped\":false,\"value\":\"worker-7\"}\n"
    );
    let differs = on_terminal(&["cas", &file, "lock", "worker-8", "worker-9"]);
    assert!(stderr(&differs).contains("holds worker-7, not worker-8. Nothing changed."), "{}", stderr(&differs));
    ok(&["cas", &file, "lock", "worker-7", "--delete"]);
    assert_eq!(run(&["get", &file, "lock"]).exit, 1);
    let absent = on_terminal(&["cas", &file, "lock", "worker-7", "x"]);
    assert_eq!(absent.exit, 1);
    assert!(stderr(&absent).contains("not swapped: lock is not in "));
    ok(&["collection", "add", &file, "locks"]);
    ok(&["cas", &file, "-C", "locks", "l", "--if-absent", "me"]);
    assert_eq!(ok(&["get", &file, "-C", "locks", "l"]).stdout, b"me");
}

#[test]
fn usage_errors_name_the_time_grammar_and_the_other_command() {
    let directory = TempDirectory::new("expiry-cli-usage");
    let file = created(&directory);
    let usage = |args: &[&str]| {
        let ran = run(args);
        assert_eq!(ran.exit, 2, "{args:?}: {}", stderr(&ran));
        stderr(&ran)
    };
    assert!(usage(&["put", &file, "k", "v", "--ttl", "30"])
        .contains("--ttl needs a time such as 30s, 5m, 1h30m, 7d or 1500ms, not '30'"));
    assert!(usage(&["put", &file, "k", "v", "--ttl", "0s"])
        .contains("--ttl 0 would remove the key as soon as it is stored"));
    assert!(usage(&["expire", &file, "k", "0m"]).contains("a time to live of 0 would remove k now; use eggdb del"));
    assert!(usage(&["expire", &file, "k", "soon"]).contains("expire needs a time such as"));
    assert!(usage(&["expire", &file, "k"]).contains("eggdb expire needs a time"));
    assert!(usage(&["incr", &file, "k", "-5"]).contains("unknown option '-5' for eggdb incr; to subtract, use eggdb decr"));
    assert!(usage(&["incr", &file, "k", "x"]).contains("incr needs a whole number from 0 to 9223372036854775807, not 'x'"));
    assert!(usage(&["cas", &file, "k", "--if-absent", "--delete"]).contains("--if-absent with --delete would change nothing"));
    assert!(usage(&["cas", &file, "k", "only-one"]).contains("eggdb cas needs"));
    assert!(usage(&["ttl", &file, "k", "-F", "hex"]).contains("--format hex is for get and scan"));
    assert!(usage(&["get", &file, "k", "--ttl", "5s"]).contains("unknown option '--ttl' for eggdb get"));
}

#[test]
fn the_shell_runs_the_new_commands_in_the_chosen_collection() {
    let directory = TempDirectory::new("expiry-cli-shell");
    let file = created(&directory);
    ok(&["collection", "add", &file, "stats"]);
    let script = ".collection stats\n\
        incr hits\n\
        incr hits 4\n\
        decr hits\n\
        put s:1 alice --ttl 1h\n\
        persist s:1\n\
        ttl s:1\n\
        cas s:1 alice bob\n\
        get s:1\n";
    let ran = run_with(&["shell", &file], script.as_bytes(), Terminal::default());
    assert_eq!(ran.exit, 0, "{}", stderr(&ran));
    assert_eq!(stdout(&ran), "collection: stats\n1\n5\n4\nnever\nbob\n");
    assert_eq!(ok(&["get", &file, "-C", "stats", "hits"]).stdout, b"4");
}
