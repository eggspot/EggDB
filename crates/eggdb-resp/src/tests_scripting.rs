use crate::commands::{Client, Context};
use crate::keyspace::MemoryKeyspace;
use crate::protocol::Reply;
use crate::scripting::Scripts;
use crate::transactions::execute_with_transactions;

struct Scripted {
    keyspace: MemoryKeyspace,
    scripts: Scripts,
    client: Client,
}

impl Scripted {
    fn new() -> Self {
        Self {
            keyspace: MemoryKeyspace::default(),
            scripts: Scripts::default(),
            client: Client {
                authenticated: true,
                ..Client::default()
            },
        }
    }

    fn run(&mut self, arguments: &[&str]) -> Reply {
        let arguments: Vec<Vec<u8>> = arguments.iter().map(|word| word.as_bytes().to_vec()).collect();
        let context = Context {
            keyspace: &self.keyspace,
            password_sha256: None,
            now_ms: 1_000_000,
            reply_room_bytes: usize::MAX,
            blocked_clients: 0,
            scripts: Some(&self.scripts),
        };
        execute_with_transactions(&context, &mut self.client, &arguments)
    }
}

fn error_text(reply: &Reply) -> &str {
    match reply {
        Reply::Error(text) => text,
        other => panic!("an error reply, not {other:?}"),
    }
}

#[test]
fn eval_runs_commands_against_keys_and_arguments() {
    let mut redis = Scripted::new();
    let reply = redis.run(&["EVAL", "return redis.call('SET', KEYS[1], ARGV[1])", "1", "greeting", "hello"]);
    assert_eq!(reply, Reply::Status("OK".to_owned()));
    assert_eq!(redis.run(&["GET", "greeting"]), Reply::Bulk(Some(b"hello".to_vec())));
    let reply = redis.run(&["EVAL", "return {1, 'two', {3}, nil, 5}", "0"]);
    assert_eq!(
        reply,
        Reply::Array(Some(vec![
            Reply::Integer(1),
            Reply::Bulk(Some(b"two".to_vec())),
            Reply::Array(Some(vec![Reply::Integer(3)])),
        ]))
    );
    assert_eq!(redis.run(&["EVAL", "return redis.call('GET', 'missing')", "0"]), Reply::Bulk(None));
    assert_eq!(redis.run(&["EVAL", "return 3.99", "0"]), Reply::Integer(3));
}

#[test]
fn script_load_and_evalsha_share_the_cache() {
    let mut redis = Scripted::new();
    let Reply::Bulk(Some(sha)) = redis.run(&["SCRIPT", "LOAD", "return ARGV[1] .. '!'"]) else {
        panic!("a sha");
    };
    let sha = String::from_utf8(sha).expect("hex");
    assert_eq!(sha.len(), 40);
    assert_eq!(redis.run(&["EVALSHA", &sha, "0", "hi"]), Reply::Bulk(Some(b"hi!".to_vec())));
    assert_eq!(
        redis.run(&["SCRIPT", "EXISTS", &sha, "0000000000000000000000000000000000000000"]),
        Reply::Array(Some(vec![Reply::Integer(1), Reply::Integer(0)]))
    );
    assert_eq!(redis.run(&["SCRIPT", "FLUSH"]), Reply::Status("OK".to_owned()));
    assert!(error_text(&redis.run(&["EVALSHA", &sha, "0"])).starts_with("NOSCRIPT"));
}

#[test]
fn errors_come_back_as_error_replies() {
    let mut redis = Scripted::new();
    redis.run(&["SET", "text", "abc"]);
    let reply = redis.run(&["EVAL", "return redis.call('INCR', 'text')", "0"]);
    assert!(error_text(&reply).contains("not an integer"), "{reply:?}");
    let reply = redis.run(&["EVAL", "local r = redis.pcall('INCR', 'text') return r", "0"]);
    assert!(error_text(&reply).contains("not an integer"), "{reply:?}");
    let reply = redis.run(&["EVAL", "return redis.error_reply('MY failure')", "0"]);
    assert_eq!(error_text(&reply), "MY failure");
    let reply = redis.run(&["EVAL", "return redis.call('EVAL', 'return 1', '0')", "0"]);
    assert!(error_text(&reply).contains("not allowed from script"), "{reply:?}");
    let reply = redis.run(&["EVAL", "return (", "0"]);
    assert!(error_text(&reply).starts_with("ERR Error compiling script"), "{reply:?}");
    let reply = redis.run(&["EVAL", "return 1", "2", "only-one"]);
    assert!(error_text(&reply).contains("greater than"), "{reply:?}");
}

#[test]
fn read_only_scripts_refuse_writes() {
    let mut redis = Scripted::new();
    let reply = redis.run(&["EVAL_RO", "return redis.call('SET', 'k', 'v')", "0"]);
    assert!(error_text(&reply).contains("read-only"), "{reply:?}");
    let reply = redis.run(&["EVAL", "#!lua flags=no-writes\nreturn redis.call('SET', 'k', 'v')", "0"]);
    assert!(error_text(&reply).contains("read-only"), "{reply:?}");
    assert_eq!(redis.run(&["GET", "k"]), Reply::Bulk(None));
}

#[test]
fn eval_runs_inside_multi() {
    let mut redis = Scripted::new();
    redis.run(&["MULTI"]);
    redis.run(&["EVAL", "return redis.call('INCRBY', 'n', 5)", "0"]);
    redis.run(&["INCR", "n"]);
    assert_eq!(
        redis.run(&["EXEC"]),
        Reply::Array(Some(vec![Reply::Integer(5), Reply::Integer(6)]))
    );
}

const LIBRARY: &str = "#!lua name=counters\n\
    redis.register_function('bump', function(keys, args) return redis.call('INCRBY', keys[1], args[1]) end)\n\
    redis.register_function{function_name='peek', callback=function(keys) return redis.call('GET', keys[1]) end, flags={'no-writes'}}";

#[test]
fn functions_load_list_and_run() {
    let mut redis = Scripted::new();
    assert_eq!(redis.run(&["FUNCTION", "LOAD", LIBRARY]), Reply::Bulk(Some(b"counters".to_vec())));
    assert!(error_text(&redis.run(&["FUNCTION", "LOAD", LIBRARY])).contains("already exists"));
    assert_eq!(redis.run(&["FUNCTION", "LOAD", "REPLACE", LIBRARY]), Reply::Bulk(Some(b"counters".to_vec())));
    assert_eq!(redis.run(&["FCALL", "bump", "1", "n", "4"]), Reply::Integer(4));
    assert_eq!(redis.run(&["FCALL_RO", "peek", "1", "n"]), Reply::Bulk(Some(b"4".to_vec())));
    assert!(error_text(&redis.run(&["FCALL_RO", "bump", "1", "n", "1"])).contains("write flag"));
    assert!(error_text(&redis.run(&["FCALL", "missing", "0"])).contains("Function not found"));
    let Reply::Array(Some(libraries)) = redis.run(&["FUNCTION", "LIST", "LIBRARYNAME", "count*"]) else {
        panic!("a list");
    };
    assert_eq!(libraries.len(), 1);
    assert_eq!(redis.run(&["FUNCTION", "DELETE", "counters"]), Reply::Status("OK".to_owned()));
    assert!(error_text(&redis.run(&["FCALL", "bump", "1", "n", "1"])).contains("Function not found"));
    assert!(error_text(&redis.run(&["FUNCTION", "LOAD", "return 1"])).contains("metadata"));
}
