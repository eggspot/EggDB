use crate::OutputColumn;
use crate::ast::{
    AlterAction, AlterTable, ConflictResolution, ConstraintKind, CreateTable, Delete, DerivedTable,
    Expr, ExprKind, FromClause, GeneratedTable, Insert, InsertSource, InsertValue, JoinCondition,
    JoinedTables, MergeStatement, Name, OnDuplicate, Query, Select, SelectItem, SetExpr, Statement,
    TableReference, Update,
};
use crate::column_privileges::{ColumnScopes, ScopeItem};
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
use std::collections::HashSet;
use std::sync::Arc;

const MAX_QUERY_NESTING: usize = 32;
const MAX_NESTING: usize = 64;
const MAX_QUERIES: usize = 1024;
const MAX_VIEW_TEXT_PER_STATEMENT: usize = 64 << 20;

#[derive(Debug, Default)]
pub(crate) struct Resolution {
    pub(crate) references: Vec<String>,
    pub(crate) expanded: Vec<String>,
    recorded: Option<HashSet<String>>,
    seen_expanded: Option<HashSet<String>>,
}

pub(crate) struct Resolver<'a> {
    reader: &'a dyn KeyValueRead,
    tables: &'a mut TableCache,
    settings: Option<&'a Settings>,
    walk_expressions: bool,
    selects: usize,
    view_bytes: usize,
    depth: usize,
    levels: usize,
    recorded_at: Option<usize>,
    stack: Vec<String>,
    base_path: Arc<[String]>,
    view_paths: Vec<Arc<[String]>>,
    resolution: Resolution,
    target_privilege: Option<crate::roles::Privilege>,
    columns: ColumnScopes,
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

