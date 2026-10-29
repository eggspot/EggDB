use super::{Parser, oracle, sqlite};
use crate::ast::{
    BinaryOperator, CommonTable, DerivedTable, Expr, ExprKind, GeneratedTable, Literal, Name,
    OrderItem, Query, Select, SetExpr, SetOperation, SetOperator, Statement, Subquery,
    SubqueryState, SubqueryTest, TableReference,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::types::SqlType;

pub(super) const MAX_QUERY_NESTING: usize = 32;
pub(super) const MAX_QUERIES: usize = 1024;

#[derive(Default)]
pub(super) struct QueryTail {
    pub(super) order_by: Vec<OrderItem>,
    pub(super) limit: Option<Expr>,
    pub(super) offset: Option<Expr>,
}

impl QueryTail {
    fn is_empty(&self) -> bool {
        self.order_by.is_empty() && self.limit.is_none() && self.offset.is_none()
    }
}

fn too_many_queries(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        format!("a statement can hold at most {MAX_QUERIES} SELECTs"),
    )
    .with_hint("Split the statement, or use fewer subqueries and set operations.")
    .at(position)
}

fn nested_too_deep(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        "stack depth limit exceeded",
    )
    .with_hint(format!(
        "The statement nests subqueries more than {MAX_QUERY_NESTING} levels deep."
    ))
    .at(position)
}

