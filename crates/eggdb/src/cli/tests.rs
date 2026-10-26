use super::{Console, Terminal, args, run};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-cli-unit-{name}-{}-{number}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn file(&self, name: &str) -> String {
        self.path.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, words: &[&'a str]) -> &'a str {
        words[self.below(words.len())]
    }
}

struct Ran {
    exit: u8,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_with(args: &[&str], stdin: &[u8], terminal: Terminal) -> Ran {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let mut stdin = stdin;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut console = Console {
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
        terminal,
    };
    let exit = run(&args, &mut console);
    Ran {
        exit,
        stdout,
        stderr,
    }
}

const VOCABULARY: &[&str] = &[
    "put",
    "get",
    "del",
    "delete",
    "scan",
    "check",
    "stats",
    "help",
    "version",
    "ui",
    "export",
    "frob",
    "--format",
    "-F",
    "json",
    "text",
    "hex",
    "raw",
    "yaml",
    "--color",
    "always",
    "never",
    "--cache",
    "64k",
    "1",
    "0",
    "99999999999999999999g",
    "--page-size",
    "--prefix",
    "-p",
    "--from",
    "--to",
    "-r",
    "--reverse",
    "-n",
    "--limit",
    "-k",
    "--keys-only",
    "-h",
    "-V",
    "--",
    "-",
    "--bogus",
    "-x",
    "--format=",
    "--to=",
    "@-",
    "@/nonexistent/eggdb-cli-fuzz",
    "=",
    "=@x",
    "0x",
    "0x4",
    "0x4g",
    "0x00ff",
    "user:1",
    "user:",
    "",
    "é",
];

#[test]
fn random_arguments_never_panic_the_parser() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..20_000 {
        let count = rng.below(8);
        let mut args: Vec<OsString> = (0..count)
            .map(|_| OsString::from(rng.pick(VOCABULARY)))
            .collect();
        if rng.below(4) == 0 {
            args.push(random_os_string(&mut rng));
        }
        let _ = args::parse(&args);
        let _ = args::color_choice_of(&args);
    }
}

#[cfg(unix)]
fn random_os_string(rng: &mut Rng) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    let length = rng.below(12);
    OsString::from_vec((0..length).map(|_| rng.next() as u8).collect())
}

#[cfg(not(unix))]
fn random_os_string(rng: &mut Rng) -> OsString {
    let length = rng.below(12);
    (0..length)
        .map(|_| char::from_u32(rng.next() as u32 % 0x800).unwrap_or('?'))
        .collect::<String>()
        .into()
}

#[test]
fn random_commands_against_real_files_never_panic_and_exit_with_a_designed_code() {
    let directory = TempDirectory::new("fuzz");
    let database = directory.file("fuzz.db");
    let missing = directory.file("missing.db");
    let not_a_database = directory.file("not-a-database.db");
    std::fs::write(&not_a_database, vec![7u8; 9000]).unwrap();
    let setup = run_with(
        &["put", "--create", &database, "user:1", "Ada"],
        b"",
        Terminal::default(),
    );
    assert_eq!(setup.exit, 0);
    let files = [database.as_str(), missing.as_str(), not_a_database.as_str()];
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for round in 0..600 {
        let mut words: Vec<&str> = Vec::new();
        words.push(rng.pick(&["put", "get", "del", "scan", "check", "stats", "frob"]));
        words.push(rng.pick(&files));
        for _ in 0..rng.below(6) {
            words.push(rng.pick(VOCABULARY));
        }
        let terminal = Terminal {
            stdin: rng.below(2) == 0,
            stdout: rng.below(2) == 0,
            stdout_color: rng.below(2) == 0,
            stderr_color: rng.below(2) == 0,
        };
        let ran = run_with(&words, b"piped bytes", terminal);
        assert!(
            ran.exit <= 5,
            "round {round}: {words:?} exited {}: {}",
            ran.exit,
            String::from_utf8_lossy(&ran.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&ran.stderr).contains("Error {"),
            "round {round}: {words:?} printed a Debug dump"
        );
    }
    let check = run_with(
        &["check", &database, "-F", "json"],
        b"",
        Terminal::default(),
    );
    assert_eq!(check.exit, 0, "{}", String::from_utf8_lossy(&check.stdout));
}

#[test]
fn colour_marks_keys_and_messages_but_never_the_data() {
    let directory = TempDirectory::new("colour");
    let database = directory.file("colour.db");
    let colour = Terminal {
        stdin: true,
        stdout: true,
        stdout_color: true,
        stderr_color: true,
    };
    assert_eq!(
        run_with(&["put", "--create", &database, "k", "v"], b"", colour).exit,
        0
    );
    let scan = run_with(&["scan", &database], b"", colour);
    assert_eq!(scan.stdout, b"\x1b[1mk\x1b[0m\tv\n");
    assert_eq!(scan.stderr, b"\x1b[2m1 entry\x1b[0m\n");
    let get = run_with(&["get", &database, "k"], b"", colour);
    assert_eq!(get.stdout, b"v\n");
    let missing = run_with(&["get", &database, "nope"], b"", colour);
    assert_eq!(
        missing.stderr,
        b"\x1b[31meggdb:\x1b[0m key not found: nope\n"
    );
    let never = run_with(&["scan", &database, "--color", "never"], b"", colour);
    assert_eq!(never.stdout, b"k\tv\n");
    let forced = run_with(
        &["get", &database, "nope", "--color=always"],
        b"",
        Terminal::default(),
    );
    assert_eq!(
        forced.stderr,
        b"\x1b[31meggdb:\x1b[0m key not found: nope\n"
    );
}

#[test]
fn a_value_is_required_when_stdin_is_a_terminal() {
    let directory = TempDirectory::new("terminal-stdin");
    let database = directory.file("terminal.db");
    let terminal = Terminal {
        stdin: true,
        ..Terminal::default()
    };
    let ran = run_with(&["put", "--create", &database, "k"], b"ignored", terminal);
    assert_eq!(ran.exit, 2);
    assert!(
        String::from_utf8_lossy(&ran.stderr).starts_with(
            "eggdb: put needs a value: give it as an argument, as @file, or pipe it to stdin\n"
        ),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let piped = run_with(
        &["put", "--create", &database, "k"],
        b"from stdin",
        Terminal::default(),
    );
    assert_eq!(piped.exit, 0);
    let get = run_with(&["get", &database, "k"], b"", Terminal::default());
    assert_eq!(get.stdout, b"from stdin");
}
