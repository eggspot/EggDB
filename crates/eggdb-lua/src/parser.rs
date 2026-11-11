use crate::LuaError;
use crate::ast::{BinaryOp, Block, Expr, FunctionBody, Stat, TableField, UnaryOp};
use crate::lexer::{Located, Token};
use std::rc::Rc;

const MOST_DEPTH: usize = 200;
const UNARY_PRIORITY: u8 = 8;

fn priority(op: BinaryOp) -> (u8, u8) {
    match op {
        BinaryOp::Or => (1, 1),
        BinaryOp::And => (2, 2),
        BinaryOp::Less
        | BinaryOp::Greater
        | BinaryOp::LessEqual
        | BinaryOp::GreaterEqual
        | BinaryOp::NotEqual
        | BinaryOp::Equal => (3, 3),
        BinaryOp::Concat => (5, 4),
        BinaryOp::Add | BinaryOp::Subtract => (6, 6),
        BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Modulo => (7, 7),
        BinaryOp::Power => (10, 9),
    }
}

fn binary(token: &Token) -> Option<BinaryOp> {
    Some(match token {
        Token::Keyword("or") => BinaryOp::Or,
        Token::Keyword("and") => BinaryOp::And,
        Token::Symbol("<") => BinaryOp::Less,
        Token::Symbol(">") => BinaryOp::Greater,
        Token::Symbol("<=") => BinaryOp::LessEqual,
        Token::Symbol(">=") => BinaryOp::GreaterEqual,
        Token::Symbol("~=") => BinaryOp::NotEqual,
        Token::Symbol("==") => BinaryOp::Equal,
        Token::Symbol("..") => BinaryOp::Concat,
        Token::Symbol("+") => BinaryOp::Add,
        Token::Symbol("-") => BinaryOp::Subtract,
        Token::Symbol("*") => BinaryOp::Multiply,
        Token::Symbol("/") => BinaryOp::Divide,
        Token::Symbol("%") => BinaryOp::Modulo,
        Token::Symbol("^") => BinaryOp::Power,
        _ => return None,
    })
}