impl Parser<'_> {
    pub(super) fn starts_query(&self) -> bool {
        self.starts_query_at(0)
    }

    fn starts_query_at(&self, ahead: usize) -> bool {
        self.is_word_at(ahead, "select")
            || self.is_word_at(ahead, "with")
            || (self.postgresql() && self.is_word_at(ahead, "values"))
    }

    fn starts_query_in_parentheses(&self) -> bool {
        let mut ahead = 0;
        while self.peek_kind_at(ahead) == &TokenKind::LeftParen {
            ahead += 1;
        }
        ahead > 0 && self.starts_query_at(ahead)
    }

    pub(super) fn query_statement(&mut self) -> SqlResult<Statement> {
        let query = self.query()?;
        Ok(match query {
            Query {
                with,
                body: SetExpr::Select(select),
                order_by,
                limit: None,
                offset: None,
                ..
            } if with.is_empty() && order_by.is_empty() => Statement::Select(*select),
            query => Statement::Query(Box::new(query)),
        })
    }

    pub(super) fn nested_query(&mut self) -> SqlResult<Query> {
        let position = self.position();
        self.query_depth += 1;
        self.depth += 1;
        let result = if self.query_depth > MAX_QUERY_NESTING || self.depth > super::MAX_NESTING {
            Err(nested_too_deep(position))
        } else {
            self.query()
        };
        self.query_depth -= 1;
        self.depth -= 1;
        result
    }

    pub(super) fn query(&mut self) -> SqlResult<Query> {
        let position = self.position();
        let (with, recursive) = if self.take_word("with") {
            self.common_tables()?
        } else {
            (Vec::new(), false)
        };
        if !with.is_empty()
            && ["insert", "update", "delete"]
                .iter()
                .any(|word| self.is_word(word))
        {
            return Err(self.not_supported("WITH before INSERT, UPDATE or DELETE"));
        }
        let body = self.set_expression()?;
        let top = self.pending_top.take();
        let mut tail = self.query_tail()?;
        if self.ignored_view_top && self.at_view_top_level() {
            self.ignored_view_top = false;
            if !matches!(body, SetExpr::Select(_)) {
                return Err(super::sqlserver::not_supported("TOP ... PERCENT").at(position));
            }
            tail.order_by.clear();
        }
        if let Some(top) = top {
            if !matches!(body, SetExpr::Select(_)) {
                return Err(
                    super::sqlserver::not_supported("TOP with UNION, INTERSECT or EXCEPT")
                        .at(top.position),
                );
            }
            if tail.offset.is_some() || tail.limit.is_some() {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    "A TOP can not be used in the same query or sub-query as a OFFSET.",
                )
                .with_sqlserver_code(10741)
                .at(top.position));
            }
            tail.limit = Some(top);
        }
        let (body, tail) = match body {
            SetExpr::Select(mut select)
                if select.order_by.is_empty()
                    && select.limit.is_none()
                    && select.offset.is_none() =>
            {
                if self.oracle() {
                    oracle::reject_rownum_outside_where(
                        &select.items,
                        &select.group_by,
                        select.having.as_ref(),
                        &tail.order_by,
                    )?;
                }
                select.order_by = tail.order_by;
                select.limit = tail.limit;
                select.offset = tail.offset;
                (SetExpr::Select(select), QueryTail::default())
            }
            body => (body, tail),
        };
        Ok(Query {
            with,
            recursive,
            body,
            order_by: tail.order_by,
            limit: tail.limit,
            offset: tail.offset,
            position,
        })
    }

    fn common_tables(&mut self) -> SqlResult<(Vec<CommonTable>, bool)> {
        let recursive = self.take_word("recursive");
        if recursive && self.oracle() {
            return Err(self.unexpected());
        }
        let mut tables: Vec<CommonTable> = Vec::new();
        loop {
            let name = self.name()?;
            if tables.iter().any(|table| table.name.text == name.text) {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_ALIAS,
                    format!("WITH query name \"{}\" specified more than once", name.text),
                )
                .at(name.position));
            }
            let column_names = if self.peek_kind() == &TokenKind::LeftParen {
                self.name_list()?
            } else {
                Vec::new()
            };
            self.expect_word("as")?;
            if self.is_word("materialized") || self.is_word("not") {
                if !self.postgresql() {
                    return Err(self.unexpected());
                }
                self.take_word("not");
                self.expect_word("materialized")?;
            }
            self.expect(&TokenKind::LeftParen)?;
            if !self.starts_query() && !self.starts_query_in_parentheses() {
                if ["insert", "update", "delete"]
                    .iter()
                    .any(|word| self.is_word(word))
                {
                    return Err(self.not_supported("data-modifying statements in WITH"));
                }
                return Err(self.unexpected());
            }
            let query = self.nested_query()?;
            self.expect(&TokenKind::RightParen)?;
            if self.is_word("search") || self.is_word("cycle") {
                return Err(self.not_supported("SEARCH and CYCLE clauses"));
            }
            tables.push(CommonTable {
                name,
                column_names,
                query,
                dialect: self.dialect,
            });
            if !self.take(&TokenKind::Comma) {
                return Ok((tables, recursive));
            }
        }
    }

    fn set_operator(&self) -> Option<(SetOperator, bool)> {
        let TokenKind::Word(word) = self.peek_kind() else {
            return None;
        };
        let operator = match word.as_ref() {
            "union" => SetOperator::Union,
            "intersect" => SetOperator::Intersect,
            "except" => SetOperator::Except,
            "minus" if self.oracle() => SetOperator::Except,
            _ => return None,
        };
        let binds_tighter = operator == SetOperator::Intersect && !self.sqlite() && !self.oracle();
        Some((operator, binds_tighter))
    }

    fn set_expression(&mut self) -> SqlResult<SetExpr> {
        let mut left = self.set_term()?;
        while let Some((operator, false)) = self.set_operator() {
            let position = self.position();
            self.at += 1;
            let all = self.set_quantifier(operator)?;
            let right = self.set_term()?;
            left = self.set_operation(operator, all, left, right, position)?;
        }
        Ok(left)
    }

    fn set_term(&mut self) -> SqlResult<SetExpr> {
        let mut left = self.set_operand()?;
        while let Some((operator, true)) = self.set_operator() {
            let position = self.position();
            self.at += 1;
            let all = self.set_quantifier(operator)?;
            let right = self.set_operand()?;
            left = self.set_operation(operator, all, left, right, position)?;
        }
        Ok(left)
    }

    fn set_quantifier(&mut self, operator: SetOperator) -> SqlResult<bool> {
        if self.is_word("all") {
            if self.sqlite() && operator != SetOperator::Union {
                return Err(self.unexpected());
            }
            self.at += 1;
            return Ok(true);
        }
        if self.is_word("distinct") {
            if self.sqlite() || self.oracle() {
                return Err(self.unexpected());
            }
            self.at += 1;
        }
        Ok(false)
    }

    fn set_operation(
        &self,
        operator: SetOperator,
        all: bool,
        left: SetExpr,
        right: SetExpr,
        position: usize,
    ) -> SqlResult<SetExpr> {
        Ok(SetExpr::Operation(Box::new(SetOperation {
            operator,
            all,
            left,
            right,
            dialect: self.dialect,
            position,
        })))
    }

    fn set_operand(&mut self) -> SqlResult<SetExpr> {
        if let Some(top) = &self.pending_top {
            return Err(
                super::sqlserver::not_supported("TOP with UNION, INTERSECT or EXCEPT")
                    .at(top.position),
            );
        }
        self.count_query()?;
        if self.peek_kind() == &TokenKind::LeftParen {
            if self.sqlite() {
                return Err(self.unexpected());
            }
            self.at += 1;
            let query = self.nested_query()?;
            self.expect(&TokenKind::RightParen)?;
            let unwrapped = query.plain_select().is_some_and(|select| {
                select.order_by.is_empty() && select.limit.is_none() && select.offset.is_none()
            });
            return Ok(match query.body {
                SetExpr::Select(select) if unwrapped => SetExpr::Select(select),
                body => SetExpr::Query(Box::new(Query { body, ..query })),
            });
        }
        if self.postgresql() && self.is_word("values") {
            return Ok(SetExpr::Select(Box::new(self.values_list()?)));
        }
        if !self.is_word("select") {
            if self.is_word("values") || self.is_word("table") {
                return Err(self.not_supported("a VALUES or TABLE statement"));
            }
            return Err(self.unexpected());
        }
        Ok(SetExpr::Select(Box::new(self.select_core()?)))
    }

    fn count_query(&mut self) -> SqlResult<()> {
        self.queries += 1;
        if self.queries > MAX_QUERIES {
            return Err(too_many_queries(self.position()));
        }
        Ok(())
    }

    pub(super) fn order_items(&mut self) -> SqlResult<Vec<OrderItem>> {
        let mut order_by = Vec::new();
        loop {
            let expr = self.expression()?;
            let descending = if self.take_word("desc") {
                true
            } else {
                self.take_word("asc");
                false
            };
            if self.is_word("using") {
                return Err(self.not_supported("ORDER BY ... USING"));
            }
            let nulls_first = if self.take_word("nulls") {
                if self.take_word("first") {
                    Some(true)
                } else {
                    self.expect_word("last")?;
                    Some(false)
                }
            } else {
                None
            };
            order_by.push(OrderItem {
                expr,
                descending,
                nulls_first,
            });
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        Ok(order_by)
    }

    pub(super) fn query_tail(&mut self) -> SqlResult<QueryTail> {
        let mut order_by = Vec::new();
        if self.take_word("order") {
            self.expect_word("by")?;
            order_by = self.order_items()?;
        }
        let mut limit = None;
        let mut offset = None;
        if self.sqlserver() {
            (offset, limit) = self.sqlserver_offset_fetch(!order_by.is_empty())?;
        }
        loop {
            if self.sqlserver()
                && ["limit", "offset", "fetch", "for"]
                    .iter()
                    .any(|word| self.is_word(word))
            {
                break;
            }
            if self.oracle() && self.is_word("limit") {
                return Err(SqlError::syntax(
                    "SQL command not properly ended",
                    self.position(),
                ));
            }
            if self.oracle() && self.is_word("fetch") {
                if limit.is_some() {
                    return Err(self.unexpected());
                }
                limit = Some(self.oracle_fetch()?);
                continue;
            }
            if self.take_word("limit") {
                if limit.is_some() {
                    return Err(SqlError::syntax(
                        "multiple LIMIT clauses not allowed",
                        self.tokens[self.at - 1].start,
                    ));
                }
                limit = Some(if self.is_word("all") {
                    let position = self.position();
                    self.at += 1;
                    Expr {
                        kind: ExprKind::Literal(Literal::Null),
                        position,
                    }
                } else {
                    self.expression()?
                });
                if (self.mysql() || self.sqlite()) && self.take(&TokenKind::Comma) {
                    if offset.is_some() {
                        return Err(self.unexpected());
                    }
                    offset = limit.take();
                    limit = Some(self.expression()?);
                }
            } else if self.take_word("offset") {
                if offset.is_some() {
                    return Err(SqlError::syntax(
                        "multiple OFFSET clauses not allowed",
                        self.tokens[self.at - 1].start,
                    ));
                }
                offset = Some(self.expression()?);
                if self.oracle() {
                    offset = offset.map(oracle::not_below_zero);
                    if !self.take_word("rows") {
                        self.expect_word("row")?;
                    }
                }
                let _ = self.take_word("rows") || self.take_word("row");
            } else if self.is_word("fetch") {
                return Err(self.not_supported("FETCH FIRST"));
            } else if self.is_word("for") || self.mysql() && self.is_word("lock") {
                return Err(self.not_supported("row locking (FOR UPDATE, FOR SHARE)"));
            } else {
                break;
            }
        }
        if self.sqlite() {
            limit = limit.map(sqlite::without_negative_limit);
            offset = offset.map(sqlite::without_negative_offset);
        }
        let tail = QueryTail {
            order_by,
            limit,
            offset,
        };
        if !tail.is_empty() && self.set_operator().is_some() {
            return Err(self.unexpected());
        }
        Ok(tail)
    }

    pub(super) fn derived_table(&mut self) -> SqlResult<TableReference> {
        let position = self.position();
        self.expect(&TokenKind::LeftParen)?;
        self.count_query()?;
        let mut query = self.nested_query()?;
        self.expect(&TokenKind::RightParen)?;
        let alias = if self.take_word("as") || self.at_bare_alias() {
            Some(self.name()?.text)
        } else {
            None
        };
        if alias.is_none() && self.mysql() {
            return Err(SqlError::new(
                SqlState::MYSQL_SYNTAX_OR_ACCESS,
                "Every derived table must have its own alias",
            )
            .with_vendor_code(1248)
            .at(position));
        }
        let column_names = if alias.is_some() && self.peek_kind() == &TokenKind::LeftParen {
            if self.oracle() || self.sqlite() {
                return Err(self.unexpected());
            }
            self.name_list()?
        } else {
            Vec::new()
        };
        let name = Name {
            text: alias
                .clone()
                .unwrap_or_else(|| "unnamed_subquery".to_owned()),
            position,
        };
        if self.postgresql()
            && let Some(mut list) = Self::values_table(&mut query)
        {
            list.column_names = column_names;
            return Ok(TableReference {
                name,
                alias,
                derived: None,
                generated: Some(Box::new(GeneratedTable::Values(list))),
            });
        }
        Ok(TableReference {
            name,
            alias,
            derived: Some(Box::new(DerivedTable {
                query,
                column_names,
                dialect: self.dialect,
            })),
            generated: None,
        })
    }

    pub(super) fn at_derived_table(&self) -> bool {
        self.starts_query_in_parentheses()
    }

    pub(super) fn subquery_expression(
        &mut self,
        test: SubqueryTest,
        position: usize,
    ) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        if !self.starts_query() && !self.starts_query_in_parentheses() {
            return Err(self.unexpected());
        }
        self.count_query()?;
        let query = self.nested_query()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(Expr {
            kind: ExprKind::Subquery(Box::new(Subquery {
                test,
                query,
                state: SubqueryState::Unprepared,
                dialect: self.dialect,
            })),
            position,
        })
    }

    pub(super) fn at_subquery(&self) -> bool {
        self.peek_kind() == &TokenKind::LeftParen && self.starts_query_at(1)
    }

    pub(super) fn at_quantified(&self) -> bool {
        !self.sqlite()
            && (self.is_word("any") || self.is_word("some") || self.is_word("all"))
            && self.peek_kind_at(1) == &TokenKind::LeftParen
    }

    pub(super) fn quantified_comparison(
        &mut self,
        left: Expr,
        operator: BinaryOperator,
    ) -> SqlResult<Expr> {
        let all = self.is_word("all");
        self.at += 1;
        if !self.starts_query_at(1) {
            if self.postgresql() {
                return self.quantified_expression(left, operator, all);
            }
            return Err(self.not_supported("ANY, SOME and ALL over a list or an array"));
        }
        let position = left.position;
        self.subquery_expression(
            SubqueryTest::Compare {
                expr: left,
                operator,
                all,
            },
            position,
        )
    }

    pub(super) fn quantified_list(
        left: Expr,
        operator: BinaryOperator,
        all: bool,
        array: Expr,
    ) -> Result<Expr, Box<(Expr, Expr)>> {
        let position = left.position;
        let membership = match (operator, all) {
            (BinaryOperator::Equal, false) => Some(false),
            (BinaryOperator::NotEqual, true) => Some(true),
            _ => None,
        };
        if membership.is_none() && !is_cheap_to_repeat(&left) && !is_empty_constructor(&array) {
            return Err(Box::new((left, array)));
        }
        let elements = match listed_elements(array) {
            Ok(elements) => elements,
            Err(array) => return Err(Box::new((left, array))),
        };
        if elements.is_empty() {
            let constant = || Expr {
                kind: ExprKind::Literal(Literal::Boolean(all)),
                position,
            };
            return Ok(Expr {
                kind: if all {
                    ExprKind::And(vec![constant(), constant()])
                } else {
                    ExprKind::Or(vec![constant(), constant()])
                },
                position,
            });
        }
        if let Some(negated) = membership {
            return Ok(Expr {
                kind: ExprKind::InList {
                    expr: Box::new(left),
                    list: elements,
                    negated,
                },
                position,
            });
        }
        let mut comparisons: Vec<Expr> = elements
            .into_iter()
            .map(|element| Self::binary(left.clone(), operator, element))
            .collect();
        if comparisons.len() == 1 {
            return Ok(comparisons.remove(0));
        }
        Ok(Expr {
            kind: if all {
                ExprKind::And(comparisons)
            } else {
                ExprKind::Or(comparisons)
            },
            position,
        })
    }

    pub(super) fn select_core(&mut self) -> SqlResult<Select> {
        let mut select = self.select_clauses()?;
        select.order_by = Vec::new();
        Ok(select)
    }
}

