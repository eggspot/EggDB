use super::{Level, Parser};
use crate::ast::{
    DerivedTable, Expr, ExprKind, FromClause, Hierarchy, HierarchyOutput, Literal, Name, OrderItem,
    Query, Select, SelectItem, SetExpr, TableReference,
};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{children, children_mut};
use crate::hierarchy::{LEVEL_SLOT, PARENT_SLOTS};
use crate::types::SortDirection;

const LEVEL: &str = "level";
const PRIOR: &str = "prior";
const CONNECT_BY_ROOT: &str = "connect_by_root";
const CONNECT_BY_ISLEAF: &str = "connect_by_isleaf";
const CONNECT_BY_ISCYCLE: &str = "connect_by_iscycle";
const SYS_CONNECT_BY_PATH: &str = "sys_connect_by_path";
const DUAL: &str = "dual";

pub(super) struct HierarchicalClause {
    start: Option<Expr>,
    connect: Expr,
    no_cycle: bool,
    position: usize,
}

fn connect_by_required(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "CONNECT BY clause required in this query block",
    )
    .with_oracle_code(1788)
    .at(position)
}

fn path_not_allowed(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "SYS_CONNECT_BY_PATH function is not allowed here",
    )
    .with_oracle_code(30002)
    .at(position)
}

fn illegal_path_parameter(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "illegal parameter in SYS_CONNECT_BY_PATH function",
    )
    .with_oracle_code(30003)
    .at(position)
}

fn nocycle_required(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "NOCYCLE keyword is required with CONNECT_BY_ISCYCLE pseudo-column",
    )
    .with_oracle_code(30930)
    .at(position)
}

pub(super) fn siblings_not_allowed(position: usize) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        "ORDER SIBLINGS BY clause not allowed here",
    )
    .with_oracle_code(30929)
    .at(position)
}

fn not_a_select_list_position(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INVALID_COLUMN_REFERENCE,
        "ORDER BY item must be the number of a SELECT-list expression",
    )
    .with_oracle_code(1785)
    .at(position)
}

fn refused_here(what: &str, position: usize) -> SqlError {
    SqlError::not_supported(format!("{what} is not supported yet")).at(position)
}

fn hierarchical_operator(expr: &Expr) -> Option<&'static str> {
    match &expr.kind {
        ExprKind::OracleFunction { name, .. }
            if [
                LEVEL,
                PRIOR,
                CONNECT_BY_ROOT,
                CONNECT_BY_ISLEAF,
                CONNECT_BY_ISCYCLE,
            ]
            .contains(name) =>
        {
            Some(*name)
        }
        ExprKind::Function { name, .. } if name == SYS_CONNECT_BY_PATH => Some(SYS_CONNECT_BY_PATH),
        _ => None,
    }
}

fn first_hierarchical_operator(expr: &Expr) -> Option<(&'static str, usize)> {
    if let Some(name) = hierarchical_operator(expr) {
        return Some((name, expr.position));
    }
    children(expr)
        .into_iter()
        .find_map(first_hierarchical_operator)
}

fn mentions_hierarchy(expr: &Expr) -> bool {
    first_hierarchical_operator(expr).is_some()
}

fn mentions_hidden_column(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Column { name, .. } if crate::temp_tables::is_hidden_column(name))
        || children(expr).into_iter().any(mentions_hidden_column)
}

fn only_argument(expr: &Expr) -> Option<&Expr> {
    match &expr.kind {
        ExprKind::OracleFunction { arguments, .. } => match arguments.as_slice() {
            [argument] => Some(argument),
            _ => None,
        },
        _ => None,
    }
}

fn slot(index: usize, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Slot {
            index,
            sql_type: None,
        },
        position,
    }
}

fn column(name: String, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Column {
            qualifier: None,
            name,
        },
        position,
    }
}

fn outside_hierarchy(expr: &Expr) -> SqlResult<()> {
    match first_hierarchical_operator(expr) {
        Some((SYS_CONNECT_BY_PATH, position)) => Err(path_not_allowed(position)),
        Some((_, position)) => Err(connect_by_required(position)),
        None => Ok(()),
    }
}

