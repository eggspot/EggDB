use crate::ast::{BinaryOp, Expr, Stat};
use crate::parser::parse;
use crate::{LuaErrorKind, check_syntax};

#[test]
fn scripts_parse_into_statements_with_lua_precedence() {
    let block = parse(b"local x = 1 + 2 * 3 ^ 2 .. 'a' return redis.call('GET', KEYS[1]), x").expect("parses");
    assert_eq!(block.len(), 2);
    let Stat::Local(names, values) = &block[0] else {
        panic!("a local statement");
    };
    assert_eq!(names, &["x".to_owned()]);
    let Expr::Binary(BinaryOp::Concat, left, _) = &values[0] else {
        panic!("concatenation binds loosest here: {:?}", values[0]);
    };
    assert!(matches!(**left, Expr::Binary(BinaryOp::Add, _, _)));
    assert!(matches!(&block[1], Stat::Return(values) if values.len() == 2));
}

#[test]
fn every_statement_form_parses() {
    let source = br#"
        -- a comment
        --[[ a long
             comment ]]
        local t = {1, 2, n = 3, ["k"] = [[long
string]]}
        function t.add(a, b) return a + b end
        function t:size() return #self end
        local function fact(n) if n <= 1 then return 1 else return n * fact(n - 1) end end
        for i = 1, 10, 2 do t[i] = i end
        for k, v in pairs(t) do local _ = k end
        while false do break end
        repeat local y = 1 until y == 1
        do local z = 0x1F end
        local s = "esc\"aped\n" .. 'x\65'
        print(t:size(), fact(5), ...)
    "#;
    check_syntax(source).expect("parses");
}

#[test]
fn syntax_errors_carry_their_line() {
    let error = check_syntax(b"local x = 1\nlocal = 2").expect_err("a syntax error");
    assert_eq!(error.kind, LuaErrorKind::Syntax);
    assert_eq!(error.line, 2);
    assert!(check_syntax(b"x = 'unfinished").is_err());
    assert!(check_syntax(b"return 1 +").is_err());
    let deep = format!("return {}1{}", "(".repeat(500), ")".repeat(500));
    assert!(check_syntax(deep.as_bytes()).is_err());
}

fn run(source: &str) -> Result<Vec<crate::Value>, crate::LuaError> {
    crate::Interpreter::new().run(source.as_bytes(), &mut crate::NoHost, 100_000)
}

fn number(values: &[crate::Value]) -> f64 {
    values.first().and_then(crate::Value::to_number).expect("a number")
}

#[test]
fn arithmetic_closures_and_multiple_returns_run() {
    assert_eq!(number(&run("return 1 + 2 * 3 - 4 / 2").expect("runs")), 5.0);
    let counter = "local function counter() local n = 0 return function() n = n + 1 return n end end \
        local c = counter() c() c() return c()";
    assert_eq!(number(&run(counter).expect("runs")), 3.0);
    let values = run("local function two() return 1, 2 end local t = {two()} return #t, select('#', two())").expect("runs");
    assert_eq!(values.len(), 2);
    assert_eq!(values[0].to_number(), Some(2.0));
    assert_eq!(values[1].to_number(), Some(2.0));
}

#[test]
fn tables_iterate_and_sort() {
    let source = "local t = {5, 3, 9, 1} table.sort(t) local s = 0 \
        for i, v in ipairs(t) do s = s * 10 + v end \
        local h = {a = 1, b = 2} local n = 0 for k, v in pairs(h) do n = n + v end \
        return s, n, table.concat({'x', 'y', 'z'}, ',')";
    let values = run(source).expect("runs");
    assert_eq!(values[0].to_number(), Some(1359.0));
    assert_eq!(values[1].to_number(), Some(3.0));
    assert_eq!(values[2].to_bytes().as_deref(), Some(&b"x,y,z"[..]));
}

#[test]
fn pcall_catches_errors_and_error_prefixes_the_line() {
    let values = run("local ok, message = pcall(function() error('boom') end) return ok, message").expect("runs");
    assert_eq!(values[0], crate::Value::Boolean(false));
    assert_eq!(values[1].to_bytes().as_deref(), Some(&b"user_script:1: boom"[..]));
    let failure = run("\nerror('bad')").expect_err("fails");
    assert_eq!(failure.kind, LuaErrorKind::Runtime);
    assert!(failure.message.ends_with("bad"), "{}", failure.message);
}

#[test]
fn metatables_supply_missing_keys() {
    let source = "local base = {greet = 'hi'} local t = setmetatable({}, {__index = base}) return t.greet";
    let values = run(source).expect("runs");
    assert_eq!(values[0].to_bytes().as_deref(), Some(&b"hi"[..]));
}

#[test]
fn an_endless_loop_stops_at_the_step_budget() {
    let failure = run("while true do end").expect_err("killed");
    assert_eq!(failure.kind, LuaErrorKind::Runtime);
    let caught = run("return pcall(function() while true do end end)");
    assert!(caught.is_err(), "pcall must not catch the budget kill");
}
