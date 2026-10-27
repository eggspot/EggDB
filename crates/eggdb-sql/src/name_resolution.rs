use crate::ast::{
    AlterAction, AlterTable, ConstraintKind, CreateTable, Delete, DerivedTable, Expr, ExprKind,
    FromClause, Insert, InsertSource, InsertValue, JoinCondition, Name, OnDuplicate, Query, Select,
    SelectItem, SetExpr, Statement, TableReference, Update,
};
use crate::dialect::Dialect;
use crate::encoding::ViewDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::children_mut;
use crate::execute::{Relation, TableCache};
use crate::lexer::tokenize_in;
use crate::parser::{Nesting, Parameters, Parser};
use crate::schemas::{self, DEFAULT_SCHEMA, base_name, display, is_qualified};
use crate::settings::Settings;
use crate::store::KeyValueRead;
use std::sync::Arc;

const MAX_QUERY_NESTING: usize = 32;
const MAX_NESTING: usize = 64;
const MAX_QUERIES: usize = 1024;

#[derive(Debug, Default)]
pub(crate) struct Resolution {
    pub(crate) references: Vec<String>,
    pub(crate) expanded: Vec<String>,
}

pub(crate) struct Resolver<'a> {
    reader: &'a dyn KeyValueRead,
    tables: &'a mut TableCache,
    settings: Option<&'a Settings>,
    walk_expressions: bool,
    selects: usize,
    depth: usize,
    levels: usize,
    recorded_at: usize,
    stack: Vec<String>,
    paths: Vec<Vec<String>>,
    resolution: Resolution,
}

fn nested_too_deep(position: usize) -> SqlError {
    SqlError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
        .with_hint(format!(
            "The statement and the views it uses nest queries more than {MAX_QUERY_NESTING} levels deep."
        ))
        .at(position)
}

fn too_many_queries(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        format!("a statement can hold at most {MAX_QUERIES} SELECTs, counting those of its views"),
    )
    .at(position)
}