pub(crate) fn check_view_text(text: &str, dialect: Dialect, settings: &Settings) -> SqlResult<()> {
    let tokens = tokenize_in(text, dialect)?;
    let mut parser = Parser::new(text, &tokens)
        .with_context(Parameters::Absent, settings)
        .in_dialect(dialect);
    parser.view_query().map(|_| ())
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
        let path = schemas::shared_relation_path(reader)?;
        Ok(Self {
            reader,
            tables,
            settings,
            walk_expressions,
            selects: 0,
            view_bytes: 0,
            depth: 0,
            levels: 0,
            recorded_at: None,
            stack: Vec::new(),
            base_path: path,
            view_paths: Vec::new(),
            resolution: Resolution::default(),
            target_privilege: None,
            columns: ColumnScopes::default(),
        })
    }

    fn with_path(mut self, path: Vec<String>) -> SqlResult<Self> {
        let mut existing = Vec::with_capacity(path.len());
        for schema in path {
            if schemas::schema_exists(self.reader, &schema)? {
                existing.push(schema);
            }
        }
        self.base_path = existing.into();
        Ok(self)
    }

    fn resolve_name(&self, name: &str) -> SqlResult<String> {
        Ok(self.resolved_name(name)?.unwrap_or_else(|| name.to_owned()))
    }

    fn resolved_name(&self, name: &str) -> SqlResult<Option<String>> {
        if crate::constraints::current_dialect() == crate::dialect::Dialect::Oracle
            && !crate::temp_tables::is_temp_table(name)
            && let Some(target) = crate::synonyms::resolved(self.reader, name)?
        {
            return Ok(Some(target));
        }
        if is_qualified(name) {
            let plain = schemas::normalized(name);
            return Ok((plain.len() != name.len()).then(|| plain.to_owned()));
        }
        if crate::temp_tables::is_temp_table(name) {
            return Ok(None);
        }
        if self.view_paths.is_empty() && crate::temporary::active() {
            let temporary = crate::temporary::internal_name(name);
            if self
                .reader
                .get(&crate::encoding::table_key(&temporary))?
                .is_some()
                || self
                    .reader
                    .get(&crate::encoding::sequence_key(&temporary))?
                    .is_some()
            {
                return Ok(Some(temporary));
            }
        }
        let path: &[String] = self.view_paths.last().unwrap_or(&self.base_path);
        if schemas::is_default_path(path) {
            return Ok(None);
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
                return Ok((candidate != name).then_some(candidate));
            }
        }
        if path.iter().any(|schema| schema == DEFAULT_SCHEMA) {
            Ok(None)
        } else {
            Ok(Some(schemas::unreachable_name(name)))
        }
    }

    pub(crate) fn names(&mut self, names: &mut [Name]) -> SqlResult<()> {
        for name in names {
            crate::pg_catalog::refuse_catalog_change(self.reader, name)?;
            name.text = self.resolve_name(&name.text)?;
        }
        Ok(())
    }

    pub(crate) fn name(&mut self, name: &mut Name) -> SqlResult<()> {
        name.text = self.resolve_name(&name.text)?;
        Ok(())
    }

    pub(crate) fn target(&mut self, reference: &mut TableReference) -> SqlResult<()> {
        self.resolve_target(reference)?;
        if let Some(privilege) = self.target_privilege {
            self.check_privilege(&reference.name.text, privilege)?;
            self.check_new_rows(&reference.name.text, privilege)?;
        }
        Ok(())
    }

    fn resolve_target(&mut self, reference: &mut TableReference) -> SqlResult<()> {
        if let Some(relation) =
            crate::pg_catalog::catalog_reference(self.reader, &reference.name.text)?
        {
            return Err(crate::pg_catalog::read_only(relation.name()).at(reference.name.position));
        }
        let resolved = self.resolve_name(&reference.name.text)?;
        if (is_qualified(&reference.name.text) || crate::temporary::is_temporary_name(&resolved))
            && reference.alias.is_none()
        {
            reference.alias = Some(base_name(&resolved).to_owned());
        }
        reference.name.text = resolved;
        Ok(())
    }

    fn check_written_columns(
        &self,
        table: &str,
        privilege: crate::roles::Privilege,
        written: &[String],
    ) -> SqlResult<()> {
        match self.settings {
            Some(settings) if self.stack.is_empty() => crate::roles::check_columns(
                self.reader,
                settings,
                table,
                privilege,
                written.iter().map(String::as_str),
            ),
            _ => Ok(()),
        }
    }

    fn open_target_scope(&mut self, reference: &TableReference) -> SqlResult<()> {
        self.columns.open();
        if !self.checks_columns() {
            return Ok(());
        }
        let table = reference.name.text.clone();
        let Some(relation) = self.tables.relation(self.reader, &table)? else {
            return Ok(());
        };
        let label = reference
            .alias
            .clone()
            .unwrap_or_else(|| base_name(&table).to_owned());
        let item = match self.readable_item(label.clone(), &table, &relation) {
            Ok(item) => item,
            Err(error) if error.state() == SqlState::INSUFFICIENT_PRIVILEGE => {
                let columns =
                    crate::roles::relation_columns(self.reader, &table)?.unwrap_or_default();
                ScopeItem::relation(label, table, columns.clone(), columns)
            }
            Err(error) => return Err(error),
        };
        self.columns.add(item);
        Ok(())
    }

    fn walk_in_target_scope(
        &mut self,
        reference: &TableReference,
        returning: &mut Option<Vec<SelectItem>>,
        exprs: Vec<&mut Expr>,
        read_columns: &[Name],
    ) -> SqlResult<()> {
        self.open_target_scope(reference)?;
        let walked = self.walk_target_reads(returning, exprs, read_columns);
        self.columns.close();
        walked
    }

    fn walk_target_reads(
        &mut self,
        returning: &mut Option<Vec<SelectItem>>,
        exprs: Vec<&mut Expr>,
        read_columns: &[Name],
    ) -> SqlResult<()> {
        let checks_columns = self.columns.has_unreadable();
        if checks_columns {
            for column in read_columns {
                self.columns.check_column(None, &column.text)?;
            }
            for item in returning.iter().flatten() {
                if let SelectItem::Wildcard { qualifier, .. } = item {
                    self.columns.check_wildcard(qualifier.as_deref(), true)?;
                }
            }
        }
        if !self.walk_expressions && !checks_columns {
            return Ok(());
        }
        let mut ctes = Vec::new();
        for expr in exprs.into_iter().chain(returning_exprs(returning)) {
            self.walk_expr(expr, &mut ctes)?;
        }
        Ok(())
    }

    fn walk_policy_expr(&mut self, expr: &mut Expr) -> SqlResult<()> {
        let walk_expressions = std::mem::replace(&mut self.walk_expressions, true);
        let outer_columns = self.columns.suspend();
        let mut ctes = Vec::new();
        let walked = self.walk_expr(expr, &mut ctes);
        self.columns.resume(outer_columns);
        self.walk_expressions = walk_expressions;
        walked
    }

    fn restrict_to_policies(
        &mut self,
        table: &str,
        commands: &[crate::row_security::PolicyCommand],
        filter: &mut Option<Expr>,
    ) -> SqlResult<()> {
        for command in commands {
            if let Some(mut condition) = self.row_security_filter(table, *command)? {
                self.walk_policy_expr(&mut condition)?;
                *filter = Some(crate::row_security::restricted(filter.take(), condition));
            }
        }
        Ok(())
    }

    fn add_new_row_check(
        &mut self,
        table: &str,
        command: crate::row_security::PolicyCommand,
        returning: &mut Option<Vec<SelectItem>>,
    ) -> SqlResult<()> {
        let Some(settings) = self.settings else {
            return Ok(());
        };
        if self.target_privilege.is_none() || !self.stack.is_empty() {
            return Ok(());
        }
        let Some(text) = crate::row_security::new_row_condition(
            self.reader,
            settings,
            table,
            command,
            returning.is_some(),
        )?
        else {
            return Ok(());
        };
        let mut check = crate::row_security::condition_expression(&text, settings)?;
        self.walk_policy_expr(&mut check)?;
        crate::row_security::append_new_row_check(returning, check);
        Ok(())
    }

    fn check_conflict_action(&self, table: &str, action: &OnDuplicate) -> SqlResult<()> {
        if self.target_privilege.is_none() {
            return Ok(());
        }
        let (command, form) = match action {
            OnDuplicate::Update(assignments, _) => {
                let written: Vec<String> = assignments
                    .iter()
                    .map(|(name, _)| name.text.clone())
                    .collect();
                self.check_written_columns(table, crate::roles::Privilege::Update, &written)?;
                (
                    crate::row_security::PolicyCommand::Update,
                    "INSERT ... ON CONFLICT DO UPDATE",
                )
            }
            OnDuplicate::Replace => return self.check_replacing_deletes(table, "REPLACE"),
            OnDuplicate::Fail | OnDuplicate::Ignore => return Ok(()),
        };
        self.refuse_under_row_security(table, command, form)
    }

    fn check_replacing_deletes(&self, table: &str, form: &str) -> SqlResult<()> {
        self.check_privilege(table, crate::roles::Privilege::Delete)?;
        self.refuse_under_row_security(table, crate::row_security::PolicyCommand::Delete, form)
    }

    fn refuse_under_row_security(
        &self,
        table: &str,
        command: crate::row_security::PolicyCommand,
        form: &str,
    ) -> SqlResult<()> {
        if self.row_security_filter(table, command)?.is_some() {
            return Err(SqlError::not_supported(format!(
                "{form} on a table with row-level security for this role"
            )));
        }
        Ok(())
    }

    fn check_new_rows(&self, table: &str, privilege: crate::roles::Privilege) -> SqlResult<()> {
        let command = match privilege {
            crate::roles::Privilege::Insert => crate::row_security::PolicyCommand::Insert,
            crate::roles::Privilege::Update => crate::row_security::PolicyCommand::Update,
            _ => return Ok(()),
        };
        match self.settings {
            Some(settings) if self.stack.is_empty() => {
                crate::row_security::check_new_rows(self.reader, settings, table, command)
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn row_security_filter(
        &self,
        table: &str,
        command: crate::row_security::PolicyCommand,
    ) -> SqlResult<Option<Expr>> {
        let Some(settings) = self.settings else {
            return Ok(None);
        };
        crate::row_security::visible_rows(self.reader, settings, table, command)?
            .map(|text| crate::row_security::condition_expression(&text, settings))
            .transpose()
    }

    fn row_security_view(
        &mut self,
        reference: &mut TableReference,
        table: &crate::encoding::TableDefinition,
    ) -> SqlResult<bool> {
        let Some(settings) = self.settings else {
            return Ok(false);
        };
        if self.stack.contains(&table.name) {
            return Ok(false);
        }
        let Some(text) = crate::row_security::visible_rows(
            self.reader,
            settings,
            &table.name,
            crate::row_security::PolicyCommand::Select,
        )?
        else {
            return Ok(false);
        };
        let shown: Vec<String> = display(&table.name)
            .split('.')
            .map(crate::schema::quoted)
            .collect();
        let view = crate::encoding::ViewDefinition {
            name: table.name.clone(),
            dialect: Dialect::PostgreSql,
            query: format!("SELECT * FROM {} WHERE {text}", shown.join(".")),
            columns: table
                .columns
                .iter()
                .map(|column| crate::encoding::ViewColumn {
                    name: column.name.clone(),
                    sql_type: Some(column.sql_type),
                    source: String::new(),
                })
                .collect(),
            search_path: vec!["public".to_owned()],
            references: vec![table.name.clone()],
            expands_wildcard: false,
            has_column_list: false,
            check_option: None,
            columns_narrowed: false,
        };
        self.expand(reference, Arc::new(view))?;
        Ok(true)
    }

    fn check_privilege(&self, table: &str, privilege: crate::roles::Privilege) -> SqlResult<()> {
        match self.settings {
            Some(settings) if self.stack.is_empty() => {
                crate::roles::check_table(self.reader, settings, table, privilege)
            }
            _ => Ok(()),
        }
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
        let Query {
            body,
            order_by,
            limit,
            offset,
            ..
        } = query;
        let ordered_with_body = if let SetExpr::Select(select) = &mut *body {
            self.walk_select_ordered(select, ctes, order_by)?;
            true
        } else {
            let mut bodies: Vec<&mut SetExpr> = vec![body];
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
            false
        };
        if self.walk_expressions || self.columns.has_unreadable() {
            if !ordered_with_body {
                for item in order_by.iter_mut() {
                    self.walk_expr(&mut item.expr, ctes)?;
                }
            }
            for expr in limit.iter_mut().chain(offset.iter_mut()) {
                self.walk_expr(expr, ctes)?;
            }
        }
        ctes.truncate(base);
        Ok(())
    }

    fn walk_select(&mut self, select: &mut Select, ctes: &mut Vec<String>) -> SqlResult<()> {
        self.walk_select_ordered(select, ctes, &mut [])
    }

    fn walk_select_ordered(
        &mut self,
        select: &mut Select,
        ctes: &mut Vec<String>,
        query_order: &mut [crate::ast::OrderItem],
    ) -> SqlResult<()> {
        self.columns.open();
        let walked = self.walk_select_scoped(select, ctes, query_order);
        self.columns.close();
        walked
    }

    fn walk_select_scoped(
        &mut self,
        select: &mut Select,
        ctes: &mut Vec<String>,
        query_order: &mut [crate::ast::OrderItem],
    ) -> SqlResult<()> {
        self.selects += 1;
        if self.selects > MAX_QUERIES {
            return Err(too_many_queries(0));
        }
        {
            let Select { from, filter, .. } = &mut *select;
            if let Some(from) = from {
                let filter = filter.as_ref();
                self.walk_reference(&mut from.first, ctes, filter)?;
                for join in &mut from.joins {
                    self.walk_reference(&mut join.table, ctes, filter)?;
                }
            }
        }
        let checks_columns = self.columns.has_unreadable();
        if !self.walk_expressions && !checks_columns {
            return Ok(());
        }
        if checks_columns {
            self.check_select_columns(select)?;
        }
        for item in &mut select.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.walk_expr(expr, ctes)?;
            }
        }
        for item in query_order.iter_mut() {
            self.walk_expr(&mut item.expr, ctes)?;
        }
        let mut others: Vec<&mut Expr> = select.filter.iter_mut().collect();
        others.extend(select.group_by.iter_mut());
        others.extend(select.having.iter_mut());
        others.extend(select.distinct_on.iter_mut());
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

    fn check_select_columns(&self, select: &Select) -> SqlResult<()> {
        for item in &select.items {
            if let SelectItem::Wildcard { qualifier, .. } = item {
                self.columns.check_wildcard(qualifier.as_deref(), true)?;
            }
        }
        if let Some(from) = &select.from {
            for (at, join) in from.joins.iter().enumerate() {
                match &join.condition {
                    JoinCondition::Using(columns) => self.columns.check_using(columns)?,
                    JoinCondition::Natural => self.columns.check_natural(at + 1)?,
                    JoinCondition::On(_) | JoinCondition::Always => {}
                }
            }
        }
        Ok(())
    }

    fn walk_expr_inner(&mut self, expr: &mut Expr, ctes: &mut Vec<String>) -> SqlResult<()> {
        match &mut expr.kind {
            ExprKind::Column { qualifier, name } => {
                self.columns.check_column(qualifier.as_deref(), name)
            }
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
        if self.recorded_at == Some(self.stack.len())
            && self
                .resolution
                .recorded
                .get_or_insert_with(HashSet::new)
                .insert(name.to_owned())
        {
            self.resolution.references.push(name.to_owned());
        }
    }

    fn walk_reference(
        &mut self,
        reference: &mut TableReference,
        ctes: &mut Vec<String>,
        filter: Option<&Expr>,
    ) -> SqlResult<()> {
        let item = self.walk_reference_item(reference, ctes, filter)?;
        if self.checks_columns() {
            self.columns.add(item);
        }
        Ok(())
    }

    fn checks_columns(&self) -> bool {
        self.stack.is_empty()
            && self
                .settings
                .is_some_and(|settings| settings.checked_role.is_some())
    }

    fn opaque_item(&self, reference: &TableReference) -> ScopeItem {
        if !self.checks_columns() {
            return ScopeItem::opaque(None);
        }
        ScopeItem::opaque(
            reference
                .alias
                .clone()
                .or_else(|| Some(base_name(&reference.name.text).to_owned())),
        )
    }

    fn readable_item(
        &self,
        label: String,
        table: &str,
        relation: &Relation,
    ) -> SqlResult<ScopeItem> {
        let columns: Vec<String> = match relation {
            Relation::Table(definition) => definition
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
            Relation::View(view) => view
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect(),
        };
        let unreadable = match self.settings {
            Some(settings) if self.stack.is_empty() => {
                let reach = crate::roles::column_reach(
                    self.reader,
                    settings,
                    table,
                    crate::roles::Privilege::Select,
                )?;
                crate::column_privileges::unreadable_columns(&columns, &reach)
            }
            _ => Vec::new(),
        };
        Ok(ScopeItem::relation(
            label,
            table.to_owned(),
            columns,
            unreadable,
        ))
    }

    fn walk_reference_item(
        &mut self,
        reference: &mut TableReference,
        ctes: &mut Vec<String>,
        filter: Option<&Expr>,
    ) -> SqlResult<ScopeItem> {
        if let Some(derived) = &mut reference.derived {
            self.nested_query(&mut derived.query, ctes, reference.name.position)?;
            return Ok(self.opaque_item(reference));
        }
        if let Some(generated) = &mut reference.generated {
            self.populate_row_type(generated)?;
            if self.walk_expressions || self.columns.has_unreadable() {
                for expr in crate::set_returning::expressions_mut(generated) {
                    self.walk_expr(expr, ctes)?;
                }
            }
            return Ok(self.opaque_item(reference));
        }
        if reference.system_time.is_some()
            && (ctes.contains(&reference.name.text)
                || crate::pg_catalog::catalog_reference(self.reader, &reference.name.text)?.is_some())
        {
            return Err(not_system_versioned(&reference.name));
        }
        if ctes.contains(&reference.name.text) {
            refuse_partition_selection(reference)?;
            return Ok(self.opaque_item(reference));
        }
        if let Some(relation) =
            crate::pg_catalog::catalog_reference(self.reader, &reference.name.text)?
        {
            refuse_partition_selection(reference)?;
            if reference.alias.is_none() {
                reference.alias = Some(relation.name().to_owned());
            }
            reference.generated = Some(Box::new(GeneratedTable::Catalog(relation)));
            return Ok(self.opaque_item(reference));
        }
        let written_qualified = is_qualified(&reference.name.text);
        if let Some(resolved) = self.resolved_name(&reference.name.text)? {
            reference.name.text = resolved;
        }
        if (written_qualified || crate::temporary::is_temporary_name(&reference.name.text))
            && reference.alias.is_none()
        {
            reference.alias = Some(base_name(&reference.name.text).to_owned());
        }
        let relation = self.tables.relation(self.reader, &reference.name.text)?;
        let item = match &relation {
            Some(found) if self.checks_columns() => {
                let label = reference
                    .alias
                    .clone()
                    .unwrap_or_else(|| base_name(&reference.name.text).to_owned());
                self.readable_item(label, &reference.name.text, found)?
            }
            _ => self.opaque_item(reference),
        };
        if let Some(Relation::Table(table)) = &relation
            && self.row_security_view(reference, table)?
        {
            if reference.partition_selection.is_some() {
                return Err(SqlError::not_supported(
                    "PARTITION () on a table whose row-level security policies apply",
                )
                .at(reference.name.position));
            }
            return Ok(item);
        }
        if relation.is_some() && self.recorded_at.is_some() {
            let name = reference.name.text.clone();
            self.record(&name);
        }
        if relation.is_some()
            && let Some(clause) = reference.system_time.take()
        {
            let Some(Relation::Table(table)) = &relation else {
                return Err(not_system_versioned(&reference.name));
            };
            self.expand_system_time(reference, table, *clause)?;
            return Ok(item);
        }
        match relation {
            Some(Relation::View(view)) => {
                if reference.partition_selection.is_some() {
                    return Err(crate::partition_selection::not_partitioned(&view.name)
                        .at(reference.name.position));
                }
                self.expand(reference, view)?
            }
            Some(Relation::Table(table)) => self.expand_partitioned(reference, &table, filter)?,
            None => self.sequence_reference(reference)?,
        }
        Ok(item)
    }

    fn versions_view_of(&mut self, table: &str) -> SqlResult<Option<(String, String, String)>> {
        let registry_name = schemas::qualified(
            crate::temporal_tables::REGISTRY_SCHEMA,
            crate::temporal_tables::REGISTRY_TABLE,
        );
        let Some(registry) = self.tables.table(self.reader, &registry_name)? else {
            return Ok(None);
        };
        let schema = schemas::schema_of(table);
        let schema = if schema.is_empty() { DEFAULT_SCHEMA } else { schema };
        let name = base_name(table);
        for (_, row) in crate::execute::every_row(self.reader, &registry)? {
            let text = |at: usize| row.get(at).and_then(crate::types::Value::character_text);
            if text(0) == Some(schema) && text(1) == Some(name) {
                let start = text(6).unwrap_or(crate::temporal_tables::SYSTEM_START);
                let end = text(7).unwrap_or(crate::temporal_tables::SYSTEM_END);
                return Ok(text(4).map(|view| {
                    (
                        schemas::qualified(schema, view),
                        start.to_owned(),
                        end.to_owned(),
                    )
                }));
            }
        }
        Ok(None)
    }

    fn expand_system_time(
        &mut self,
        reference: &mut TableReference,
        table: &crate::encoding::TableDefinition,
        clause: crate::ast::SystemTime,
    ) -> SqlResult<()> {
        use crate::ast::{BinaryOperator, SystemTime};
        let position = reference.name.position;
        let Some((view, start_column, end_column)) = self.versions_view_of(&table.name)? else {
            return Err(not_system_versioned(&reference.name));
        };
        let column = |name: &str| Expr {
            kind: ExprKind::Column {
                qualifier: None,
                name: name.to_owned(),
            },
            position,
        };
        let compare = |left: Expr, operator: BinaryOperator, right: Expr| Expr {
            kind: ExprKind::Binary(operator, Box::new(left), Box::new(right)),
            position,
        };
        let start = || column(&start_column);
        let end = || column(&end_column);
        let conditions = match clause {
            SystemTime::All => Vec::new(),
            SystemTime::AsOf(at) => vec![
                compare(start(), BinaryOperator::LessOrEqual, at.clone()),
                compare(end(), BinaryOperator::Greater, at),
            ],
            SystemTime::FromTo(from, to) => vec![
                compare(start(), BinaryOperator::Less, to),
                compare(end(), BinaryOperator::Greater, from),
            ],
            SystemTime::Between(from, to) => vec![
                compare(start(), BinaryOperator::LessOrEqual, to),
                compare(end(), BinaryOperator::Greater, from),
            ],
            SystemTime::ContainedIn(from, to) => vec![
                compare(start(), BinaryOperator::GreaterOrEqual, from),
                compare(end(), BinaryOperator::LessOrEqual, to),
            ],
        };
        let filter = match conditions.len() {
            0 => None,
            _ => Some(Expr {
                kind: ExprKind::And(conditions),
                position,
            }),
        };
        let label = reference
            .alias
            .clone()
            .unwrap_or_else(|| base_name(&reference.name.text).to_owned());
        let items: Vec<SelectItem> = table
            .columns
            .iter()
            .map(|found| SelectItem::Expr {
                expr: column(&found.name),
                alias: None,
            })
            .collect();
        let mut query = Query {
            with: Vec::new(),
            recursive: false,
            body: crate::ast::SetExpr::Select(Box::new(Select {
                distinct: false,
                items,
                from: Some(FromClause {
                    first: TableReference::named(
                        Name {
                            text: view,
                            position,
                        },
                        None,
                    ),
                    joins: Vec::new(),
                }),
                filter,
                group_by: Vec::new(),
                having: None,
                order_by: Vec::new(),
                limit: None,
                offset: None,
                source_row_limit: None,
                grouping_sets: None,
                distinct_on: Vec::new(),
            })),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            position,
        };
        let walk_expressions = std::mem::replace(&mut self.walk_expressions, true);
        let walked = self.walk_query(&mut query, &mut Vec::new());
        self.walk_expressions = walk_expressions;
        walked?;
        reference.alias = Some(label);
        reference.derived = Some(Box::new(DerivedTable {
            query,
            column_names: table
                .columns
                .iter()
                .map(|found| Name {
                    text: found.name.clone(),
                    position,
                })
                .collect(),
            dialect: crate::constraints::current_dialect(),
            lateral: false,
            hierarchy: None,
        }));
        Ok(())
    }

    fn populate_row_type(&mut self, generated: &mut GeneratedTable) -> SqlResult<()> {
        let Some(row_type) = crate::sql_json::populate_row_type(generated) else {
            return Ok(());
        };
        let columns = match row_type {
            crate::sql_json::RowType::Columns(columns) => columns,
            crate::sql_json::RowType::Table(name) => {
                let resolved = self.resolve_name(&name.text)?;
                match self.tables.relation(self.reader, &resolved)? {
                    Some(Relation::Table(table)) => table
                        .columns
                        .iter()
                        .map(|column| (column.name.clone(), Some(column.sql_type)))
                        .collect(),
                    Some(Relation::View(view)) => view
                        .columns
                        .iter()
                        .map(|column| (column.name.clone(), column.sql_type))
                        .collect(),
                    None => return Err(crate::sql_json::undefined_row_type(&name)),
                }
            }
        };
        crate::sql_json::fill_row_type(generated, columns)
    }

    fn sequence_reference(&mut self, reference: &mut TableReference) -> SqlResult<()> {
        if crate::constraints::current_dialect() != Dialect::PostgreSql
            || !crate::sequences::readable_as_relation(self.reader, &reference.name.text)?
        {
            return Ok(());
        }
        self.check_privilege(&reference.name.text, crate::roles::Privilege::Select)?;
        if reference.alias.is_none() {
            reference.alias = Some(base_name(&reference.name.text).to_owned());
        }
        reference.generated = Some(Box::new(GeneratedTable::Sequence(
            reference.name.text.clone(),
        )));
        Ok(())
    }

    fn expand_partitioned(
        &mut self,
        reference: &mut TableReference,
        table: &crate::encoding::TableDefinition,
        filter: Option<&Expr>,
    ) -> SqlResult<()> {
        let selection = reference.partition_selection.take();
        let position = reference.name.position;
        let catalog = crate::partitions::catalog(self.reader, self.tables)?;
        if catalog.is_empty() || catalog.key_of(&table.name).is_none() {
            return match selection {
                Some(_) => {
                    Err(crate::partition_selection::not_partitioned(&table.name).at(position))
                }
                None => Ok(()),
            };
        }
        let label = reference
            .alias
            .clone()
            .unwrap_or_else(|| base_name(&reference.name.text).to_owned());
        let qualifiers = [label.as_str(), base_name(&table.name)];
        let mut leaves = Vec::new();
        self.surviving_leaves(&catalog, table, filter, &qualifiers, 0, &mut leaves)?;
        if let Some(selection) = &selection {
            let selected =
                crate::partition_selection::SelectedPartitions::of(&catalog, table, selection)
                    .map_err(|error| error.at_if_unplaced(position))?;
            leaves = selected.narrowed(&catalog, leaves);
        }
        if leaves.is_empty() {
            return Ok(());
        }
        reference.alias = Some(label);
        if let [only] = leaves.as_slice() {
            reference.name.text = only.clone();
            return Ok(());
        }
        let items: Vec<SelectItem> = table
            .columns
            .iter()
            .map(|column| SelectItem::Expr {
                expr: Expr {
                    kind: crate::ast::ExprKind::Column {
                        qualifier: None,
                        name: column.name.clone(),
                    },
                    position,
                },
                alias: None,
            })
            .collect();
        let dialect = crate::constraints::current_dialect();
        let branch = |leaf: &String| {
            crate::ast::SetExpr::Select(Box::new(Select {
                distinct: false,
                items: items.clone(),
                from: Some(crate::ast::FromClause {
                    first: TableReference::named(
                        Name {
                            text: leaf.clone(),
                            position,
                        },
                        None,
                    ),
                    joins: Vec::new(),
                }),
                filter: None,
                group_by: Vec::new(),
                having: None,
                order_by: Vec::new(),
                limit: None,
                offset: None,
                source_row_limit: None,
                grouping_sets: None,
                distinct_on: Vec::new(),
            }))
        };
        let mut body = branch(&leaves[0]);
        for leaf in &leaves[1..] {
            body = crate::ast::SetExpr::Operation(Box::new(crate::ast::SetOperation {
                operator: crate::ast::SetOperator::Union,
                all: true,
                left: body,
                right: branch(leaf),
                dialect,
                position,
            }));
        }
        let mut query = Query {
            with: Vec::new(),
            recursive: false,
            body,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            position,
        };
        let walk_expressions = std::mem::replace(&mut self.walk_expressions, true);
        let walked = self.walk_query(&mut query, &mut Vec::new());
        self.walk_expressions = walk_expressions;
        walked?;
        reference.derived = Some(Box::new(DerivedTable {
            query,
            column_names: table
                .columns
                .iter()
                .map(|column| Name {
                    text: column.name.clone(),
                    position,
                })
                .collect(),
            dialect,
            lateral: false,
            hierarchy: None,
        }));
        Ok(())
    }

    fn surviving_leaves(
        &mut self,
        catalog: &crate::partitions::PartitionCatalog,
        table: &crate::encoding::TableDefinition,
        filter: Option<&Expr>,
        qualifiers: &[&str],
        depth: usize,
        leaves: &mut Vec<String>,
    ) -> SqlResult<()> {
        if depth > MAX_QUERY_NESTING {
            return Err(crate::encoding::corrupt("partition tree"));
        }
        let Some(partitioned) = crate::partitions::Partitioned::of(catalog, table, None)? else {
            leaves.push(table.name.clone());
            return Ok(());
        };
        let children: Vec<String> = partitioned
            .surviving(filter, qualifiers)
            .into_iter()
            .map(str::to_owned)
            .collect();
        for child in children {
            let Some(child_table) = self.tables.table(self.reader, &child)? else {
                return Err(crate::encoding::corrupt("partition tree"));
            };
            self.surviving_leaves(catalog, &child_table, filter, qualifiers, depth + 1, leaves)?;
        }
        Ok(())
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
        self.view_bytes = self.view_bytes.saturating_add(view.query.len());
        if self.view_bytes > MAX_VIEW_TEXT_PER_STATEMENT {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "a statement can expand at most {MAX_VIEW_TEXT_PER_STATEMENT} bytes of view text"
                ),
            )
            .with_hint("Use the views fewer times in one statement, or make them smaller.")
            .at(position));
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
        if self
            .resolution
            .seen_expanded
            .get_or_insert_with(HashSet::new)
            .insert(view.name.clone())
        {
            self.resolution.expanded.push(view.name.clone());
        }
        let mut path = Vec::with_capacity(view.search_path.len());
        for schema in &view.search_path {
            if schemas::schema_exists(self.reader, schema)? {
                path.push(schema.clone());
            }
        }
        self.stack.push(view.name.clone());
        self.view_paths.push(path.into());
        let walk_expressions = std::mem::replace(&mut self.walk_expressions, true);
        let outer_columns = self.columns.suspend();
        self.depth += 1;
        self.levels += 1;
        let mut ctes = Vec::new();
        let walked = self.walk_query(&mut query, &mut ctes);
        self.depth -= 1;
        self.levels -= 1;
        self.columns.resume(outer_columns);
        self.walk_expressions = walk_expressions;
        self.view_paths.pop();
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
            self.wildcard_view(query, &view, column_names, &label, position)?
        } else {
            DerivedTable {
                query,
                column_names,
                dialect: view.dialect,
                lateral: false,
                hierarchy: None,
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

impl Resolver<'_> {
    fn wildcard_view(
        &mut self,
        query: Query,
        view: &ViewDefinition,
        column_names: Vec<Name>,
        label: &str,
        position: usize,
    ) -> SqlResult<DerivedTable> {
        match view.dialect {
            Dialect::PostgreSql => Ok(DerivedTable {
                query: Query::of_select(
                    narrowed(query, column_names, label, view.dialect, position),
                    position,
                ),
                column_names: Vec::new(),
                dialect: view.dialect,
                lateral: false,
                hierarchy: None,
            }),
            Dialect::Sqlite if view.has_column_list => {
                let present = self.output_columns(&query)?;
                if present.len() != column_names.len() {
                    return Err(SqlError::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        format!(
                            "expected {} columns for '{label}' but got {}",
                            column_names.len(),
                            present.len()
                        ),
                    )
                    .at(position));
                }
                Ok(DerivedTable {
                    query,
                    column_names,
                    dialect: view.dialect,
                    lateral: false,
                    hierarchy: None,
                })
            }
            Dialect::Sqlite => Ok(DerivedTable {
                query,
                column_names: Vec::new(),
                dialect: view.dialect,
                lateral: false,
                hierarchy: None,
            }),
            Dialect::MySql | Dialect::Oracle | Dialect::SqlServer => {
                let present = self.output_columns(&query)?;
                let sources = source_positions(view, &present)
                    .ok_or_else(|| invalid_view(view, self.settings))?;
                Ok(DerivedTable {
                    query: Query::of_select(
                        by_source_position(query, view, &present, &sources, label, position),
                        position,
                    ),
                    column_names: Vec::new(),
                    dialect: view.dialect,
                    lateral: false,
                    hierarchy: None,
                })
            }
        }
    }

    fn output_columns(&mut self, query: &Query) -> SqlResult<Vec<OutputColumn>> {
        let described = crate::execute::query(self.reader, self.tables, query, true)?;
        Ok(described.columns.unwrap_or_default())
    }
}

fn same_column_name(dialect: Dialect, left: &str, right: &str) -> bool {
    match dialect {
        Dialect::MySql => left
            .chars()
            .flat_map(char::to_lowercase)
            .eq(right.chars().flat_map(char::to_lowercase)),
        _ => left == right,
    }
}

fn source_positions(view: &ViewDefinition, present: &[OutputColumn]) -> Option<Vec<usize>> {
    let unchanged = present.len() == view.columns.len()
        && present
            .iter()
            .zip(&view.columns)
            .all(|(found, column)| found.name == column.source);
    if unchanged {
        return Some((0..present.len()).collect());
    }
    let mut claimed = vec![false; present.len()];
    let mut positions = Vec::with_capacity(view.columns.len());
    for column in &view.columns {
        let unclaimed = |matches: &dyn Fn(&str) -> bool| {
            present
                .iter()
                .enumerate()
                .position(|(index, found)| !claimed[index] && matches(&found.name))
        };
        let position = unclaimed(&|name| name == column.source)
            .or_else(|| unclaimed(&|name| same_column_name(view.dialect, name, &column.source)))?;
        claimed[position] = true;
        positions.push(position);
    }
    Some(positions)
}

fn invalid_view(view: &ViewDefinition, settings: Option<&Settings>) -> SqlError {
    let name = base_name(&view.name);
    match view.dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "View '{}.{name}' references invalid table(s) or column(s) or function(s) or definer/invoker of view lack rights to use them",
                settings.map_or_else(|| "eggdb".to_owned(), |settings| settings.database.clone())
            ),
        )
        .with_vendor_code(1356),
        Dialect::SqlServer => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!(
                "Could not use view or function '{}' because of binding errors.",
                crate::schemas::display(&view.name)
            ),
        )
        .with_sqlserver_code(4413),
        _ => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!(
                "view \"{}\" has errors",
                crate::lexer::swap_uniform_case(name)
            ),
        )
        .with_oracle_code(4063),
    }
}

