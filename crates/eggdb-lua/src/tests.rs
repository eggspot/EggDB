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
