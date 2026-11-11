use super::Parser;
use crate::ast::{AggregateFunction, Expr, ExprKind, Name, OrderItem};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::window::{
    Frame, FrameBound, FrameClause, FrameExclusion, FrameUnits, ValueTreatment, WindowCall,
    WindowDefinition, WindowFunction, WindowReference,
};

fn wrong_kind(message: String, position: usize) -> SqlError {
    SqlError::new(SqlState::WRONG_OBJECT_TYPE, message).at(position)
}

fn no_such_function(name: &str, arguments: usize, position: usize) -> SqlError {
    let unknown = vec!["unknown"; arguments].join(", ");
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!("function {name}({unknown}) does not exist"),
    )
    .with_hint(
        "No function matches the given name and argument types. You might need to add explicit type casts.",
    )
    .at(position)
}

pub(super) fn whole_partition_call(
    function: AggregateFunction,
    arguments: Vec<Expr>,
    partition: Vec<Expr>,
    order: Vec<OrderItem>,
    position: usize,
) -> Expr {
    let frame = FrameClause {
        frame: Frame {
            units: FrameUnits::Rows,
            start: FrameBound::UnboundedPreceding,
            end: FrameBound::UnboundedFollowing,
            exclusion: FrameExclusion::NoOthers,
        },
        start_offset: None,
        end_offset: None,
    };
    crate::window::call(WindowCall {
        function: WindowFunction::Aggregate(function),
        treatment: ValueTreatment::default(),
        arguments,
        filter: None,
        window: WindowReference::Inline(WindowDefinition {
            base: None,
            partition,
            order,
            frame: Some(frame),
        }),
        position,
    })
}

