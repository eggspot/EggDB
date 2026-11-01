use super::{Level, Parser};
use crate::ast::{
    AggregateFunction, BinaryOperator, Expr, ExprKind, FromClause, GeneratedTable, Literal, Name,
    Query, Select, SelectItem, SetExpr, SubqueryTest, TableFunction, TableReference, ValuesList,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;

const ACCEPTED_COLLATIONS: [&str; 4] = ["default", "C", "POSIX", "ucs_basic"];

fn function(name: &str, arguments: Vec<Expr>, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

fn ordered(argument: Expr, order: Vec<crate::ast::OrderItem>) -> Expr {
    if order.is_empty() {
        argument
    } else {
        crate::aggregate::ordered_arguments(argument, order)
    }
}

fn no_operator(symbol: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("operator does not exist: {symbol}"),
    )
    .with_hint(
        "No operator matches the given name and argument types. You might need to add explicit type casts.",
    )
    .at(position)
}

impl Parser<'_> {
    pub(super) fn regex_operator(&self) -> Option<BinaryOperator> {
        let TokenKind::Operator(symbol) = self.peek_kind() else {
            return None;
        };
        Some(match *symbol {
            "~" => BinaryOperator::RegexMatch,
            "~*" => BinaryOperator::RegexMatchFolded,
            "!~" => BinaryOperator::RegexNoMatch,
            "!~*" => BinaryOperator::RegexNoMatchFolded,
            _ => return None,
        })
    }

    fn identifier_at(&self, ahead: usize) -> Option<&str> {
        match self.peek_kind_at(ahead) {
            TokenKind::Word(word) => Some(word.as_ref()),
            TokenKind::QuotedIdentifier(name) => Some(name.as_str()),
            _ => None,
        }
    }

    pub(super) fn explicit_operator(&mut self, left: Expr) -> SqlResult<Expr> {
        let position = self.position();
        self.at += 1;
        self.expect(&TokenKind::LeftParen)?;
        let schema = match self.identifier_at(0) {
            Some(schema) if self.peek_kind_at(1) == &TokenKind::Dot => {
                let schema = schema.to_owned();
                self.at += 2;
                Some(schema)
            }
            _ => None,
        };
        let TokenKind::Operator(symbol) = self.peek_kind() else {
            return Err(self.unexpected());
        };
        let symbol: &str = symbol;
        self.at += 1;
        self.expect(&TokenKind::RightParen)?;
        if let Some(schema) = schema.filter(|schema| schema != "pg_catalog") {
            return Err(no_operator(&format!("{schema}.{symbol}"), position));
        }
        let comparison = match symbol {
            "=" => Some(BinaryOperator::Equal),
            "<>" | "!=" => Some(BinaryOperator::NotEqual),
            "<" => Some(BinaryOperator::Less),
            "<=" => Some(BinaryOperator::LessOrEqual),
            ">" => Some(BinaryOperator::Greater),
            ">=" => Some(BinaryOperator::GreaterOrEqual),
            _ => None,
        };
        if let Some(operator) = comparison
            && self.at_quantified()
        {
            return self.quantified_comparison(left, operator);
        }
        let like = match symbol {
            "~~" => Some((false, false)),
            "!~~" => Some((true, false)),
            "~~*" => Some((false, true)),
            "!~~*" => Some((true, true)),
            _ => None,
        };
        let operator = comparison.or(match symbol {
            "+" => Some(BinaryOperator::Add),
            "-" => Some(BinaryOperator::Subtract),
            "*" => Some(BinaryOperator::Multiply),
            "/" => Some(BinaryOperator::Divide),
            "%" => Some(BinaryOperator::Modulo),
            "^" => Some(BinaryOperator::Power),
            "||" => Some(BinaryOperator::Concat),
            "~" => Some(BinaryOperator::RegexMatch),
            "~*" => Some(BinaryOperator::RegexMatchFolded),
            "!~" => Some(BinaryOperator::RegexNoMatch),
            "!~*" => Some(BinaryOperator::RegexNoMatchFolded),
            "->" => Some(BinaryOperator::JsonField),
            "->>" => Some(BinaryOperator::JsonFieldText),
            "#>" => Some(BinaryOperator::JsonPath),
            "#>>" => Some(BinaryOperator::JsonPathText),
            "@>" => Some(BinaryOperator::JsonContains),
            "<@" => Some(BinaryOperator::JsonContainedIn),
            "?" => Some(BinaryOperator::JsonHasKey),
            "?|" => Some(BinaryOperator::JsonHasAnyKey),
            "?&" => Some(BinaryOperator::JsonHasAllKeys),
            "#-" => Some(BinaryOperator::JsonDeletePath),
            "@@" => Some(BinaryOperator::TextSearchMatch),
            "<->" => Some(BinaryOperator::VectorL2Distance),
            "<=>" => Some(BinaryOperator::VectorCosineDistance),
            "<#>" => Some(BinaryOperator::VectorNegativeInnerProduct),
            "<+>" => Some(BinaryOperator::VectorL1Distance),
            _ => None,
        });
        if operator.is_none() && like.is_none() {
            return Err(no_operator(symbol, position));
        }
        let right = self.expression_above(Level::Additive)?;
        if let Some((negated, case_insensitive)) = like {
            let position = left.position;
            return Ok(Expr {
                kind: ExprKind::Like {
                    expr: Box::new(left),
                    pattern: Box::new(right),
                    negated,
                    case_insensitive,
                },
                position,
            });
        }
        match operator {
            Some(operator) => Ok(Self::binary(left, operator, right)),
            None => Err(no_operator(symbol, position)),
        }
    }

    pub(super) fn collate(&mut self, expr: Expr) -> SqlResult<Expr> {
        self.at += 1;
        let position = self.position();
        let qualified = self.identifier_at(0).is_some() && self.peek_kind_at(1) == &TokenKind::Dot;
        let schema = if qualified {
            let schema = self.identifier_at(0).unwrap_or_default().to_owned();
            self.at += 2;
            Some(schema)
        } else {
            None
        };
        let Some(name) = self.identifier_at(0).map(str::to_owned) else {
            return Err(self.unexpected());
        };
        self.at += 1;
        let shown = match &schema {
            Some(schema) => format!("{schema}.{name}"),
            None => name.clone(),
        };
        let known = schema
            .as_deref()
            .is_none_or(|schema| schema == "pg_catalog")
            && ACCEPTED_COLLATIONS.contains(&name.as_str());
        if !known {
            return Err(SqlError::new(
                SqlState::UNDEFINED_OBJECT,
                format!("collation \"{shown}\" for encoding \"UTF8\" does not exist"),
            )
            .at(position));
        }
        if matches!(expr.kind, ExprKind::Literal(Literal::String(_))) {
            return Ok(expr);
        }
        let at = expr.position;
        Ok(function("collate", vec![expr], at))
    }

    pub(super) fn subscript(&mut self, array: Expr) -> SqlResult<Expr> {
        let position = array.position;
        self.expect(&TokenKind::LeftBracket)?;
        if self.peek_kind() == &TokenKind::Colon {
            return Err(self.not_supported("array slices"));
        }
        let index = self.expression()?;
        if self.peek_kind() == &TokenKind::Colon {
            return Err(self.not_supported("array slices"));
        }
        self.expect(&TokenKind::RightBracket)?;
        Ok(function("array_subscript", vec![array, index], position))
    }

    pub(super) fn array_constructor(&mut self) -> SqlResult<Expr> {
        let position = self.position();
        if self.peek_kind_at(1) == &TokenKind::LeftParen {
            self.at += 1;
            return self.subquery_expression(SubqueryTest::Array, position);
        }
        self.at += 2;
        let mut elements = Vec::new();
        if !self.take(&TokenKind::RightBracket) {
            loop {
                if self.peek_kind() == &TokenKind::LeftBracket {
                    return Err(self.not_supported("multidimensional arrays"));
                }
                elements.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightBracket)?;
        }
        Ok(function("array_constructor", elements, position))
    }

    pub(super) fn quantified_expression(
        &mut self,
        left: Expr,
        operator: BinaryOperator,
        all: bool,
    ) -> SqlResult<Expr> {
        let position = left.position;
        let left_is_parameter = self.starts_with_parameter(&left);
        self.expect(&TokenKind::LeftParen)?;
        let array = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        let (left, array) = if left_is_parameter {
            (left, array)
        } else {
            match Self::quantified_list(left, operator, all, array) {
                Ok(listed) => return Ok(listed),
                Err(operands) => *operands,
            }
        };
        let symbol = Expr {
            kind: ExprKind::Literal(Literal::String(operator.symbol().to_owned())),
            position,
        };
        let name = if all { "array_all" } else { "array_any" };
        Ok(function(name, vec![left, array, symbol], position))
    }

    fn starts_with_parameter(&self, expr: &Expr) -> bool {
        let parameter_shaped = matches!(
            expr.kind,
            ExprKind::Literal(_) | ExprKind::Constant { .. } | ExprKind::Cast { .. }
        );
        parameter_shaped
            && self
                .tokens
                .binary_search_by_key(&expr.position, |token| token.start)
                .ok()
                .and_then(|index| self.tokens.get(index))
                .is_some_and(|token| {
                    matches!(
                        token.kind,
                        TokenKind::Parameter(_)
                            | TokenKind::Placeholder
                            | TokenKind::NumberedParameter(_)
                    )
                })
    }

    pub(super) fn at_table_function(&self) -> bool {
        let callable = |ahead: usize| match self.peek_kind_at(ahead) {
            TokenKind::Word(word) => !self.reserved(word),
            TokenKind::QuotedIdentifier(_) => true,
            _ => false,
        };
        (callable(0) && self.peek_kind_at(1) == &TokenKind::LeftParen)
            || (callable(0)
                && self.peek_kind_at(1) == &TokenKind::Dot
                && callable(2)
                && self.peek_kind_at(3) == &TokenKind::LeftParen)
    }

    pub(super) fn table_function(&mut self) -> SqlResult<TableReference> {
        let position = self.position();
        let schema = if self.peek_kind_at(1) == &TokenKind::Dot {
            let schema = self.name()?;
            self.at += 1;
            Some(schema.text)
        } else {
            None
        };
        let mut name = self.name()?;
        match schema.as_deref() {
            None | Some(crate::schemas::CATALOG_SCHEMA) => {}
            Some(schema) => {
                name.text = crate::schemas::written_qualified(schema, &name.text);
            }
        }
        self.expect(&TokenKind::LeftParen)?;
        let mut arguments = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                arguments.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        let ordinality = self.is_word("with") && self.is_word_at(1, "ordinality");
        if ordinality {
            self.at += 2;
        }
        let alias = if self.take_word("as") || self.at_bare_alias() {
            Some(self.name()?.text)
        } else {
            None
        };
        let column_names = if alias.is_some() && self.peek_kind() == &TokenKind::LeftParen {
            self.name_list()?
        } else {
            Vec::new()
        };
        Ok(TableReference {
            name: Name {
                text: alias
                    .clone()
                    .unwrap_or_else(|| crate::schemas::base_name(&name.text).to_owned()),
                position,
            },
            alias,
            derived: None,
            generated: Some(Box::new(GeneratedTable::Function(TableFunction {
                name: name.text,
                arguments,
                ordinality,
                column_names,
                position,
            }))),
        })
    }

    pub(super) fn values_list(&mut self) -> SqlResult<Select> {
        let position = self.position();
        self.at += 1;
        let mut rows: Vec<Vec<Expr>> = Vec::new();
        loop {
            let row_position = self.position();
            self.expect(&TokenKind::LeftParen)?;
            if self.is_word("default") {
                return Err(self.not_supported("DEFAULT in a VALUES list outside INSERT"));
            }
            let mut row = vec![self.expression()?];
            while self.take(&TokenKind::Comma) {
                row.push(self.expression()?);
            }
            self.expect(&TokenKind::RightParen)?;
            if rows.first().is_some_and(|first| first.len() != row.len()) {
                return Err(SqlError::syntax(
                    "VALUES lists must all be the same length",
                    row_position,
                ));
            }
            rows.push(row);
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        let label = crate::set_returning::VALUES_LABEL;
        Ok(Select {
            distinct: false,
            items: vec![SelectItem::Wildcard {
                qualifier: None,
                position,
            }],
            from: Some(FromClause {
                first: TableReference {
                    name: Name {
                        text: label.to_owned(),
                        position,
                    },
                    alias: Some(label.to_owned()),
                    derived: None,
                    generated: Some(Box::new(GeneratedTable::Values(ValuesList {
                        rows,
                        column_names: Vec::new(),
                        position,
                    }))),
                },
                joins: Vec::new(),
            }),
            filter: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            source_row_limit: None,
        })
    }

    pub(super) fn values_table(query: &mut Query) -> Option<ValuesList> {
        let bare = query.with.is_empty()
            && query.order_by.is_empty()
            && query.limit.is_none()
            && query.offset.is_none();
        let SetExpr::Select(select) = &mut query.body else {
            return None;
        };
        if !bare || !is_values_select(select) {
            return None;
        }
        let generated = select.from.as_mut()?.first.generated.take()?;
        let GeneratedTable::Values(list) = *generated else {
            return None;
        };
        Some(list)
    }

    pub(super) fn postgresql_trim(&mut self, name: Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let internal = if self.take_word("leading") {
            "ltrim"
        } else if self.take_word("trailing") {
            "rtrim"
        } else {
            self.take_word("both");
            "btrim"
        };
        let mut arguments = Vec::with_capacity(2);
        if self.take_word("from") {
            arguments.push(self.expression()?);
            if self.take(&TokenKind::Comma) {
                arguments.push(self.expression()?);
            }
        } else {
            let first = self.expression()?;
            if self.take_word("from") {
                arguments.push(self.expression()?);
                arguments.push(first);
            } else {
                arguments.push(first);
                if self.take(&TokenKind::Comma) {
                    arguments.push(self.expression()?);
                }
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(function(internal, arguments, name.position))
    }

    pub(super) fn postgresql_function(&mut self, name: &Name) -> SqlResult<Option<Expr>> {
        match name.text.as_str() {
            "position" => self.position_call(name).map(Some),
            "overlay" => self.overlay_call(name).map(Some),
            "row_to_json"
                if self.is_word_at(1, "row") && self.peek_kind_at(2) == &TokenKind::LeftParen =>
            {
                self.row_to_json_call(name).map(Some)
            }
            "json_object_agg" | "jsonb_object_agg" => self.object_agg(name).map(Some),
            text => match AggregateFunction::postgresql_named(text) {
                Some(function) => self.aggregate(name.clone(), function).map(Some),
                None => Ok(None),
            },
        }
    }

    fn position_call(&mut self, name: &Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let wanted = self.expression_above(Level::Other)?;
        self.expect_word("in")?;
        let text = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(function("position", vec![text, wanted], name.position))
    }

    fn overlay_call(&mut self, name: &Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let text = self.expression()?;
        let mut arguments = vec![text];
        if self.take_word("placing") {
            arguments.push(self.expression()?);
            self.expect_word("from")?;
            arguments.push(self.expression()?);
            if self.take_word("for") {
                arguments.push(self.expression()?);
            }
        } else {
            while self.take(&TokenKind::Comma) {
                arguments.push(self.expression()?);
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(function("overlay", arguments, name.position))
    }

    fn row_to_json_call(&mut self, name: &Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let row_position = self.position();
        self.at += 1;
        self.expect(&TokenKind::LeftParen)?;
        let mut fields = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                fields.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        let mut arguments = vec![function(
            crate::json_functions::ROW_OBJECT,
            fields,
            row_position,
        )];
        if self.take(&TokenKind::Comma) {
            arguments.push(self.expression()?);
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(function("row_to_json", arguments, name.position))
    }

    fn object_agg(&mut self, name: &Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let distinct = self.take_word("distinct");
        if !distinct {
            self.take_word("all");
        }
        let mut arguments = vec![self.expression()?];
        while self.take(&TokenKind::Comma) {
            arguments.push(self.expression()?);
        }
        let order = if self.take_word("order") {
            self.expect_word("by")?;
            self.order_items()?
        } else {
            Vec::new()
        };
        self.expect(&TokenKind::RightParen)?;
        let Some(aggregate) = AggregateFunction::postgresql_named(&name.text) else {
            return Err(self.unexpected());
        };
        if arguments.len() != 2 {
            let unknown = vec!["unknown"; arguments.len()].join(", ");
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {}({unknown}) does not exist", name.text),
            )
            .with_hint(
                "No function matches the given name and argument types. You might need to add explicit type casts.",
            )
            .at(name.position));
        }
        Ok(Expr {
            kind: ExprKind::Aggregate {
                function: aggregate,
                argument: Some(Box::new(ordered(
                    function("object_agg_arguments", arguments, name.position),
                    order,
                ))),
                distinct,
            },
            position: name.position,
        })
    }

    pub(super) fn string_agg(&mut self, name: Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let distinct = self.take_word("distinct");
        if !distinct {
            self.take_word("all");
        }
        let mut arguments = vec![self.expression()?];
        while self.take(&TokenKind::Comma) {
            arguments.push(self.expression()?);
        }
        let order = if self.take_word("order") {
            self.expect_word("by")?;
            self.order_items()?
        } else {
            Vec::new()
        };
        self.expect(&TokenKind::RightParen)?;
        if arguments.len() != 2 {
            let unknown = vec!["unknown"; arguments.len()].join(", ");
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function string_agg({unknown}) does not exist"),
            )
            .with_hint(
                "No function matches the given name and argument types. You might need to add explicit type casts.",
            )
            .at(name.position));
        }
        Ok(Expr {
            kind: ExprKind::Aggregate {
                function: AggregateFunction::StringAgg,
                argument: Some(Box::new(ordered(
                    function("string_agg_arguments", arguments, name.position),
                    order,
                ))),
                distinct,
            },
            position: name.position,
        })
    }
}

fn is_values_select(select: &Select) -> bool {
    let Some(from) = &select.from else {
        return false;
    };
    !select.distinct
        && matches!(
            select.items.as_slice(),
            [SelectItem::Wildcard {
                qualifier: None,
                ..
            }]
        )
        && from.joins.is_empty()
        && from.first.name.text == crate::set_returning::VALUES_LABEL
        && matches!(
            from.first.generated.as_deref(),
            Some(GeneratedTable::Values(_))
        )
        && select.filter.is_none()
        && select.group_by.is_empty()
        && select.having.is_none()
        && select.order_by.is_empty()
        && select.limit.is_none()
        && select.offset.is_none()
}