fn plain_operand(expr: &Expr) -> SqlResult<Expr> {
    match first_hierarchical_operator(expr) {
        Some((name, position)) => Err(refused_here(
            &format!(
                "{} inside a hierarchical operator",
                name.to_ascii_uppercase()
            ),
            position,
        )),
        None => Ok(expr.clone()),
    }
}

fn select_list_expr(items: &[SelectItem], expr: Expr) -> SqlResult<Expr> {
    if let ExprKind::Literal(Literal::Integer(number)) = &expr.kind {
        return usize::try_from(*number)
            .ok()
            .and_then(|number| number.checked_sub(1))
            .and_then(|at| items.get(at))
            .and_then(|item| match item {
                SelectItem::Expr { expr, .. } => Some(expr.clone()),
                SelectItem::Wildcard { .. } => None,
            })
            .ok_or_else(|| not_a_select_list_position(expr.position));
    }
    let aliased = match &expr.kind {
        ExprKind::Column {
            qualifier: None,
            name,
        } => items.iter().find_map(|item| match item {
            SelectItem::Expr {
                expr: listed,
                alias: Some(alias),
            } if alias.eq_ignore_ascii_case(name) => Some(listed.clone()),
            _ => None,
        }),
        _ => None,
    };
    Ok(aliased.unwrap_or(expr))
}

struct HierarchyBuilder<'b> {
    inner: &'b mut Select,
    hierarchy: &'b mut Hierarchy,
}

impl HierarchyBuilder<'_> {
    fn input(&mut self, expr: Expr) -> usize {
        let at = self.hierarchy.inputs;
        self.inner
            .items
            .insert(at, SelectItem::Expr { expr, alias: None });
        self.hierarchy.inputs += 1;
        at
    }

    fn output(&mut self, output: HierarchyOutput) -> String {
        let number = match self
            .hierarchy
            .outputs
            .iter()
            .position(|known| *known == output)
        {
            Some(number) => number,
            None => {
                self.hierarchy.outputs.push(output);
                self.hierarchy.outputs.len() - 1
            }
        };
        crate::hierarchy::output_column(number)
    }

    fn connect_condition(&mut self, expr: &Expr) -> SqlResult<Expr> {
        let position = expr.position;
        match hierarchical_operator(expr) {
            Some(PRIOR) => {
                let argument =
                    only_argument(expr).ok_or_else(|| refused_here("PRIOR", position))?;
                let input = self.input(plain_operand(argument)?);
                self.hierarchy.prior_inputs.push(input);
                return Ok(slot(PARENT_SLOTS + input, position));
            }
            Some(LEVEL) => return Ok(slot(LEVEL_SLOT, position)),
            Some(other) => {
                return Err(refused_here(
                    &format!("{} in CONNECT BY", other.to_ascii_uppercase()),
                    position,
                ));
            }
            None => {}
        }
        if !mentions_hierarchy(expr) {
            if matches!(
                expr.kind,
                ExprKind::Literal(_) | ExprKind::Constant { .. } | ExprKind::SessionValue { .. }
            ) {
                return Ok(expr.clone());
            }
            let input = self.input(expr.clone());
            return Ok(slot(input, position));
        }
        if matches!(expr.kind, ExprKind::Subquery(_)) {
            return Err(refused_here(
                "a subquery beside PRIOR or LEVEL in CONNECT BY",
                position,
            ));
        }
        crate::aggregate::rebuild(expr, &mut |child| self.connect_condition(child))
    }

    fn sibling_key(&mut self, items: &[SelectItem], item: OrderItem) -> SqlResult<()> {
        let direction = SortDirection::of(&item);
        let position = item.expr.position;
        let key = select_list_expr(items, item.expr)?;
        if mentions_hidden_column(&key) || mentions_hierarchy(&key) {
            return Err(refused_here(
                "ORDER SIBLINGS BY a hierarchical pseudo-column or operator",
                position,
            ));
        }
        let input = self.input(key);
        self.hierarchy.siblings.push((input, direction));
        Ok(())
    }

    fn replaced(&mut self, expr: &Expr) -> SqlResult<Option<String>> {
        let position = expr.position;
        let name = match (&expr.kind, hierarchical_operator(expr)) {
            (_, Some(LEVEL)) => crate::hierarchy::level_column(),
            (_, Some(CONNECT_BY_ISLEAF)) => crate::hierarchy::leaf_column(),
            (_, Some(CONNECT_BY_ISCYCLE)) => {
                if !self.hierarchy.no_cycle {
                    return Err(nocycle_required(position));
                }
                crate::hierarchy::cycle_column()
            }
            (_, Some(name @ (CONNECT_BY_ROOT | PRIOR))) => {
                let argument = only_argument(expr)
                    .ok_or_else(|| refused_here(&name.to_ascii_uppercase(), position))?;
                let input = self.input(plain_operand(argument)?);
                self.output(if name == PRIOR {
                    HierarchyOutput::Prior(input)
                } else {
                    HierarchyOutput::Root(input)
                })
            }
            (ExprKind::Function { arguments, .. }, Some(SYS_CONNECT_BY_PATH)) => {
                let [value, separator] = arguments.as_slice() else {
                    return Err(illegal_path_parameter(position));
                };
                let ExprKind::Literal(Literal::String(separator)) = &separator.kind else {
                    return Err(illegal_path_parameter(separator.position));
                };
                if separator.is_empty() {
                    return Err(illegal_path_parameter(position));
                }
                let separator = separator.clone();
                let input = self.input(plain_operand(value)?);
                self.output(HierarchyOutput::Path { input, separator })
            }
            _ => return Ok(None),
        };
        Ok(Some(name))
    }

    fn replace_outer(&mut self, expr: &mut Expr) -> SqlResult<()> {
        if let Some(name) = self.replaced(expr)? {
            *expr = column(name, expr.position);
            return Ok(());
        }
        for child in children_mut(expr) {
            self.replace_outer(child)?;
        }
        Ok(())
    }

    fn replace_in_select(&mut self, select: &mut Select) -> SqlResult<()> {
        for item in &mut select.items {
            if let SelectItem::Expr { expr, alias } = item
                && mentions_hierarchy(expr)
            {
                if alias.is_none() {
                    *alias = Some(crate::eval::output_name(expr));
                }
                self.replace_outer(expr)?;
            }
        }
        if let Some(filter) = &mut select.filter {
            self.replace_outer(filter)?;
        }
        for key in &mut select.group_by {
            self.replace_outer(key)?;
        }
        if let Some(having) = &mut select.having {
            self.replace_outer(having)?;
        }
        Ok(())
    }
}