fn by_source_position(
    query: Query,
    view: &ViewDefinition,
    present: &[OutputColumn],
    sources: &[usize],
    label: &str,
    position: usize,
) -> Select {
    let mut renamed: Vec<Name> = present
        .iter()
        .enumerate()
        .map(|(index, column)| Name {
            text: format!(
                "{}{}{index}",
                column.name,
                crate::temp_tables::TEMP_TABLE_MARKER
            ),
            position,
        })
        .collect();
    for (column, source) in view.columns.iter().zip(sources) {
        if let Some(name) = renamed.get_mut(*source) {
            name.text.clone_from(&column.name);
        }
    }
    let items = column_items(
        view.columns.iter().map(|column| column.name.as_str()),
        position,
    );
    select_from(items, query, renamed, label, view.dialect, position)
}

fn not_system_versioned(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!(
            "table \"{}\" is not system-versioned",
            display(&name.text)
        ),
    )
    .with_hint("Make it system-versioned with eggdb_temporal.enable(schema, table, key_columns).")
    .at(name.position)
}

fn refuse_partition_selection(reference: &TableReference) -> SqlResult<()> {
    match reference.partition_selection {
        Some(_) => Err(
            crate::partition_selection::not_partitioned(&reference.name.text)
                .at(reference.name.position),
        ),
        None => Ok(()),
    }
}