fn constructor_elements(expr: &Expr) -> Option<&[Expr]> {
    match &expr.kind {
        ExprKind::Function { name, arguments } if name == "array_constructor" => Some(arguments),
        ExprKind::Cast {
            expr,
            sql_type: SqlType::Array(_),
        } => constructor_elements(expr),
        _ => None,
    }
}

fn is_empty_constructor(expr: &Expr) -> bool {
    constructor_elements(expr).is_some_and(<[Expr]>::is_empty)
}

fn listed_elements(array: Expr) -> Result<Vec<Expr>, Expr> {
    if constructor_elements(&array).is_none() {
        return Err(array);
    }
    Ok(constructor_elements_owned(array))
}

fn constructor_elements_owned(array: Expr) -> Vec<Expr> {
    match array.kind {
        ExprKind::Function { arguments, .. } => arguments,
        ExprKind::Cast {
            expr,
            sql_type: SqlType::Array(element),
        } => constructor_elements_owned(*expr)
            .into_iter()
            .map(|item| {
                let position = item.position;
                Expr {
                    kind: ExprKind::Cast {
                        expr: Box::new(item),
                        sql_type: element.sql_type(),
                    },
                    position,
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn is_cheap_to_repeat(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Column { .. } | ExprKind::Literal(_) => true,
        ExprKind::Cast { expr, .. } => is_cheap_to_repeat(expr),
        _ => false,
    }
}