fn builder_of(select: &mut Select) -> Option<HierarchyBuilder<'_>> {
    let derived = select.from.as_mut()?.first.derived.as_deref_mut()?;
    let DerivedTable {
        query, hierarchy, ..
    } = derived;
    let hierarchy = hierarchy.as_deref_mut()?;
    let SetExpr::Select(inner) = &mut query.body else {
        return None;
    };
    Some(HierarchyBuilder {
        inner,
        hierarchy,
    })
}

fn source_tail(from: Option<&FromClause>, position: usize) -> SelectItem {
    match from {
        Some(_) => SelectItem::Wildcard {
            qualifier: None,
            position,
        },
        None => SelectItem::Expr {
            expr: Expr {
                kind: ExprKind::Literal(Literal::String("X".to_owned())),
                position,
            },
            alias: Some(format!("{}dual", crate::temp_tables::HIDDEN_COLUMN_MARKER)),
        },
    }
}

/// Rewrites an Oracle query with `START WITH ... CONNECT BY` into one that reads a derived
/// table walking the hierarchy of its `FROM` source, with the hierarchical pseudo-columns and
/// operators of its select list, `WHERE`, `GROUP BY` and `HAVING` read from that table.
pub(super) fn connect_by(mut select: Select, clause: HierarchicalClause) -> SqlResult<Select> {
    let position = clause.position;
    let (label, from) = match select.from.take() {
        None => (DUAL.to_owned(), None),
        Some(from) if from.joins.is_empty() => (
            super::joined_dml::label_of(&from.first).to_owned(),
            Some(from),
        ),
        Some(_) => return Err(refused_here("CONNECT BY over a join", position)),
    };
    let mut inner = Select {
        distinct: false,
        items: vec![source_tail(from.as_ref(), position)],
        from,
        filter: None,
        group_by: Vec::new(),
        grouping_sets: None,
        distinct_on: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    };
    let mut hierarchy = Hierarchy {
        inputs: 0,
        start: None,
        connect: clause.connect.clone(),
        prior_inputs: Vec::new(),
        no_cycle: clause.no_cycle,
        siblings: Vec::new(),
        outputs: Vec::new(),
        position,
    };
    {
        let mut builder = HierarchyBuilder {
            inner: &mut inner,
            hierarchy: &mut hierarchy,
        };
        if let Some(start) = clause.start {
            outside_hierarchy(&start)?;
            let input = builder.input(start);
            builder.hierarchy.start = Some(input);
        }
        builder.hierarchy.connect = builder.connect_condition(&clause.connect)?;
        builder.replace_in_select(&mut select)?;
    }
    select.from = Some(FromClause {
        first: TableReference {
            name: Name {
                text: label.clone(),
                position,
            },
            alias: Some(label),
            derived: Some(Box::new(DerivedTable {
                query: Query::of_select(inner, position),
                column_names: Vec::new(),
                dialect: Dialect::Oracle,
                lateral: false,
                hierarchy: Some(Box::new(hierarchy)),
            })),
            generated: None,
        },
        joins: Vec::new(),
    });
    Ok(select)
}

