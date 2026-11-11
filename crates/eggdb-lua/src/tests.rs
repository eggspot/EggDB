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

fn text(values: &[crate::Value], at: usize) -> String {
    String::from_utf8(values[at].to_bytes().expect("a string")).expect("utf-8")
}

#[test]
fn string_functions_slice_and_search_with_patterns() {
    let values = run("local s = 'hello world' return s:sub(1, 5), s:upper(), ('x'):rep(3), s:len(), s:byte(1)").expect("runs");
    assert_eq!(text(&values, 0), "hello");
    assert_eq!(text(&values, 1), "HELLO WORLD");
    assert_eq!(text(&values, 2), "xxx");
    assert_eq!(values[3].to_number(), Some(11.0));
    assert_eq!(values[4].to_number(), Some(104.0));
    let values = run("return string.find('key:42:end', ':(%d+):'), string.match('a=1, b=2', '(%w+)=(%w+)')").expect("runs");
    assert_eq!(values[0].to_number(), Some(4.0));
    assert_eq!(values[1].to_number(), Some(7.0));
    assert_eq!(text(&values, 2), "42");
    assert_eq!(text(&values, 3), "a");
    assert_eq!(text(&values, 4), "1");
}

#[test]
fn gsub_and_gmatch_follow_lua_semantics() {
    let values = run("return string.gsub('hello world', '(%w+)', '<%1>')").expect("runs");
    assert_eq!(text(&values, 0), "<hello> <world>");
    assert_eq!(values[1].to_number(), Some(2.0));
    let values = run("return (string.gsub('abc', '', '-'))").expect("runs");
    assert_eq!(text(&values, 0), "-a-b-c-");
    let values = run("local n = 0 for word in string.gmatch('one two three', '%a+') do n = n + #word end return n").expect("runs");
    assert_eq!(values[0].to_number(), Some(11.0));
    let values = run("return (string.gsub('$name is $age', '%$(%w+)', {name = 'Ann', age = 30}))").expect("runs");
    assert_eq!(text(&values, 0), "Ann is 30");
    let values = run("return string.find('f(a(b)c)', '%b()'), string.find('THE (quick) fox', '%f[%a]%a+', 5)").expect("runs");
    assert_eq!(values[0].to_number(), Some(2.0));
    assert_eq!(values[1].to_number(), Some(8.0));
    assert_eq!(values[2].to_number(), Some(6.0));
}

#[test]
fn format_follows_c_printf() {
    let values = run("return string.format('%5d|%-5s|%05.2f|%x|%g|%e|%q', 42, 'ab', 3.14159, 255, 0.0001, 12345.678, 'a\"b')").expect("runs");
    assert_eq!(text(&values, 0), "   42|ab   |03.14|ff|0.0001|1.234568e+04|\"a\\\"b\"");
}

#[test]
fn cjson_round_trips_tables_and_null() {
    let values = run("return cjson.encode({1, 2, 'x/y'}), cjson.encode({a = true}), cjson.encode({})").expect("runs");
    assert_eq!(text(&values, 0), "[1,2,\"x\\/y\"]");
    assert_eq!(text(&values, 1), "{\"a\":true}");
    assert_eq!(text(&values, 2), "{}");
    let values = run("local t = cjson.decode('{\"k\":[1,null,\"\\\\u00e9\"]}') return t.k[1], t.k[2] == cjson.null, t.k[3]").expect("runs");
    assert_eq!(values[0].to_number(), Some(1.0));
    assert_eq!(values[1], crate::Value::Boolean(true));
    assert_eq!(text(&values, 2), "é");
    assert!(run("return cjson.encode({[1] = 1, [100] = 2})").is_err());
}

#[test]
fn bit_operations_wrap_to_signed_32_bits() {
    let values = run("return bit.band(0xff, 0x0f), bit.bor(1, 2, 4), bit.lshift(1, 31), bit.tohex(255), bit.bnot(0)").expect("runs");
    assert_eq!(values[0].to_number(), Some(15.0));
    assert_eq!(values[1].to_number(), Some(7.0));
    assert_eq!(values[2].to_number(), Some(-2_147_483_648.0));
    assert_eq!(text(&values, 3), "000000ff");
    assert_eq!(values[4].to_number(), Some(-1.0));
}

#[test]
fn cmsgpack_packs_and_unpacks() {
    let values = run("local packed = cmsgpack.pack({1, 2, 3}, 'hi', -5, 1.5) return #packed, cmsgpack.unpack(packed)").expect("runs");
    assert_eq!(values[0].to_number(), Some(13.0));
    assert!(matches!(&values[1], crate::Value::Table(_)));
    assert_eq!(text(&values, 2), "hi");
    assert_eq!(values[3].to_number(), Some(-5.0));
    assert_eq!(values[4].to_number(), Some(1.5));
}