fn column_items<'n>(names: impl Iterator<Item = &'n str>, position: usize) -> Vec<SelectItem> {
    names
        .map(|name| SelectItem::Expr {
            expr: Expr {
                kind: ExprKind::Column {
                    qualifier: None,
                    name: name.to_owned(),
                },
                position,
            },
            alias: None,
        })
        .collect()
}

fn select_from(
    items: Vec<SelectItem>,
    query: Query,
    column_names: Vec<Name>,
    label: &str,
    dialect: Dialect,
    position: usize,
) -> Select {
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
                    lateral: false,
                    hierarchy: None,
                })),
                generated: None,
                partition_selection: None,
                system_time: None,
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
        grouping_sets: None,
        distinct_on: Vec::new(),
    }
}

fn narrowed(
    query: Query,
    column_names: Vec<Name>,
    label: &str,
    dialect: Dialect,
    position: usize,
) -> Select {
    let items = column_items(column_names.iter().map(|name| name.text.as_str()), position);
    select_from(items, query, column_names, label, dialect, position)
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
    resolver.recorded_at = Some(1);
    resolver.levels = 1;
    resolver.depth = 1;
    resolver.query(query)?;
    Ok(resolver.finish())
}

fn merge_privileges(merge: &MergeStatement) -> Vec<crate::roles::Privilege> {
    let mut privileges = Vec::new();
    for update in &merge.matched {
        if !update.assignments.is_empty() && !privileges.contains(&crate::roles::Privilege::Update)
        {
            privileges.push(crate::roles::Privilege::Update);
        }
        if update.delete_condition.is_some()
            && !privileges.contains(&crate::roles::Privilege::Delete)
        {
            privileges.push(crate::roles::Privilege::Delete);
        }
    }
    if merge.insert.is_some() {
        privileges.push(crate::roles::Privilege::Insert);
    }
    for clause in &merge.by_source {
        let privilege = if clause.delete_condition.is_some() {
            crate::roles::Privilege::Delete
        } else {
            crate::roles::Privilege::Update
        };
        if !privileges.contains(&privilege) {
            privileges.push(privilege);
        }
    }
    privileges
}