/// Refuses the hierarchical pseudo-columns and operators in an Oracle query without
/// `CONNECT BY`, as Oracle does.
pub(super) fn reject_outside_hierarchy(select: &Select) -> SqlResult<()> {
    for item in &select.items {
        if let SelectItem::Expr { expr, .. } = item {
            outside_hierarchy(expr)?;
        }
    }
    select
        .filter
        .iter()
        .chain(&select.group_by)
        .chain(&select.having)
        .try_for_each(outside_hierarchy)
}

/// Applies an Oracle query's `ORDER BY` or `ORDER SIBLINGS BY` once its tail is read: the
/// pseudo-columns of a hierarchical query's `ORDER BY` are read from its derived table, and
/// `ORDER SIBLINGS BY` orders the rows of each level of the hierarchy among themselves.
pub(super) fn apply_ordering(select: &mut Select, siblings: Vec<OrderItem>) -> SqlResult<()> {
    let items = select.items.clone();
    let mut order_by = std::mem::take(&mut select.order_by);
    match builder_of(select) {
        Some(mut builder) => {
            for item in &mut order_by {
                builder.replace_outer(&mut item.expr)?;
            }
            for item in siblings {
                builder.sibling_key(&items, item)?;
            }
        }
        None => {
            if let Some(item) = siblings.first() {
                return Err(siblings_not_allowed(item.expr.position));
            }
            for item in &order_by {
                outside_hierarchy(&item.expr)?;
            }
        }
    }
    select.order_by = order_by;
    Ok(())
}

impl Parser<'_> {
    pub(super) fn hierarchical_primary(
        &mut self,
        word: &str,
        position: usize,
    ) -> SqlResult<Option<Expr>> {
        let name = match word {
            "level" => LEVEL,
            "connect_by_isleaf" => CONNECT_BY_ISLEAF,
            "connect_by_iscycle" => CONNECT_BY_ISCYCLE,
            "prior" => PRIOR,
            "connect_by_root" => CONNECT_BY_ROOT,
            _ => return Ok(None),
        };
        self.at += 1;
        let arguments = if name == PRIOR || name == CONNECT_BY_ROOT {
            vec![self.expression_above(Level::Unary)?]
        } else {
            Vec::new()
        };
        Ok(Some(Expr {
            kind: ExprKind::OracleFunction { name, arguments },
            position,
        }))
    }

    pub(super) fn hierarchical_clause(&mut self) -> SqlResult<Option<HierarchicalClause>> {
        let position = self.position();
        let mut start = None;
        let mut connect = None;
        let mut no_cycle = false;
        loop {
            if start.is_none() && self.is_word("start") && self.is_word_at(1, "with") {
                self.at += 2;
                let condition = self.expression()?;
                start = Some(self.condition(condition));
            } else if connect.is_none() && self.is_word("connect") && self.is_word_at(1, "by") {
                self.at += 2;
                no_cycle = self.take_word("nocycle");
                let condition = self.expression()?;
                connect = Some(self.condition(condition));
            } else {
                break;
            }
        }
        match connect {
            Some(connect) => Ok(Some(HierarchicalClause {
                start,
                connect,
                no_cycle,
                position,
            })),
            None if start.is_some() => Err(connect_by_required(self.position())),
            None => Ok(None),
        }
    }
}