pub(crate) fn parse_view(
    view: &ViewDefinition,
    settings: Option<&Settings>,
    start: Nesting,
) -> SqlResult<(Query, usize)> {
    let damaged = || crate::encoding::corrupt("view definition");
    let tokens = tokenize_in(&view.query, view.dialect).map_err(|_| damaged())?;
    let defaults;
    let settings = match settings {
        Some(settings) => settings,
        None => {
            defaults = Settings::default();
            &defaults
        }
    };
    let mut parser = Parser::new(&view.query, &tokens)
        .with_context(Parameters::Absent, settings)
        .in_dialect(view.dialect)
        .nested_at(start);
    match parser.view_query() {
        Ok(found) => Ok(found),
        Err(error) if error.state() == SqlState::STATEMENT_TOO_COMPLEX => Err(error),
        Err(_) => Err(damaged()),
    }
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(
        reader: &'a dyn KeyValueRead,
        tables: &'a mut TableCache,
        settings: Option<&'a Settings>,
        walk_expressions: bool,
    ) -> SqlResult<Self> {
        let path = schemas::relation_path(reader)?;
        Ok(Self {
            reader,
            tables,
            settings,
            walk_expressions,
            selects: 0,
            depth: 0,
            levels: 0,
            recorded_at: 0,
            stack: Vec::new(),
            paths: vec![path],
            resolution: Resolution::default(),
        })
    }

    fn with_path(mut self, path: Vec<String>) -> SqlResult<Self> {
        let mut existing = Vec::with_capacity(path.len());
        for schema in path {
            if schemas::schema_exists(self.reader, &schema)? {
                existing.push(schema);
            }
        }
        self.paths = vec![existing];
        Ok(self)
    }

    fn resolve_name(&self, name: &str) -> SqlResult<String> {
        if is_qualified(name) || crate::temp_tables::is_temp_table(name) {
            return Ok(schemas::normalized(name).to_owned());
        }
        let path = self.paths.last().map(Vec::as_slice).unwrap_or(&[]);
        if path.len() == 1 && path[0] == DEFAULT_SCHEMA {
            return Ok(name.to_owned());
        }
        for schema in path {
            let candidate = schemas::qualified(schema, name);
            if self
                .reader
                .get(&crate::encoding::table_key(&candidate))?
                .is_some()
                || self
                    .reader
                    .get(&crate::encoding::sequence_key(&candidate))?
                    .is_some()
            {
                return Ok(candidate);
            }
        }
        if path.iter().any(|schema| schema == DEFAULT_SCHEMA) {
            Ok(name.to_owned())
        } else {
            Ok(schemas::unreachable_name(name))
        }
    }

    pub(crate) fn names(&mut self, names: &mut [Name]) -> SqlResult<()> {
        for name in names {
            name.text = self.resolve_name(&name.text)?;
        }
        Ok(())
    }

    pub(crate) fn name(&mut self, name: &mut Name) -> SqlResult<()> {
        name.text = self.resolve_name(&name.text)?;
        Ok(())
    }

    pub(crate) fn target(&mut self, reference: &mut TableReference) -> SqlResult<()> {
        let resolved = self.resolve_name(&reference.name.text)?;
        if is_qualified(&reference.name.text) && reference.alias.is_none() {
            reference.alias = Some(base_name(&resolved).to_owned());
        }
        reference.name.text = resolved;
        Ok(())
    }

    pub(crate) fn query(&mut self, query: &mut Query) -> SqlResult<()> {
        let mut ctes = Vec::new();
        self.walk_query(query, &mut ctes)
    }

    pub(crate) fn select(&mut self, select: &mut Select) -> SqlResult<()> {
        let mut ctes = Vec::new();
        self.walk_select(select, &mut ctes)
    }

    pub(crate) fn expressions<'e>(
        &mut self,
        exprs: impl IntoIterator<Item = &'e mut Expr>,
    ) -> SqlResult<()> {
        if !self.walk_expressions {
            return Ok(());
        }
        let mut ctes = Vec::new();
        for expr in exprs {
            self.walk_expr(expr, &mut ctes)?;
        }
        Ok(())
    }

    fn enter(&mut self, position: usize) -> SqlResult<()> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(nested_too_deep(position));
        }
        Ok(())
    }

    fn nested_query(
        &mut self,
        query: &mut Query,
        ctes: &mut Vec<String>,
        position: usize,
    ) -> SqlResult<()> {
        self.depth += 1;
        self.levels += 1;
        let walked = if self.depth > MAX_NESTING || self.levels > MAX_QUERY_NESTING {
            Err(nested_too_deep(position))
        } else {
            self.walk_query(query, ctes)
        };
        self.depth -= 1;
        self.levels -= 1;
        walked
    }

    fn walk_query(&mut self, query: &mut Query, ctes: &mut Vec<String>) -> SqlResult<()> {
        let base = ctes.len();
        for table in &mut query.with {
            ctes.push(table.name.text.clone());
            self.nested_query(&mut table.query, ctes, table.name.position)?;
        }
        let mut bodies: Vec<&mut SetExpr> = vec![&mut query.body];
        while let Some(body) = bodies.pop() {
            match body {
                SetExpr::Select(select) => self.walk_select(select, ctes)?,
                SetExpr::Operation(operation) => {
                    bodies.push(&mut operation.right);
                    bodies.push(&mut operation.left);
                }
                SetExpr::Query(inner) => {
                    let position = inner.position;
                    self.nested_query(inner, ctes, position)?;
                }
            }
        }
        if self.walk_expressions {
            for item in &mut query.order_by {
                self.walk_expr(&mut item.expr, ctes)?;
            }
            for expr in query.limit.iter_mut().chain(query.offset.iter_mut()) {
                self.walk_expr(expr, ctes)?;
            }
        }
        ctes.truncate(base);
        Ok(())
    }

    fn walk_select(&mut self, select: &mut Select, ctes: &mut Vec<String>) -> SqlResult<()> {
        self.selects += 1;
        if self.selects > MAX_QUERIES {
            return Err(too_many_queries(0));
        }
        if let Some(from) = &mut select.from {
            self.walk_reference(&mut from.first, ctes)?;
            for join in &mut from.joins {
                self.walk_reference(&mut join.table, ctes)?;
            }
        }
        if !self.walk_expressions {
            return Ok(());
        }
        for item in &mut select.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.walk_expr(expr, ctes)?;
            }
        }
        let mut others: Vec<&mut Expr> = select.filter.iter_mut().collect();
        others.extend(select.group_by.iter_mut());
        others.extend(select.having.iter_mut());
        others.extend(select.order_by.iter_mut().map(|item| &mut item.expr));
        others.extend(select.limit.iter_mut());
        others.extend(select.offset.iter_mut());
        if let Some(from) = &mut select.from {
            for join in &mut from.joins {
                if let JoinCondition::On(expr) = &mut join.condition {
                    others.push(expr);
                }
            }
        }
        for expr in others {
            self.walk_expr(expr, ctes)?;
        }
        Ok(())
    }

    fn walk_expr(&mut self, expr: &mut Expr, ctes: &mut Vec<String>) -> SqlResult<()> {
        self.enter(expr.position)?;
        let walked = self.walk_expr_inner(expr, ctes);
        self.depth -= 1;
        walked
    }

    fn walk_expr_inner(&mut self, expr: &mut Expr, ctes: &mut Vec<String>) -> SqlResult<()> {
        match &mut expr.kind {
            ExprKind::Subquery(subquery) => {
                if let Some(operand) = subquery.test.operand_mut() {
                    self.walk_expr(operand, ctes)?;
                }
                let position = subquery.query.position;
                self.nested_query(&mut subquery.query, ctes, position)
            }
            ExprKind::SessionValue { name, value } if *name == "current_schema" => {
                *value = schemas::current_schema(self.reader)?.unwrap_or_default();
                Ok(())
            }
            ExprKind::SessionValue { name, value } if *name == "current_schemas" => {
                *value = schemas::current_schemas(self.reader, value == "true")?;
                Ok(())
            }
            _ => {
                for child in children_mut(expr) {
                    self.walk_expr(child, ctes)?;
                }
                Ok(())
            }
        }
    }

    fn record(&mut self, name: &str) {
        if self.stack.len() == self.recorded_at
            && !self.resolution.references.iter().any(|known| known == name)
        {
            self.resolution.references.push(name.to_owned());
        }
    }

    fn walk_reference(
        &mut self,
        reference: &mut TableReference,
        ctes: &mut Vec<String>,
    ) -> SqlResult<()> {
        if let Some(derived) = &mut reference.derived {
            return self.nested_query(&mut derived.query, ctes, reference.name.position);
        }
        if ctes.contains(&reference.name.text) {
            return Ok(());
        }
        let resolved = self.resolve_name(&reference.name.text)?;
        let relation = self.tables.relation(self.reader, &resolved)?;
        if relation.is_some() {
            self.record(&resolved);
        }
        match relation {
            Some(Relation::View(view)) => self.expand(reference, view),
            _ => {
                if is_qualified(&reference.name.text) && reference.alias.is_none() {
                    reference.alias = Some(base_name(&resolved).to_owned());
                }
                reference.name.text = resolved;
                Ok(())
            }
        }
    }

    fn expand(
        &mut self,
        reference: &mut TableReference,
        view: Arc<ViewDefinition>,
    ) -> SqlResult<()> {
        let position = reference.name.position;
        if self.stack.contains(&view.name) {
            return Err(SqlError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!(
                    "infinite recursion detected in rules for relation \"{}\"",
                    display(&view.name)
                ),
            )
            .at(position));
        }
        let query_depth = self.levels + 1;
        if query_depth > MAX_QUERY_NESTING {
            return Err(nested_too_deep(position));
        }
        let (mut query, selects) = parse_view(
            &view,
            self.settings,
            Nesting {
                depth: self.depth + 1,
                query_depth,
                queries: self.selects,
            },
        )
        .map_err(|error| error.at_if_unplaced(position))?;
        if selects > MAX_QUERIES {
            return Err(too_many_queries(position));
        }
        if !self.resolution.expanded.contains(&view.name) {
            self.resolution.expanded.push(view.name.clone());
        }
        let mut path = Vec::with_capacity(view.search_path.len());
        for schema in &view.search_path {
            if schemas::schema_exists(self.reader, schema)? {
                path.push(schema.clone());
            }
        }
        self.stack.push(view.name.clone());
        self.paths.push(path);
        let walk_expressions = std::mem::replace(&mut self.walk_expressions, true);
        self.depth += 1;
        self.levels += 1;
        let mut ctes = Vec::new();
        let walked = self.walk_query(&mut query, &mut ctes);
        self.depth -= 1;
        self.levels -= 1;
        self.walk_expressions = walk_expressions;
        self.paths.pop();
        self.stack.pop();
        walked?;
        let column_names: Vec<Name> = view
            .columns
            .iter()
            .map(|column| Name {
                text: column.name.clone(),
                position,
            })
            .collect();
        let label = base_name(&view.name).to_owned();
        let derived = if view.expands_wildcard {
            DerivedTable {
                query: Query::of_select(
                    narrowed(query, column_names, &label, view.dialect, position),
                    position,
                ),
                column_names: Vec::new(),
                dialect: view.dialect,
            }
        } else {
            DerivedTable {
                query,
                column_names,
                dialect: view.dialect,
            }
        };
        if reference.alias.is_none() {
            reference.alias = Some(label);
        }
        reference.derived = Some(Box::new(derived));
        Ok(())
    }

    pub(crate) fn finish(self) -> Resolution {
        self.resolution
    }
}

