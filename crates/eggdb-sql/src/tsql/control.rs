use super::variables::ScalarVariable;
use crate::ast::Statement;
use crate::error::SqlError;
use crate::lexer::{Token, TokenKind};
use crate::session::OutputColumn;
use std::ops::Range;

pub(crate) const MAX_DYNAMIC_NESTING: u8 = 32;

const LOGIC_WORDS: [&str; 12] = [
    "declare",
    "if",
    "while",
    "print",
    "raiserror",
    "throw",
    "return",
    "break",
    "continue",
    "goto",
    "try",
    "sp_executesql",
];

const TRANSACTION_WORDS: [&str; 5] = ["tran", "transaction", "distributed", "dialog", "conversation"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Instruction {
    Run {
        at: (usize, usize),
    },
    Test {
        condition: (usize, usize),
        otherwise: usize,
        end: usize,
    },
    Jump {
        to: usize,
    },
    Return,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TryBlock {
    pub(crate) body: Range<usize>,
    pub(crate) handler: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Declaration {
    Scalar(ScalarVariable),
    Table {
        name: String,
        columns: Vec<OutputColumn>,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Program {
    pub(crate) instructions: Vec<Instruction>,
    pub(crate) tries: Vec<TryBlock>,
    pub(crate) declarations: Vec<Declaration>,
}

impl Program {
    pub(crate) fn try_around(&self, at: usize) -> Option<usize> {
        self.tries.iter().rposition(|block| block.body.contains(&at))
    }

    pub(crate) fn handler_around(&self, at: usize) -> Option<usize> {
        self.tries.iter().rposition(|block| block.handler.contains(&at))
    }
}

#[derive(Debug)]
pub(crate) enum ProgramEnd {
    Finished,
    Stopped,
    DialectChanged { at: usize },
    Escaped(SqlError),
}

pub(crate) fn chooses_dialect(statement: &Statement) -> bool {
    match statement {
        Statement::Set { name, .. } | Statement::Reset { name: Some(name) } => {
            name.eq_ignore_ascii_case(crate::settings::DIALECT_SETTING)
        }
        _ => false,
    }
}

fn word_at<'k>(tokens: &'k [Token<'_>], at: usize) -> Option<&'k str> {
    match tokens.get(at).map(|token| &token.kind) {
        Some(TokenKind::Word(word)) => Some(word.as_ref()),
        _ => None,
    }
}

pub(crate) fn uses_batch_logic(tokens: &[Token<'_>]) -> bool {
    tokens
        .iter()
        .enumerate()
        .any(|(at, token)| match &token.kind {
            TokenKind::Variable(name) => !name.starts_with("@@"),
            TokenKind::Word(word) => match word.as_ref() {
                "begin" => !word_at(tokens, at + 1).is_some_and(|next| TRANSACTION_WORDS.contains(&next)),
                "exec" | "execute" => tokens
                    .get(at + 1)
                    .is_some_and(|next| next.kind == TokenKind::LeftParen),
                other => LOGIC_WORDS.contains(&other),
            },
            _ => false,
        })
}