pub(crate) struct Binding<'a> {
    pub(crate) settings: &'a Settings,
    pub(crate) walk_expressions: bool,
    pub(crate) target_privilege: Option<crate::roles::Privilege>,
}

impl Binding<'_> {
    fn resolver<'r>(
        &'r self,
        reader: &'r dyn KeyValueRead,
        tables: &'r mut TableCache,
    ) -> SqlResult<Resolver<'r>> {
        crate::pg_catalog::load_if_wanted(reader)?;
        crate::text_search_catalog::load_if_wanted(reader)?;
        let mut resolver =
            Resolver::new(reader, tables, Some(self.settings), self.walk_expressions)?;
        resolver.target_privilege = self.target_privilege;
        Ok(resolver)
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

    pub(crate) fn condition(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        condition: &mut Expr,
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?
            .expressions(std::iter::once(condition))
    }

    pub(crate) fn insert(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        insert: &mut Insert,
    ) -> SqlResult<()> {
        if let InsertSource::Query(query) = &mut insert.source {
            self.query(reader, tables, query)?;
        }
        let mut resolver = self.resolver(reader, tables)?;
        resolver.resolve_target(&mut insert.table)?;
        let table = insert.table.name.text.clone();
        if resolver.target_privilege.is_some() {
            let written = inserted_columns(reader, &table, insert)?;
            resolver.check_written_columns(&table, crate::roles::Privilege::Insert, &written)?;
            resolver.check_conflict_action(&table, &insert.on_duplicate)?;
        }
        if self.walk_expressions {
            let mut values: Vec<&mut Expr> = Vec::new();
            if let InsertSource::Values(rows) = &mut insert.source {
                for row in rows {
                    for value in row {
                        if let InsertValue::Expr(expr) = value {
                            values.push(expr);
                        }
                    }
                }
            }
            resolver.expressions(values)?;
        }
        let mut reads: Vec<&mut Expr> = Vec::new();
        if let OnDuplicate::Update(assignments, filter) = &mut insert.on_duplicate {
            reads.extend(assignments.iter_mut().map(|(_, expr)| expr));
            reads.extend(filter.as_mut());
        }
        let conflict_columns: Vec<Name> = insert
            .conflict_target
            .as_ref()
            .map(|target| target.columns().to_vec())
            .unwrap_or_default();
        if let Some(target) = &mut insert.conflict_target {
            reads.extend(target.expressions_mut());
        }
        resolver.walk_in_target_scope(
            &insert.table,
            &mut insert.returning,
            reads,
            &conflict_columns,
        )?;
        resolver.add_new_row_check(
            &table,
            crate::row_security::PolicyCommand::Insert,
            &mut insert.returning,
        )
    }

    pub(crate) fn update(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        update: &mut Update,
    ) -> SqlResult<()> {
        if update.from.is_some() {
            return self.joined_update(reader, tables, update);
        }
        let mut resolver = self.resolver(reader, tables)?;
        resolver.resolve_target(&mut update.table)?;
        let table = update.table.name.text.clone();
        if resolver.target_privilege.is_some() {
            let written: Vec<String> = update
                .assignments
                .iter()
                .map(|(name, _)| name.text.clone())
                .collect();
            resolver.check_written_columns(&table, crate::roles::Privilege::Update, &written)?;
            if update.conflict_resolution == ConflictResolution::Replace {
                resolver.check_replacing_deletes(&table, "UPDATE OR REPLACE")?;
            }
        }
        let reads_rows = update_reads_rows(update);
        let mut exprs: Vec<&mut Expr> = update
            .assignments
            .iter_mut()
            .filter_map(|(_, value)| match value {
                InsertValue::Expr(expr) => Some(expr),
                InsertValue::Default => None,
            })
            .collect();
        exprs.extend(update.filter.iter_mut());
        resolver.walk_in_target_scope(&update.table, &mut update.returning, exprs, &[])?;
        let commands: &[crate::row_security::PolicyCommand] = if reads_rows {
            &[
                crate::row_security::PolicyCommand::Update,
                crate::row_security::PolicyCommand::Select,
            ]
        } else {
            &[crate::row_security::PolicyCommand::Update]
        };
        resolver.restrict_to_policies(&table, commands, &mut update.filter)?;
        resolver.add_new_row_check(
            &table,
            crate::row_security::PolicyCommand::Update,
            &mut update.returning,
        )
    }

    pub(crate) fn merge(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        merge: &mut MergeStatement,
    ) -> SqlResult<()> {
        self.select(reader, tables, &mut merge.source)?;
        let mut resolver = self.resolver(reader, tables)?;
        let checked = resolver.target_privilege.take();
        resolver.target(&mut merge.target)?;
        if checked.is_some() {
            for privilege in merge_privileges(merge) {
                resolver.check_privilege(&merge.target.name.text, privilege)?;
                resolver.check_new_rows(&merge.target.name.text, privilege)?;
            }
        }
        resolver.target_privilege = checked;
        if checked.is_some() {
            resolver.open_target_scope(&merge.target)?;
            let hides_columns = resolver.columns.has_unreadable();
            resolver.columns.close();
            if hides_columns {
                return Err(crate::roles::permission_denied(&merge.target.name.text)
                    .with_hint("MERGE reads the target's columns: it needs SELECT on the whole target table."));
            }
        }
        if resolver
            .row_security_filter(
                &merge.target.name.text,
                crate::row_security::PolicyCommand::Update,
            )?
            .is_some()
        {
            return Err(SqlError::not_supported(
                "MERGE into a table with row-level security for this role",
            ));
        }
        if !self.walk_expressions {
            return Ok(());
        }
        let mut exprs: Vec<&mut Expr> = vec![&mut merge.on];
        for update in &mut merge.matched {
            exprs.extend(update.assignments.iter_mut().map(|(_, expr)| expr));
            exprs.extend(update.condition.iter_mut());
            exprs.extend(update.delete_condition.iter_mut());
        }
        if let Some(insert) = &mut merge.insert {
            exprs.extend(insert.values.iter_mut());
            exprs.extend(insert.condition.iter_mut());
        }
        for clause in &mut merge.by_source {
            exprs.extend(clause.assignments.iter_mut().map(|(_, expr)| expr));
            exprs.extend(clause.condition.iter_mut());
        }
        exprs.extend(returning_exprs(&mut merge.output));
        resolver.expressions(exprs)
    }

    pub(crate) fn delete(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        delete: &mut Delete,
    ) -> SqlResult<()> {
        if delete.using.is_some() {
            return self.joined_delete(reader, tables, delete);
        }
        let mut resolver = self.resolver(reader, tables)?;
        resolver.target(&mut delete.table)?;
        let table = delete.table.name.text.clone();
        let reads_rows = delete.filter.is_some() || delete.returning.is_some();
        let exprs: Vec<&mut Expr> = delete.filter.iter_mut().collect();
        resolver.walk_in_target_scope(&delete.table, &mut delete.returning, exprs, &[])?;
        let commands: &[crate::row_security::PolicyCommand] = if reads_rows {
            &[
                crate::row_security::PolicyCommand::Delete,
                crate::row_security::PolicyCommand::Select,
            ]
        } else {
            &[crate::row_security::PolicyCommand::Delete]
        };
        resolver.restrict_to_policies(&table, commands, &mut delete.filter)
    }

    pub(crate) fn create_table(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        create: &mut CreateTable,
    ) -> SqlResult<()> {
        let temporary = create.temporary.is_some()
            || crate::temporary::is_sqlserver_temporary(&create.name.text);
        if temporary {
            if is_qualified(&create.name.text)
                && !crate::temporary::is_temporary_name(&create.name.text)
            {
                return Err(SqlError::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "cannot create temporary relation in non-temporary schema",
                )
                .at(create.name.position));
            }
            create.name.text = crate::temporary::internal_name(base_name(&create.name.text));
        } else {
            create.name.text = schemas::creation_name(reader, &create.name)?;
        }
        if crate::temporary::is_temporary_name(&create.name.text) && create.temporary.is_none() {
            create.temporary = Some(crate::ast::OnCommit::PreserveRows);
        }
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
        crate::pg_catalog::refuse_catalog_change(reader, &alter.name)?;
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
            Statement::Merge(merge) => self.merge(reader, tables, merge),
            Statement::Explain(explain) => self.statement(reader, tables, &mut explain.statement),
            _ => Ok(()),
        }
    }
}