fn narrowed(
    query: Query,
    column_names: Vec<Name>,
    label: &str,
    dialect: Dialect,
    position: usize,
) -> Select {
    let items = column_names
        .iter()
        .map(|name| SelectItem::Expr {
            expr: Expr {
                kind: ExprKind::Column {
                    qualifier: None,
                    name: name.text.clone(),
                },
                position,
            },
            alias: None,
        })
        .collect();
    Select {
        distinct: false,
        items,
        from: Some(FromClause {
            first: TableReference {
                name: Name {
                    text: label.to_owned(),
                    position,
                },
                alias: Some(label.to_owned()),
                derived: Some(Box::new(DerivedTable {
                    query,
                    column_names,
                    dialect,
                })),
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
    }
}

pub(crate) fn resolve_view_body(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    view: &str,
    query: &mut Query,
    path: Vec<String>,
    settings: Option<&Settings>,
) -> SqlResult<Resolution> {
    let mut resolver = Resolver::new(reader, tables, settings, true)?.with_path(path)?;
    resolver.stack.push(view.to_owned());
    resolver.recorded_at = 1;
    resolver.levels = 1;
    resolver.depth = 1;
    resolver.query(query)?;
    Ok(resolver.finish())
}

pub(crate) struct Binding<'a> {
    pub(crate) settings: &'a Settings,
    pub(crate) walk_expressions: bool,
}

impl Binding<'_> {
    fn resolver<'r>(
        &'r self,
        reader: &'r dyn KeyValueRead,
        tables: &'r mut TableCache,
    ) -> SqlResult<Resolver<'r>> {
        Resolver::new(reader, tables, Some(self.settings), self.walk_expressions)
    }

    pub(crate) fn select(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        select: &mut Select,
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?.select(select)
    }

    pub(crate) fn query(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        query: &mut Query,
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?.query(query)
    }

    pub(crate) fn names(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        names: &mut [Name],
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?.names(names)
    }

    pub(crate) fn target(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        reference: &mut TableReference,
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?.target(reference)
    }

    pub(crate) fn insert(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        insert: &mut Insert,
    ) -> SqlResult<()> {
        let mut resolver = self.resolver(reader, tables)?;
        resolver.target(&mut insert.table)?;
        let mut exprs: Vec<&mut Expr> = Vec::new();
        if let InsertSource::Values(rows) = &mut insert.source {
            for row in rows {
                for value in row {
                    if let InsertValue::Expr(expr) = value {
                        exprs.push(expr);
                    }
                }
            }
        }
        if let OnDuplicate::Update(assignments) = &mut insert.on_duplicate {
            exprs.extend(assignments.iter_mut().map(|(_, expr)| expr));
        }
        exprs.extend(returning_exprs(&mut insert.returning));
        resolver.expressions(exprs)
    }

    pub(crate) fn update(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        update: &mut Update,
    ) -> SqlResult<()> {
        let mut resolver = self.resolver(reader, tables)?;
        resolver.target(&mut update.table)?;
        let mut exprs: Vec<&mut Expr> = update
            .assignments
            .iter_mut()
            .filter_map(|(_, value)| match value {
                InsertValue::Expr(expr) => Some(expr),
                InsertValue::Default => None,
            })
            .collect();
        exprs.extend(update.filter.iter_mut());
        exprs.extend(returning_exprs(&mut update.returning));
        resolver.expressions(exprs)
    }

    pub(crate) fn delete(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        delete: &mut Delete,
    ) -> SqlResult<()> {
        let mut resolver = self.resolver(reader, tables)?;
        resolver.target(&mut delete.table)?;
        let mut exprs: Vec<&mut Expr> = delete.filter.iter_mut().collect();
        exprs.extend(returning_exprs(&mut delete.returning));
        resolver.expressions(exprs)
    }

    pub(crate) fn create_table(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        create: &mut CreateTable,
    ) -> SqlResult<()> {
        create.name.text = schemas::creation_name(reader, &create.name)?;
        let mut resolver = self.resolver(reader, tables)?;
        for spec in create
            .columns
            .iter_mut()
            .flat_map(|column| column.constraints.iter_mut())
            .chain(create.constraints.iter_mut())
        {
            if let ConstraintKind::ForeignKey(key) = &mut spec.kind {
                resolver.name(&mut key.parent)?;
            }
        }
        Ok(())
    }

    pub(crate) fn alter_table(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        alter: &mut AlterTable,
    ) -> SqlResult<()> {
        let mut resolver = self.resolver(reader, tables)?;
        resolver.name(&mut alter.name)?;
        for action in &mut alter.actions {
            if let AlterAction::AddConstraint(spec) = action
                && let ConstraintKind::ForeignKey(key) = &mut spec.kind
            {
                resolver.name(&mut key.parent)?;
            }
        }
        Ok(())
    }

    pub(crate) fn statement(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        statement: &mut Statement,
    ) -> SqlResult<()> {
        match statement {
            Statement::Select(select) => self.select(reader, tables, select),
            Statement::Query(query) => self.query(reader, tables, query),
            Statement::Insert(insert) => self.insert(reader, tables, insert),
            Statement::Update(update) => self.update(reader, tables, update),
            Statement::Delete(delete) => self.delete(reader, tables, delete),
            Statement::Explain(explain) => self.statement(reader, tables, &mut explain.statement),
            _ => Ok(()),
        }
    }
}

fn returning_exprs(returning: &mut Option<Vec<SelectItem>>) -> impl Iterator<Item = &mut Expr> {
    returning
        .iter_mut()
        .flatten()
        .filter_map(|item| match item {
            SelectItem::Expr { expr, .. } => Some(expr),
            SelectItem::Wildcard { .. } => None,
        })
}

impl Binding<'_> {
    pub(crate) fn comment(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        comment: &mut crate::ast::Comment,
    ) -> SqlResult<()> {
        use crate::ast::CommentTarget;
        let mut resolver = self.resolver(reader, tables)?;
        match &mut comment.target {
            CommentTarget::Relation { name, .. } => resolver.name(name),
            CommentTarget::Column { table, .. } | CommentTarget::Constraint { table, .. } => {
                resolver.name(table)
            }
            CommentTarget::Index(_) | CommentTarget::Schema(_) | CommentTarget::Extension(_) => {
                Ok(())
            }
        }
    }
}