struct Parser {
    tokens: Vec<Located>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.at.min(self.tokens.len() - 1)].token
    }

    fn peek_at(&self, ahead: usize) -> &Token {
        &self.tokens[(self.at + ahead).min(self.tokens.len() - 1)].token
    }

    fn line(&self) -> u32 {
        self.tokens[self.at.min(self.tokens.len() - 1)].line
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        if self.at < self.tokens.len() - 1 {
            self.at += 1;
        }
        token
    }

    fn error(&self, message: &str) -> LuaError {
        LuaError::syntax(self.line(), message)
    }

    fn is_symbol(&self, symbol: &str) -> bool {
        matches!(self.peek(), Token::Symbol(found) if *found == symbol)
    }

    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Token::Keyword(found) if *found == keyword)
    }

    fn take_symbol(&mut self, symbol: &str) -> bool {
        let found = self.is_symbol(symbol);
        if found {
            self.advance();
        }
        found
    }

    fn take_keyword(&mut self, keyword: &str) -> bool {
        let found = self.is_keyword(keyword);
        if found {
            self.advance();
        }
        found
    }

    fn expect_symbol(&mut self, symbol: &str) -> Result<(), LuaError> {
        if self.take_symbol(symbol) {
            Ok(())
        } else {
            Err(self.error(&format!("'{symbol}' expected")))
        }
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<(), LuaError> {
        if self.take_keyword(keyword) {
            Ok(())
        } else {
            Err(self.error(&format!("'{keyword}' expected")))
        }
    }

    fn name(&mut self) -> Result<String, LuaError> {
        match self.advance() {
            Token::Name(name) => Ok(name),
            _ => Err(self.error("<name> expected")),
        }
    }

    fn enter(&mut self) -> Result<(), LuaError> {
        self.depth += 1;
        if self.depth > MOST_DEPTH {
            return Err(self.error("chunk has too many syntax levels"));
        }
        Ok(())
    }

    fn block_ends(&self) -> bool {
        matches!(
            self.peek(),
            Token::End | Token::Keyword("end" | "else" | "elseif" | "until")
        )
    }

    fn block(&mut self) -> Result<Block, LuaError> {
        self.enter()?;
        let mut block = Vec::new();
        while !self.block_ends() {
            if self.is_keyword("return") {
                self.advance();
                let values = if self.block_ends() || self.is_symbol(";") {
                    Vec::new()
                } else {
                    self.expression_list()?
                };
                self.take_symbol(";");
                block.push(Stat::Return(values));
                break;
            }
            if self.take_keyword("break") {
                self.take_symbol(";");
                block.push(Stat::Break);
                break;
            }
            block.push(self.statement()?);
            self.take_symbol(";");
        }
        self.depth -= 1;
        Ok(block)
    }

    fn statement(&mut self) -> Result<Stat, LuaError> {
        match self.peek().clone() {
            Token::Keyword("if") => {
                self.advance();
                let mut branches = Vec::new();
                let condition = self.expression()?;
                self.expect_keyword("then")?;
                branches.push((condition, self.block()?));
                let mut otherwise = None;
                loop {
                    if self.take_keyword("elseif") {
                        let condition = self.expression()?;
                        self.expect_keyword("then")?;
                        branches.push((condition, self.block()?));
                    } else if self.take_keyword("else") {
                        otherwise = Some(self.block()?);
                        self.expect_keyword("end")?;
                        break;
                    } else {
                        self.expect_keyword("end")?;
                        break;
                    }
                }
                Ok(Stat::If(branches, otherwise))
            }
            Token::Keyword("while") => {
                self.advance();
                let condition = self.expression()?;
                self.expect_keyword("do")?;
                let body = self.block()?;
                self.expect_keyword("end")?;
                Ok(Stat::While(condition, body))
            }
            Token::Keyword("do") => {
                self.advance();
                let body = self.block()?;
                self.expect_keyword("end")?;
                Ok(Stat::Do(body))
            }
            Token::Keyword("repeat") => {
                self.advance();
                let body = self.block()?;
                self.expect_keyword("until")?;
                Ok(Stat::Repeat(body, self.expression()?))
            }
            Token::Keyword("for") => {
                self.advance();
                let first = self.name()?;
                if self.take_symbol("=") {
                    let start = self.expression()?;
                    self.expect_symbol(",")?;
                    let limit = self.expression()?;
                    let step = if self.take_symbol(",") {
                        Some(self.expression()?)
                    } else {
                        None
                    };
                    self.expect_keyword("do")?;
                    let body = self.block()?;
                    self.expect_keyword("end")?;
                    return Ok(Stat::NumericFor {
                        variable: first,
                        start,
                        limit,
                        step,
                        body,
                    });
                }
                let mut variables = vec![first];
                while self.take_symbol(",") {
                    variables.push(self.name()?);
                }
                self.expect_keyword("in")?;
                let values = self.expression_list()?;
                self.expect_keyword("do")?;
                let body = self.block()?;
                self.expect_keyword("end")?;
                Ok(Stat::GenericFor {
                    variables,
                    values,
                    body,
                })
            }
            Token::Keyword("function") => {
                self.advance();
                let mut path = vec![self.name()?];
                while self.take_symbol(".") {
                    path.push(self.name()?);
                }
                let method = if self.take_symbol(":") {
                    Some(self.name()?)
                } else {
                    None
                };
                let body = self.function_body(method.is_some())?;
                Ok(Stat::Function(path, method, Rc::new(body)))
            }
            Token::Keyword("local") => {
                self.advance();
                if self.take_keyword("function") {
                    let name = self.name()?;
                    let body = self.function_body(false)?;
                    return Ok(Stat::LocalFunction(name, Rc::new(body)));
                }
                let mut names = vec![self.name()?];
                while self.take_symbol(",") {
                    names.push(self.name()?);
                }
                let values = if self.take_symbol("=") {
                    self.expression_list()?
                } else {
                    Vec::new()
                };
                Ok(Stat::Local(names, values))
            }
            _ => {
                let first = self.suffixed()?;
                if self.is_symbol("=") || self.is_symbol(",") {
                    let mut targets = vec![first];
                    while self.take_symbol(",") {
                        targets.push(self.suffixed()?);
                    }
                    self.expect_symbol("=")?;
                    if targets
                        .iter()
                        .any(|target| !matches!(target, Expr::Name(_) | Expr::Index(..)))
                    {
                        return Err(self.error("syntax error near '='"));
                    }
                    let values = self.expression_list()?;
                    return Ok(Stat::Assign(targets, values));
                }
                if !matches!(first, Expr::Call(..) | Expr::Method(..)) {
                    return Err(self.error("syntax error"));
                }
                Ok(Stat::Call(first))
            }
        }
    }

    fn function_body(&mut self, method: bool) -> Result<FunctionBody, LuaError> {
        self.expect_symbol("(")?;
        let mut parameters = Vec::new();
        if method {
            parameters.push("self".to_owned());
        }
        let mut variadic = false;
        if !self.is_symbol(")") {
            loop {
                if self.take_symbol("...") {
                    variadic = true;
                    break;
                }
                parameters.push(self.name()?);
                if !self.take_symbol(",") {
                    break;
                }
            }
        }
        self.expect_symbol(")")?;
        let body = self.block()?;
        self.expect_keyword("end")?;
        Ok(FunctionBody {
            parameters,
            variadic,
            body,
        })
    }

    fn expression_list(&mut self) -> Result<Vec<Expr>, LuaError> {
        let mut list = vec![self.expression()?];
        while self.take_symbol(",") {
            list.push(self.expression()?);
        }
        Ok(list)
    }

    fn expression(&mut self) -> Result<Expr, LuaError> {
        self.subexpression(0)
    }

    fn subexpression(&mut self, limit: u8) -> Result<Expr, LuaError> {
        self.enter()?;
        let unary = match self.peek() {
            Token::Keyword("not") => Some(UnaryOp::Not),
            Token::Symbol("#") => Some(UnaryOp::Length),
            Token::Symbol("-") => Some(UnaryOp::Negate),
            _ => None,
        };
        let mut left = match unary {
            Some(op) => {
                self.advance();
                let operand = self.subexpression(UNARY_PRIORITY)?;
                Expr::Unary(op, Box::new(operand))
            }
            None => self.simple()?,
        };
        while let Some(op) = binary(self.peek()) {
            let (left_priority, right_priority) = priority(op);
            if left_priority <= limit {
                break;
            }
            self.advance();
            let right = self.subexpression(right_priority)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Ok(left)
    }

    fn simple(&mut self) -> Result<Expr, LuaError> {
        Ok(match self.peek().clone() {
            Token::Number(number) => {
                self.advance();
                Expr::Number(number)
            }
            Token::String(bytes) => {
                self.advance();
                Expr::String(bytes.into())
            }
            Token::Keyword("nil") => {
                self.advance();
                Expr::Nil
            }
            Token::Keyword("true") => {
                self.advance();
                Expr::True
            }
            Token::Keyword("false") => {
                self.advance();
                Expr::False
            }
            Token::Symbol("...") => {
                self.advance();
                Expr::VarArgs
            }
            Token::Symbol("{") => self.table()?,
            Token::Keyword("function") => {
                self.advance();
                Expr::Function(Rc::new(self.function_body(false)?))
            }
            _ => self.suffixed()?,
        })
    }

    fn primary(&mut self) -> Result<Expr, LuaError> {
        match self.peek().clone() {
            Token::Name(name) => {
                self.advance();
                Ok(Expr::Name(name))
            }
            Token::Symbol("(") => {
                self.advance();
                let inner = self.expression()?;
                self.expect_symbol(")")?;
                Ok(Expr::Paren(Box::new(inner)))
            }
            _ => Err(self.error("unexpected symbol")),
        }
    }

    fn call_arguments(&mut self) -> Result<Vec<Expr>, LuaError> {
        match self.peek().clone() {
            Token::String(bytes) => {
                self.advance();
                Ok(vec![Expr::String(bytes.into())])
            }
            Token::Symbol("{") => Ok(vec![self.table()?]),
            _ => {
                self.expect_symbol("(")?;
                if self.take_symbol(")") {
                    return Ok(Vec::new());
                }
                let list = self.expression_list()?;
                self.expect_symbol(")")?;
                Ok(list)
            }
        }
    }

    fn suffixed(&mut self) -> Result<Expr, LuaError> {
        let mut expr = self.primary()?;
        loop {
            let line = self.line();
            match self.peek() {
                Token::Symbol(".") => {
                    self.advance();
                    let field = self.name()?;
                    expr = Expr::Index(Box::new(expr), Box::new(Expr::String(field.into_bytes().into())));
                }
                Token::Symbol("[") => {
                    self.advance();
                    let key = self.expression()?;
                    self.expect_symbol("]")?;
                    expr = Expr::Index(Box::new(expr), Box::new(key));
                }
                Token::Symbol(":") => {
                    self.advance();
                    let method = self.name()?;
                    let arguments = self.call_arguments()?;
                    expr = Expr::Method(Box::new(expr), method, arguments, line);
                }
                Token::Symbol("(" | "{") | Token::String(_) => {
                    let arguments = self.call_arguments()?;
                    expr = Expr::Call(Box::new(expr), arguments, line);
                }
                _ => return Ok(expr),
            }
        }
    }

    fn table(&mut self) -> Result<Expr, LuaError> {
        self.expect_symbol("{")?;
        let mut fields = Vec::new();
        while !self.is_symbol("}") {
            if self.take_symbol("[") {
                let key = self.expression()?;
                self.expect_symbol("]")?;
                self.expect_symbol("=")?;
                fields.push(TableField::Keyed(key, self.expression()?));
            } else if matches!(self.peek(), Token::Name(_)) && matches!(self.peek_at(1), Token::Symbol("=")) {
                let name = self.name()?;
                self.advance();
                fields.push(TableField::Named(name, self.expression()?));
            } else {
                fields.push(TableField::Positional(self.expression()?));
            }
            if !self.take_symbol(",") && !self.take_symbol(";") {
                break;
            }
        }
        self.expect_symbol("}")?;
        Ok(Expr::Table(fields))
    }
}

/// The block a chunk of Lua source parses to.
pub(crate) fn parse(source: &[u8]) -> Result<Block, LuaError> {
    let tokens = crate::lexer::tokens(source)?;
    let mut parser = Parser {
        tokens,
        at: 0,
        depth: 0,
    };
    let block = parser.block()?;
    if *parser.peek() != Token::End {
        return Err(parser.error("'<eof>' expected"));
    }
    Ok(block)
}