fn inserted_columns(
    reader: &dyn KeyValueRead,
    table: &str,
    insert: &Insert,
) -> SqlResult<Vec<String>> {
    if let Some(columns) = &insert.columns {
        return Ok(columns.iter().map(|name| name.text.clone()).collect());
    }
    let width = match &insert.source {
        InsertSource::DefaultValues => return Ok(Vec::new()),
        InsertSource::Values(rows) => Some(rows.iter().map(Vec::len).max().unwrap_or(0)),
        InsertSource::Query(query) => match &query.body {
            SetExpr::Select(select)
                if select
                    .items
                    .iter()
                    .all(|item| matches!(item, SelectItem::Expr { .. })) =>
            {
                Some(select.items.len())
            }
            _ => None,
        },
    };
    let columns: Vec<String> = crate::roles::relation_columns(reader, table)?
        .unwrap_or_default()
        .into_iter()
        .filter(|column| !crate::temp_tables::is_hidden_column(column))
        .collect();
    Ok(match width {
        Some(width) => columns.into_iter().take(width).collect(),
        None => columns,
    })
}

fn update_reads_rows(update: &Update) -> bool {
    update.filter.is_some()
        || update.returning.is_some()
        || update.assignments.iter().any(|(_, value)| match value {
            InsertValue::Expr(expr) => mentions_column(expr),
            InsertValue::Default => false,
        })
}