impl Parser<'_> {
    fn over_follows(&self) -> bool {
        self.is_word("over")
            && match self.peek_kind_at(1) {
                TokenKind::LeftParen | TokenKind::QuotedIdentifier(_) => true,
                TokenKind::Word(word) => !self.reserved(word) && !self.clause_word(word),
                _ => false,
            }
    }

    fn filter_follows(&self) -> bool {
        self.is_word("filter") && self.peek_kind_at(1) == &TokenKind::LeftParen
    }

    pub(super) fn call_clauses(&mut self, expr: Expr, name: &str) -> SqlResult<Expr> {
        let windowed = self.over_follows();
        let filtered = self.filter_follows();
        let grouped = self.is_word("within") && self.is_word_at(1, "group");
        if !windowed && !filtered && !grouped {
            return Ok(expr);
        }
        let position = expr.position;
        if let ExprKind::SqlServerFunction {
            function: crate::sqlserver_functions::Function::AggregateResult { .. },
            arguments,
        } = &expr.kind
            && matches!(
                arguments.as_slice(),
                [Expr {
                    kind: ExprKind::Aggregate { .. },
                    ..
                }]
            )
        {
            let ExprKind::SqlServerFunction {
                function,
                mut arguments,
            } = expr.kind
            else {
                return Err(self.unexpected());
            };
            let inner = arguments.remove(0);
            let clauses = self.call_clauses(inner, name)?;
            return Ok(Expr {
                kind: ExprKind::SqlServerFunction {
                    function,
                    arguments: vec![clauses],
                },
                position,
            });
        }
        if !matches!(expr.kind, ExprKind::Aggregate { .. }) {
            let message = if windowed {
                format!(
                    "OVER specified, but {name} is not a window function nor an aggregate function"
                )
            } else if filtered {
                format!("FILTER specified, but {name} is not an aggregate function")
            } else {
                format!("WITHIN GROUP specified, but {name} is not an aggregate function")
            };
            return Err(wrong_kind(message, position));
        }
        if grouped {
            return Err(wrong_kind(
                format!("WITHIN GROUP specified, but {name} is not an ordered-set aggregate"),
                position,
            ));
        }
        let filter = if filtered {
            Some(self.aggregate_filter()?)
        } else {
            None
        };
        if self.over_follows() {
            return self.windowed_aggregate(expr, filter);
        }
        Ok(match filter {
            Some(condition) => crate::aggregate::filtered(expr, condition),
            None => expr,
        })
    }

    pub(super) fn aggregate_filter(&mut self) -> SqlResult<Expr> {
        self.expect_word("filter")?;
        self.expect(&TokenKind::LeftParen)?;
        self.expect_word("where")?;
        let condition = self.expression()?;
        let condition = self.condition(condition);
        self.expect(&TokenKind::RightParen)?;
        Ok(condition)
    }

    fn windowed_aggregate(&mut self, expr: Expr, filter: Option<Expr>) -> SqlResult<Expr> {
        let position = expr.position;
        let ExprKind::Aggregate {
            function,
            argument,
            distinct,
        } = expr.kind
        else {
            return Err(self.unexpected());
        };
        if function.is_ordered_set() || function.is_hypothetical() {
            return Err(SqlError::not_supported(format!(
                "OVER is not supported for ordered-set aggregate {}",
                function.name()
            ))
            .at(position));
        }
        if distinct && !self.oracle() {
            return Err(SqlError::not_supported(
                "DISTINCT is not implemented for window functions",
            )
            .at(position));
        }
        let arguments = match argument {
            None => Vec::new(),
            Some(argument) => {
                let Expr { kind, position } = *argument;
                match kind {
                    ExprKind::Function { name, .. }
                        if name == crate::aggregate::ORDERED_ARGUMENTS =>
                    {
                        return Err(SqlError::not_supported(
                            "aggregate ORDER BY is not implemented for window functions",
                        )
                        .at(position));
                    }
                    ExprKind::Function { name, arguments }
                        if name == "string_agg_arguments"
                            || name == "object_agg_arguments"
                            || name == crate::bivariate_aggregates::PAIRED_ARGUMENTS =>
                    {
                        arguments
                    }
                    kind => vec![Expr { kind, position }],
                }
            }
        };
        if distinct {
            let window = self.window_reference()?;
            if let WindowReference::Inline(definition) = &window
                && !definition.order.is_empty()
            {
                return Err(SqlError::syntax("ORDER BY not allowed here", position)
                    .with_oracle_code(30487));
            }
            return Ok(crate::window::call(WindowCall {
                function: WindowFunction::Aggregate(function),
                treatment: ValueTreatment {
                    distinct: true,
                    ..ValueTreatment::default()
                },
                arguments,
                filter,
                window,
                position,
            }));
        }
        self.windowed(
            WindowFunction::Aggregate(function),
            arguments,
            filter,
            position,
        )
    }

    pub(super) fn window_call(
        &mut self,
        function: WindowFunction,
        arguments: Vec<Expr>,
        filter: Option<Expr>,
        name: &Name,
    ) -> SqlResult<Expr> {
        if !function.accepts_arguments(arguments.len()) {
            return Err(no_such_function(&name.text, arguments.len(), name.position));
        }
        if filter.is_some() {
            return Err(SqlError::not_supported(
                "FILTER is not implemented for non-aggregate window functions",
            )
            .at(name.position));
        }
        if !self.over_follows() {
            return Err(SqlError::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("window function {} requires an OVER clause", name.text),
            )
            .at(name.position));
        }
        self.windowed(function, arguments, None, name.position)
    }

    /// A window function call whose null treatment (`IGNORE NULLS`) was read with its arguments,
    /// as Transact-SQL writes it after the closing parenthesis.
    pub(super) fn treated_window_call(
        &mut self,
        function: WindowFunction,
        arguments: Vec<Expr>,
        treatment: ValueTreatment,
        name: &Name,
    ) -> SqlResult<Expr> {
        if !function.accepts_arguments(arguments.len()) {
            return Err(no_such_function(&name.text, arguments.len(), name.position));
        }
        let window = self.window_reference()?;
        Ok(crate::window::call(WindowCall {
            function,
            treatment,
            arguments,
            filter: None,
            window,
            position: name.position,
        }))
    }

    pub(super) fn whole_partition_aggregate(
        &mut self,
        function: AggregateFunction,
        arguments: Vec<Expr>,
        order: Vec<OrderItem>,
        position: usize,
    ) -> SqlResult<Expr> {
        let partition = self.partition_only_window()?;
        Ok(whole_partition_call(
            function, arguments, partition, order, position,
        ))
    }

    fn windowed(
        &mut self,
        function: WindowFunction,
        arguments: Vec<Expr>,
        filter: Option<Expr>,
        position: usize,
    ) -> SqlResult<Expr> {
        let window = self.window_reference()?;
        Ok(crate::window::call(WindowCall {
            function,
            treatment: ValueTreatment::default(),
            arguments,
            filter,
            window,
            position,
        }))
    }

    pub(super) fn oracle_window_call(
        &mut self,
        function: WindowFunction,
        arguments: Vec<Expr>,
        treatment: ValueTreatment,
        name: &Name,
    ) -> SqlResult<Expr> {
        if !function.accepts_arguments(arguments.len()) {
            return Err(
                SqlError::syntax("invalid number of arguments", name.position)
                    .with_oracle_code(909),
            );
        }
        if !self.is_word("over") {
            return Err(Self::missing_window(name.position));
        }
        let window = self.window_reference()?;
        if function.needs_order()
            && matches!(
                &window,
                WindowReference::Inline(definition)
                    if definition.order.is_empty() && definition.base.is_none()
            )
        {
            return Err(SqlError::new(
                SqlState::WINDOWING_ERROR,
                "missing ORDER BY expression in the window specification",
            )
            .with_oracle_code(30485)
            .at(name.position));
        }
        Ok(crate::window::call(WindowCall {
            function,
            treatment,
            arguments,
            filter: None,
            window,
            position: name.position,
        }))
    }

    pub(super) fn partition_only_window(&mut self) -> SqlResult<Vec<Expr>> {
        self.expect_word("over")?;
        self.expect(&TokenKind::LeftParen)?;
        let mut partition = Vec::new();
        if self.take_word("partition") {
            self.expect_word("by")?;
            loop {
                partition.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        if self.oracle() && self.is_word("order") {
            return Err(
                SqlError::syntax("ORDER BY not allowed here", self.position())
                    .with_oracle_code(30487),
            );
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(partition)
    }

    pub(super) fn window_reference(&mut self) -> SqlResult<WindowReference> {
        self.expect_word("over")?;
        if self.peek_kind() != &TokenKind::LeftParen {
            return Ok(WindowReference::Named(self.name()?));
        }
        Ok(WindowReference::Inline(self.window_definition()?))
    }

    fn names_base_window(&self) -> bool {
        match self.peek_kind() {
            TokenKind::QuotedIdentifier(_) => true,
            TokenKind::Word(word) => {
                !matches!(&**word, "partition" | "order" | "rows" | "range" | "groups")
                    && !self.reserved(word)
            }
            _ => false,
        }
    }

    fn window_definition(&mut self) -> SqlResult<WindowDefinition> {
        self.expect(&TokenKind::LeftParen)?;
        let base = if self.names_base_window() {
            Some(self.name()?)
        } else {
            None
        };
        let mut partition = Vec::new();
        if self.take_word("partition") {
            self.expect_word("by")?;
            loop {
                partition.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let order = if self.take_word("order") {
            self.expect_word("by")?;
            self.order_items()?
        } else {
            Vec::new()
        };
        let frame = self.frame_clause()?;
        self.expect(&TokenKind::RightParen)?;
        Ok(WindowDefinition {
            base,
            partition,
            order,
            frame,
        })
    }

    fn frame_clause(&mut self) -> SqlResult<Option<FrameClause>> {
        let position = self.position();
        let units = if self.take_word("rows") {
            FrameUnits::Rows
        } else if self.take_word("range") {
            FrameUnits::Range
        } else if self.take_word("groups") {
            FrameUnits::Groups
        } else {
            return Ok(None);
        };
        let (start, start_offset, end, end_offset) = if self.take_word("between") {
            let (start, start_offset) = self.frame_bound()?;
            self.expect_word("and")?;
            let (end, end_offset) = self.frame_bound()?;
            (start, start_offset, end, end_offset)
        } else {
            let (start, start_offset) = self.frame_bound()?;
            (start, start_offset, FrameBound::CurrentRow, None)
        };
        let exclusion = if self.take_word("exclude") {
            if self.take_word("current") {
                self.expect_word("row")?;
                FrameExclusion::CurrentRow
            } else if self.take_word("group") {
                FrameExclusion::Group
            } else if self.take_word("ties") {
                FrameExclusion::Ties
            } else {
                self.expect_word("no")?;
                self.expect_word("others")?;
                FrameExclusion::NoOthers
            }
        } else {
            FrameExclusion::NoOthers
        };
        let frame = Frame {
            units,
            start,
            end,
            exclusion,
        }
        .checked(position)?;
        Ok(Some(FrameClause {
            frame,
            start_offset,
            end_offset,
        }))
    }

    fn frame_bound(&mut self) -> SqlResult<(FrameBound, Option<Expr>)> {
        if self.take_word("unbounded") {
            if self.take_word("preceding") {
                return Ok((FrameBound::UnboundedPreceding, None));
            }
            self.expect_word("following")?;
            return Ok((FrameBound::UnboundedFollowing, None));
        }
        if self.is_word("current") && self.is_word_at(1, "row") {
            self.at += 2;
            return Ok((FrameBound::CurrentRow, None));
        }
        let offset = self.expression()?;
        if self.take_word("preceding") {
            return Ok((FrameBound::Preceding, Some(offset)));
        }
        self.expect_word("following")?;
        Ok((FrameBound::Following, Some(offset)))
    }

    pub(super) fn window_clause(
        &mut self,
    ) -> SqlResult<Vec<(String, crate::window::WindowDefinition)>> {
        let mut listed = Vec::new();
        loop {
            let name = self.name()?;
            self.expect_word("as")?;
            let definition = self.window_definition()?;
            listed.push((name, definition));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        crate::window::define_windows(listed)
    }

    pub(super) fn hypothetical_set_aggregate(
        &mut self,
        name: &Name,
        function: AggregateFunction,
    ) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let mut hypothetical = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                hypothetical.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        self.expect_word("within")?;
        self.expect_word("group")?;
        self.expect(&TokenKind::LeftParen)?;
        self.expect_word("order")?;
        self.expect_word("by")?;
        let order = self.order_items()?;
        self.expect(&TokenKind::RightParen)?;
        if hypothetical.len() != order.len() {
            return Err(if self.oracle() {
                SqlError::syntax("invalid number of arguments", name.position).with_oracle_code(909)
            } else {
                no_such_function(&name.text, hypothetical.len() + order.len(), name.position)
            });
        }
        Ok(Expr {
            kind: ExprKind::Aggregate {
                function,
                argument: Some(Box::new(crate::hypothetical_set::arguments(
                    hypothetical,
                    order,
                    name.position,
                ))),
                distinct: false,
            },
            position: name.position,
        })
    }

    pub(super) fn bivariate_aggregate(
        &mut self,
        name: &Name,
        function: AggregateFunction,
    ) -> SqlResult<Expr> {
        if self.oracle() && self.is_word_at(1, "distinct") {
            self.at += 1;
            return Err(self.distinct_refused());
        }
        self.expect(&TokenKind::LeftParen)?;
        let distinct = self.take_word("distinct");
        if !distinct {
            self.take_word("all");
        }
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
        let count = arguments.len();
        let mut arguments = arguments.into_iter();
        let (Some(dependent), Some(independent), None) =
            (arguments.next(), arguments.next(), arguments.next())
        else {
            return Err(if self.oracle() {
                SqlError::syntax("invalid number of arguments", name.position).with_oracle_code(909)
            } else {
                no_such_function(&name.text, count, name.position)
            });
        };
        Ok(Expr {
            kind: ExprKind::Aggregate {
                function,
                argument: Some(Box::new(crate::bivariate_aggregates::arguments(
                    dependent,
                    independent,
                    name.position,
                ))),
                distinct,
            },
            position: name.position,
        })
    }

    pub(super) fn stats_mode(&mut self, name: &Name) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let expr = self.expression()?;
        self.expect(&TokenKind::RightParen)?;
        let item = OrderItem {
            expr: expr.clone(),
            descending: false,
            nulls_first: None,
        };
        Ok(Expr {
            kind: ExprKind::Aggregate {
                function: AggregateFunction::Mode,
                argument: Some(Box::new(crate::aggregate::ordered_arguments(
                    expr,
                    vec![item],
                ))),
                distinct: false,
            },
            position: name.position,
        })
    }

    pub(super) fn ordered_set_aggregate(
        &mut self,
        name: Name,
        function: AggregateFunction,
    ) -> SqlResult<Expr> {
        self.expect(&TokenKind::LeftParen)?;
        let mut direct = Vec::new();
        if !self.take(&TokenKind::RightParen) {
            loop {
                direct.push(self.expression()?);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen)?;
        }
        if !(self.is_word("within") && self.is_word_at(1, "group")) {
            return Err(wrong_kind(
                format!(
                    "WITHIN GROUP is required for ordered-set aggregate {}",
                    name.text
                ),
                name.position,
            ));
        }
        self.at += 2;
        self.expect(&TokenKind::LeftParen)?;
        self.expect_word("order")?;
        self.expect_word("by")?;
        let mut order = self.order_items()?;
        self.expect(&TokenKind::RightParen)?;
        let wanted = usize::from(function != AggregateFunction::Mode);
        if direct.len() != wanted || order.len() != 1 {
            return Err(no_such_function(
                &name.text,
                direct.len() + order.len(),
                name.position,
            ));
        }
        if let Some(fraction) = direct.first()
            && matches!(&fraction.kind, ExprKind::Function { name, .. } if name == "array_constructor")
        {
            return Err(SqlError::not_supported(format!(
                "{} over an array of fractions is not supported yet",
                name.text
            ))
            .at(fraction.position));
        }
        let item = order.remove(0);
        let value = match direct.pop() {
            Some(fraction) => Expr {
                kind: ExprKind::Function {
                    name: crate::aggregate::ORDERED_SET_ARGUMENTS.to_owned(),
                    arguments: vec![item.expr.clone(), fraction],
                },
                position: name.position,
            },
            None => item.expr.clone(),
        };
        Ok(Expr {
            kind: ExprKind::Aggregate {
                function,
                argument: Some(Box::new(crate::aggregate::ordered_arguments(
                    value,
                    vec![item],
                ))),
                distinct: false,
            },
            position: name.position,
        })
    }
}
