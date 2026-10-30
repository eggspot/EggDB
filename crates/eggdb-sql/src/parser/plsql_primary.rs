use super::Parser;
use crate::ast::{Expr, ExprKind};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope};
use crate::lexer::TokenKind;
use crate::plpgsql::{CollectionShape, RoutineFrame, bulk_exceptions_shape, bulk_rowcount_shape};
use crate::types::{Conversion, SqlType, Value, convert};

const METHODS_WITH_ARGUMENT: [&str; 3] = ["next", "prior", "exists"];

fn constant(value: Value, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Constant {
            sql_type: value.sql_type(),
            value,
        },
        position,
    }
}

fn undeclared_component(component: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!(
            "PLS-00302: component '{}' must be declared",
            component.to_uppercase()
        ),
    )
    .at(position)
}

impl<'t> Parser<'t> {
    pub(super) fn plsql_primary(&mut self, frame: &'t RoutineFrame) -> SqlResult<Option<Expr>> {
        let position = self.position();
        let word: &'t str = match self.peek_kind() {
            TokenKind::Word(word) => word.as_ref(),
            TokenKind::QuotedIdentifier(name) => name.as_str(),
            _ => return Ok(None),
        };
        if matches!(self.peek_kind_at(1), TokenKind::Operator("%")) {
            return self.attribute_primary(frame, word, position);
        }
        if let Some((shape, value)) = frame.collection(word) {
            if !matches!(self.peek_kind_at(1), TokenKind::LeftParen | TokenKind::Dot) {
                return Ok(None);
            }
            self.at += 1;
            return self.collection_rest(shape, value, position).map(Some);
        }
        if self.peek_kind_at(1) != &TokenKind::LeftParen {
            return Ok(None);
        }
        if word == "sqlerrm" {
            self.at += 2;
            let code = self.plsql_constant()?;
            self.expect(&TokenKind::RightParen)?;
            let text = match convert(code, SqlType::BigInt, Conversion::Assignment)? {
                Value::BigInt(code) => Value::Text(crate::plsql::sqlerrm_of(code)),
                _ => Value::Null,
            };
            return Ok(Some(constant(text, position)));
        }
        if frame.index_of(word).is_some() {
            return Ok(None);
        }
        let Some(shape) = frame.constructor(word) else {
            return Ok(None);
        };
        self.at += 2;
        let mut elements = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                elements.push(self.plsql_constant()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        let value = shape
            .construct(elements)
            .map_err(|error| error.at(position))?;
        Ok(Some(constant(value, position)))
    }

    fn attribute_primary(
        &mut self,
        frame: &'t RoutineFrame,
        word: &str,
        position: usize,
    ) -> SqlResult<Option<Expr>> {
        let attribute: &'t str = match self.peek_kind_at(2) {
            TokenKind::Word(attribute) => attribute.as_ref(),
            _ => return Ok(None),
        };
        if word != "sql" {
            let Some(value) = frame.cursor_attribute(word, attribute) else {
                return Ok(None);
            };
            let value = value.map_err(|error| error.at(position))?;
            self.at += 3;
            return Ok(Some(constant(value, position)));
        }
        match attribute {
            "bulk_rowcount" => {
                self.at += 3;
                self.expect(&TokenKind::LeftParen)?;
                let key = self.plsql_constant()?;
                self.expect(&TokenKind::RightParen)?;
                let rows = bulk_rowcount_shape()
                    .element(&frame.bulk_rowcounts(), &key)
                    .map_err(|error| error.at(position))?;
                Ok(Some(constant(rows, position)))
            }
            "bulk_exceptions" => {
                self.at += 3;
                let exceptions = frame.bulk_exceptions();
                self.collection_rest(&bulk_exceptions_shape(), &exceptions, position)
                    .map(Some)
            }
            _ => {
                let Some(value) = frame.implicit_attribute(attribute) else {
                    return Err(undeclared_component(attribute, position));
                };
                self.at += 3;
                Ok(Some(constant(value, position)))
            }
        }
    }

    fn plsql_constant(&mut self) -> SqlResult<Value> {
        let expr = self.expression()?;
        let evaluator = Evaluator {
            scope: Scope::empty(),
            row: &[],
            correlation: None,
        };
        evaluator.eval(&expr, None)
    }

    fn collection_rest(
        &mut self,
        shape: &CollectionShape,
        collection: &Value,
        position: usize,
    ) -> SqlResult<Expr> {
        if self.peek_kind() == &TokenKind::Dot {
            return self.collection_method(shape, collection, position);
        }
        if !self.take(&TokenKind::LeftParen) {
            return Ok(constant(collection.clone(), position));
        }
        let key = self.plsql_constant()?;
        self.expect(&TokenKind::RightParen)?;
        let element = shape
            .element(collection, &key)
            .map_err(|error| error.at(position))?;
        if let Some(inner) = shape.nested()
            && matches!(self.peek_kind(), TokenKind::LeftParen | TokenKind::Dot)
        {
            return self.collection_rest(inner, &element, position);
        }
        self.element_fields(element, position)
    }

    fn collection_method(
        &mut self,
        shape: &CollectionShape,
        collection: &Value,
        position: usize,
    ) -> SqlResult<Expr> {
        let method: &'t str = match self.peek_kind_at(1) {
            TokenKind::Word(method) => method.as_ref(),
            _ => return Err(self.unexpected()),
        };
        self.at += 2;
        let argument = if METHODS_WITH_ARGUMENT.contains(&method) {
            self.expect(&TokenKind::LeftParen)?;
            let key = self.plsql_constant()?;
            self.expect(&TokenKind::RightParen)?;
            Some(key)
        } else {
            None
        };
        match shape.method(collection, method, argument.as_ref()) {
            Some(result) => Ok(constant(
                result.map_err(|error| error.at(position))?,
                position,
            )),
            None => Err(undeclared_component(method, position)),
        }
    }

    fn element_fields(&mut self, element: Value, position: usize) -> SqlResult<Expr> {
        let mut current = element;
        while self.peek_kind() == &TokenKind::Dot
            && self.peek_kind_at(2) != &TokenKind::LeftParen
        {
            let field: &'t str = match self.peek_kind_at(1) {
                TokenKind::Word(field) => field.as_ref(),
                TokenKind::QuotedIdentifier(field) => field.as_str(),
                _ => break,
            };
            self.at += 2;
            current = crate::records::field(&current, field)
                .map_err(|_| undeclared_component(field, position))?;
        }
        Ok(constant(current, position))
    }
}