fn mentions_column(expr: &Expr) -> bool {
    let mut pending = vec![expr];
    while let Some(next) = pending.pop() {
        if matches!(
            next.kind,
            ExprKind::Column { .. } | ExprKind::Subquery(_) | ExprKind::OuterColumn { .. }
        ) {
            return true;
        }
        pending.extend(crate::eval::children(next));
    }
    false
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
            CommentTarget::Relation { name, .. } => {
                crate::pg_catalog::refuse_catalog_change(reader, name)?;
                resolver.name(name)
            }
            CommentTarget::Column { table, .. } | CommentTarget::Constraint { table, .. } => {
                crate::pg_catalog::refuse_catalog_change(reader, table)?;
                resolver.name(table)
            }
            CommentTarget::Index(_) | CommentTarget::Schema(_) | CommentTarget::Extension(_) => {
                Ok(())
            }
        }
    }
}

fn listed_reference(tables: &FromClause, at: usize) -> Option<&TableReference> {
    match at.checked_sub(1) {
        None => Some(&tables.first),
        Some(join) => tables.joins.get(join).map(|join| &join.table),
    }
}

fn joined_row_security_refused() -> SqlError {
    SqlError::not_supported(
        "UPDATE or DELETE joined with other tables on a table with row-level security for this role",
    )
}

impl Resolver<'_> {
    fn joined_target(
        &mut self,
        reference: &mut TableReference,
        command: crate::row_security::PolicyCommand,
        updated_columns: Option<&[String]>,
    ) -> SqlResult<()> {
        match updated_columns {
            Some(columns) => {
                self.resolve_target(reference)?;
                if self.target_privilege.is_some() {
                    let privilege = crate::roles::Privilege::Update;
                    self.check_written_columns(&reference.name.text, privilege, columns)?;
                    self.check_new_rows(&reference.name.text, privilege)?;
                }
            }
            None => self.target(reference)?,
        }
        if self
            .row_security_filter(&reference.name.text, command)?
            .is_some()
        {
            return Err(joined_row_security_refused());
        }
        Ok(())
    }
}

impl Binding<'_> {
    #[allow(clippy::too_many_arguments)]
    fn joined_reads(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        target: &TableReference,
        joined: &mut JoinedTables,
        exprs: Vec<&mut Expr>,
        returning: &mut Option<Vec<SelectItem>>,
        filter: &mut Option<Expr>,
    ) -> SqlResult<()> {
        let mut items: Vec<SelectItem> = exprs
            .iter()
            .map(|expr| SelectItem::Expr {
                expr: (**expr).clone(),
                alias: None,
            })
            .collect();
        items.extend(returning.iter().flatten().cloned());
        let mut select = Select {
            distinct: false,
            items,
            from: Some(crate::joined_dml::full_from(target, joined)),
            filter: filter.take(),
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit: None,
            offset: None,
            source_row_limit: None,
            grouping_sets: None,
            distinct_on: Vec::new(),
        };
        self.select(reader, tables, &mut select)?;
        let mut resolved = select.items.into_iter();
        for expr in exprs {
            if let Some(SelectItem::Expr {
                expr: resolved_expr,
                ..
            }) = resolved.next()
            {
                *expr = resolved_expr;
            }
        }
        for item in returning.iter_mut().flatten() {
            if let Some(resolved_item) = resolved.next() {
                *item = resolved_item;
            }
        }
        *filter = select.filter;
        let Some(from) = select.from else {
            return Ok(());
        };
        joined.tables = match joined.target_at {
            Some(_) => from,
            None => {
                let mut joins = from.joins.into_iter();
                let Some(first) = joins.next() else {
                    return Ok(());
                };
                FromClause {
                    first: first.table,
                    joins: joins.collect(),
                }
            }
        };
        Ok(())
    }

    fn written_tables(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        update: &mut Update,
    ) -> SqlResult<()> {
        let Some(joined) = &mut update.from else {
            return Ok(());
        };
        if !joined.writes_any_table {
            return Ok(());
        }
        let mut candidates = Vec::new();
        for (at, reference) in joined.references().enumerate() {
            if let Some(table) = crate::joined_dml::stored_table(reader, tables, reference)? {
                candidates.push((at, table));
            }
        }
        joined.assigned_at.resize(update.assignments.len(), None);
        for ((name, _), assigned) in update.assignments.iter().zip(&mut joined.assigned_at) {
            if assigned.is_none() {
                *assigned = Some(crate::joined_dml::column_owner(&candidates, name)?);
            }
        }
        joined.target_at = joined.assigned_at.iter().flatten().next().copied();
        Ok(())
    }

    fn joined_update(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        update: &mut Update,
    ) -> SqlResult<()> {
        let written = match &update.from {
            Some(joined) => joined.tables.clone(),
            None => return Ok(()),
        };
        {
            let Update {
                table,
                assignments,
                filter,
                returning,
                from,
                ..
            } = &mut *update;
            let Some(joined) = from else {
                return Ok(());
            };
            let exprs: Vec<&mut Expr> = assignments
                .iter_mut()
                .filter_map(|(_, value)| match value {
                    InsertValue::Expr(expr) => Some(expr),
                    InsertValue::Default => None,
                })
                .collect();
            self.joined_reads(reader, tables, table, joined, exprs, returning, filter)?;
        }
        self.written_tables(reader, tables, update)?;
        let Some(joined) = &update.from else {
            return Ok(());
        };
        let main = joined.target_at;
        if let Some(reference) = main.and_then(|at| listed_reference(&written, at)) {
            update.table = reference.clone();
        }
        let mut written_at: Vec<Option<usize>> = Vec::new();
        for at in &joined.assigned_at {
            if !written_at.contains(at) {
                written_at.push(*at);
            }
        }
        let written_columns = |target: Option<usize>| -> Vec<String> {
            update
                .assignments
                .iter()
                .enumerate()
                .filter(|(at, _)| joined.assigned_at.get(*at).copied().flatten().or(main) == target)
                .map(|(_, (name, _))| name.text.clone())
                .collect()
        };
        let main_columns = written_columns(main);
        let mut resolver = self.resolver(reader, tables)?;
        let update_command = crate::row_security::PolicyCommand::Update;
        resolver.joined_target(&mut update.table, update_command, Some(&main_columns))?;
        for at in written_at.into_iter().flatten() {
            if Some(at) == main {
                continue;
            }
            if let Some(mut reference) = listed_reference(&written, at).cloned() {
                let columns = written_columns(Some(at));
                resolver.joined_target(&mut reference, update_command, Some(&columns))?;
            }
        }
        Ok(())
    }

    fn joined_delete(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        delete: &mut Delete,
    ) -> SqlResult<()> {
        let Delete {
            table,
            filter,
            returning,
            using,
        } = delete;
        let Some(joined) = using else {
            return Ok(());
        };
        let written = joined.tables.clone();
        self.joined_reads(reader, tables, table, joined, Vec::new(), returning, filter)?;
        let mut resolver = self.resolver(reader, tables)?;
        let delete_command = crate::row_security::PolicyCommand::Delete;
        resolver.joined_target(table, delete_command, None)?;
        for &at in &joined.more_targets_at {
            if let Some(mut reference) = listed_reference(&written, at).cloned() {
                resolver.joined_target(&mut reference, delete_command, None)?;
            }
        }
        Ok(())
    }
}
