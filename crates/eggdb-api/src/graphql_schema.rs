//! The GraphQL schema the API generates from the tables (D876), answered through GraphQL's
//! introspection fields `__schema` and `__type`, so GraphiQL, Apollo and code generators can
//! read it. The types follow Hasura's names.

use crate::graphql::Field;
use crate::graphql_execute::{AGGREGATE_FUNCTIONS, Context, ForeignKey};
use crate::request::literal;
use crate::{Endpoint, json_string};
use eggdb_sql::{KeyValueStore, Value};

/// One table as the schema shows it.
#[derive(Debug, Clone, Default)]
pub(crate) struct TableModel {
    pub(crate) name: String,
    pub(crate) columns: Vec<ColumnModel>,
    pub(crate) primary_key: Vec<String>,
    pub(crate) constraints: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ColumnModel {
    pub(crate) name: String,
    pub(crate) scalar: String,
    pub(crate) not_null: bool,
}

/// GraphQL's scalar name for a column's SQL type, as Hasura names them.
pub(crate) fn scalar_of(data_type: &str) -> String {
    match data_type {
        "integer" => "Int".to_owned(),
        "text" | "character varying" | "character" | "name" | "citext" => "String".to_owned(),
        "boolean" => "Boolean".to_owned(),
        "double precision" => "float8".to_owned(),
        "real" => "float4".to_owned(),
        "timestamp with time zone" => "timestamptz".to_owned(),
        "timestamp without time zone" => "timestamp".to_owned(),
        "time with time zone" => "timetz".to_owned(),
        "time without time zone" => "time".to_owned(),
        other => {
            let cleaned: String = other
                .chars()
                .map(|character| if character.is_ascii_alphanumeric() { character } else { '_' })
                .collect();
            if cleaned.is_empty() { "String".to_owned() } else { cleaned }
        }
    }
}

fn is_numeric(scalar: &str) -> bool {
    matches!(scalar, "Int" | "Float" | "float8" | "float4" | "bigint" | "smallint" | "numeric")
}

/// An introspection value: what `__schema` and `__type` select from.
#[derive(Debug, Clone, PartialEq)]
enum Intro {
    Null,
    Bool(bool),
    Text(String),
    List(Vec<Intro>),
    Object(Vec<(&'static str, Intro)>),
}

fn text(value: &str) -> Intro {
    Intro::Text(value.to_owned())
}

#[derive(Debug, Clone)]
enum TypeRef {
    Named(&'static str, String),
    NonNull(Box<TypeRef>),
    List(Box<TypeRef>),
}

fn named(kind: &'static str, name: &str) -> TypeRef {
    TypeRef::Named(kind, name.to_owned())
}

fn non_null(inner: TypeRef) -> TypeRef {
    TypeRef::NonNull(Box::new(inner))
}

fn list_of(inner: TypeRef) -> TypeRef {
    TypeRef::List(Box::new(inner))
}

fn scalar(name: &str) -> TypeRef {
    named("SCALAR", name)
}

fn type_object(kind: &str, name: Option<&str>, of_type: Intro) -> Intro {
    Intro::Object(vec![
        ("__typename", text("__Type")),
        ("kind", text(kind)),
        ("name", name.map_or(Intro::Null, text)),
        ("description", Intro::Null),
        ("specifiedByURL", Intro::Null),
        ("fields", Intro::Null),
        ("interfaces", Intro::Null),
        ("possibleTypes", Intro::Null),
        ("enumValues", Intro::Null),
        ("inputFields", Intro::Null),
        ("ofType", of_type),
        ("isOneOf", Intro::Bool(false)),
    ])
}

impl TypeRef {
    fn intro(&self) -> Intro {
        match self {
            Self::Named(kind, name) => type_object(kind, Some(name), Intro::Null),
            Self::NonNull(inner) => type_object("NON_NULL", None, inner.intro()),
            Self::List(inner) => type_object("LIST", None, inner.intro()),
        }
    }
}

fn input_value(name: &str, of_type: TypeRef) -> Intro {
    Intro::Object(vec![
        ("__typename", text("__InputValue")),
        ("name", text(name)),
        ("description", Intro::Null),
        ("type", of_type.intro()),
        ("defaultValue", Intro::Null),
        ("isDeprecated", Intro::Bool(false)),
        ("deprecationReason", Intro::Null),
    ])
}

fn field(name: &str, arguments: Vec<Intro>, of_type: TypeRef) -> Intro {
    Intro::Object(vec![
        ("__typename", text("__Field")),
        ("name", text(name)),
        ("description", Intro::Null),
        ("args", Intro::List(arguments)),
        ("type", of_type.intro()),
        ("isDeprecated", Intro::Bool(false)),
        ("deprecationReason", Intro::Null),
    ])
}

fn enum_value(name: &str) -> Intro {
    Intro::Object(vec![
        ("__typename", text("__EnumValue")),
        ("name", text(name)),
        ("description", Intro::Null),
        ("isDeprecated", Intro::Bool(false)),
        ("deprecationReason", Intro::Null),
    ])
}

struct FullType {
    kind: &'static str,
    name: String,
    fields: Vec<Intro>,
    input_fields: Vec<Intro>,
    enum_values: Vec<Intro>,
}

impl FullType {
    fn new(kind: &'static str, name: &str) -> Self {
        Self {
            kind,
            name: name.to_owned(),
            fields: Vec::new(),
            input_fields: Vec::new(),
            enum_values: Vec::new(),
        }
    }

    fn intro(&self) -> Intro {
        let listed = |kind: &str, items: &[Intro]| {
            if self.kind == kind { Intro::List(items.to_vec()) } else { Intro::Null }
        };
        Intro::Object(vec![
            ("__typename", text("__Type")),
            ("kind", text(self.kind)),
            ("name", text(&self.name)),
            ("description", Intro::Null),
            ("specifiedByURL", Intro::Null),
            ("fields", listed("OBJECT", &self.fields)),
            ("interfaces", listed("OBJECT", &[])),
            ("possibleTypes", Intro::Null),
            ("enumValues", listed("ENUM", &self.enum_values)),
            ("inputFields", listed("INPUT_OBJECT", &self.input_fields)),
            ("ofType", Intro::Null),
            ("isOneOf", Intro::Bool(false)),
        ])
    }
}

const ORDER_DIRECTIONS: [&str; 6] = [
    "asc",
    "asc_nulls_first",
    "asc_nulls_last",
    "desc",
    "desc_nulls_first",
    "desc_nulls_last",
];

const TEXT_OPERATORS: [&str; 10] = [
    "_like", "_nlike", "_ilike", "_nilike", "_similar", "_nsimilar", "_regex", "_nregex", "_iregex", "_niregex",
];

fn related_tables(foreign_keys: &[ForeignKey], table: &str) -> Vec<(String, String, bool)> {
    let mut related = Vec::new();
    for key in foreign_keys {
        if key.child == table {
            related.push((key.parent.clone(), key.parent.clone(), true));
            if let [only] = key.child_columns.as_slice()
                && let Some(short) = only.strip_suffix("_id")
                && short != key.parent
            {
                related.push((short.to_owned(), key.parent.clone(), true));
            }
        }
        if key.parent == table {
            related.push((key.child.clone(), key.child.clone(), false));
        }
    }
    related
}

fn list_arguments(table: &str) -> Vec<Intro> {
    vec![
        input_value("distinct_on", list_of(non_null(named("ENUM", &format!("{table}_select_column"))))),
        input_value("limit", scalar("Int")),
        input_value("offset", scalar("Int")),
        input_value("order_by", list_of(non_null(named("INPUT_OBJECT", &format!("{table}_order_by"))))),
        input_value("where", named("INPUT_OBJECT", &format!("{table}_bool_exp"))),
    ]
}

fn key_arguments(table: &TableModel) -> Vec<Intro> {
    table
        .primary_key
        .iter()
        .filter_map(|name| table.columns.iter().find(|column| column.name == *name))
        .map(|column| input_value(&column.name, non_null(scalar(&column.scalar))))
        .collect()
}

fn column_type(column: &ColumnModel) -> TypeRef {
    if column.not_null { non_null(scalar(&column.scalar)) } else { scalar(&column.scalar) }
}

fn table_types(table: &TableModel, foreign_keys: &[ForeignKey], types: &mut Vec<FullType>) {
    let name = table.name.as_str();
    let related = related_tables(foreign_keys, name);
    let mut object = FullType::new("OBJECT", name);
    for column in &table.columns {
        object.fields.push(field(&column.name, Vec::new(), column_type(column)));
    }
    for (relationship, target, single) in &related {
        if *single {
            object.fields.push(field(relationship, Vec::new(), named("OBJECT", target)));
        } else {
            object.fields.push(field(
                relationship,
                list_arguments(target),
                non_null(list_of(non_null(named("OBJECT", target)))),
            ));
        }
    }
    types.push(object);

    let bool_exp = format!("{name}_bool_exp");
    let mut filter = FullType::new("INPUT_OBJECT", &bool_exp);
    filter.input_fields.push(input_value("_and", list_of(non_null(named("INPUT_OBJECT", &bool_exp)))));
    filter.input_fields.push(input_value("_not", named("INPUT_OBJECT", &bool_exp)));
    filter.input_fields.push(input_value("_or", list_of(non_null(named("INPUT_OBJECT", &bool_exp)))));
    for column in &table.columns {
        filter.input_fields.push(input_value(
            &column.name,
            named("INPUT_OBJECT", &format!("{}_comparison_exp", column.scalar)),
        ));
    }
    for (relationship, target, _) in &related {
        filter.input_fields.push(input_value(relationship, named("INPUT_OBJECT", &format!("{target}_bool_exp"))));
    }
    types.push(filter);

    let mut order = FullType::new("INPUT_OBJECT", &format!("{name}_order_by"));
    let mut select_column = FullType::new("ENUM", &format!("{name}_select_column"));
    let mut update_column = FullType::new("ENUM", &format!("{name}_update_column"));
    let mut insert_input = FullType::new("INPUT_OBJECT", &format!("{name}_insert_input"));
    let mut set_input = FullType::new("INPUT_OBJECT", &format!("{name}_set_input"));
    let mut inc_input = FullType::new("INPUT_OBJECT", &format!("{name}_inc_input"));
    for column in &table.columns {
        order.input_fields.push(input_value(&column.name, named("ENUM", "order_by")));
        select_column.enum_values.push(enum_value(&column.name));
        update_column.enum_values.push(enum_value(&column.name));
        insert_input.input_fields.push(input_value(&column.name, scalar(&column.scalar)));
        set_input.input_fields.push(input_value(&column.name, scalar(&column.scalar)));
        if is_numeric(&column.scalar) {
            inc_input.input_fields.push(input_value(&column.name, scalar(&column.scalar)));
        }
    }
    types.extend([order, select_column, update_column, insert_input, set_input]);
    if !inc_input.input_fields.is_empty() {
        types.push(inc_input);
    }

    if !table.primary_key.is_empty() {
        let mut key_input = FullType::new("INPUT_OBJECT", &format!("{name}_pk_columns_input"));
        key_input.input_fields = key_arguments(table);
        types.push(key_input);
    }
    if !table.constraints.is_empty() {
        let mut constraint = FullType::new("ENUM", &format!("{name}_constraint"));
        constraint.enum_values = table.constraints.iter().map(|name| enum_value(name)).collect();
        types.push(constraint);
        let mut conflict = FullType::new("INPUT_OBJECT", &format!("{name}_on_conflict"));
        conflict.input_fields = vec![
            input_value("constraint", non_null(named("ENUM", &format!("{name}_constraint")))),
            input_value("update_columns", non_null(list_of(non_null(named("ENUM", &format!("{name}_update_column")))))),
            input_value("where", named("INPUT_OBJECT", &bool_exp)),
        ];
        types.push(conflict);
    }

    let mut response = FullType::new("OBJECT", &format!("{name}_mutation_response"));
    response.fields = vec![
        field("affected_rows", Vec::new(), non_null(scalar("Int"))),
        field("returning", Vec::new(), non_null(list_of(non_null(named("OBJECT", name))))),
    ];
    types.push(response);

    let mut aggregate = FullType::new("OBJECT", &format!("{name}_aggregate"));
    aggregate.fields = vec![
        field("aggregate", Vec::new(), named("OBJECT", &format!("{name}_aggregate_fields"))),
        field("nodes", Vec::new(), non_null(list_of(non_null(named("OBJECT", name))))),
    ];
    types.push(aggregate);
    let mut fields = FullType::new("OBJECT", &format!("{name}_aggregate_fields"));
    fields.fields.push(field(
        "count",
        vec![
            input_value("columns", list_of(non_null(named("ENUM", &format!("{name}_select_column"))))),
            input_value("distinct", scalar("Boolean")),
        ],
        non_null(scalar("Int")),
    ));
    for function in AGGREGATE_FUNCTIONS {
        let summarized: Vec<&ColumnModel> = table
            .columns
            .iter()
            .filter(|column| matches!(function, "max" | "min") || is_numeric(&column.scalar))
            .collect();
        if summarized.is_empty() {
            continue;
        }
        let type_name = format!("{name}_{function}_fields");
        let mut summary = FullType::new("OBJECT", &type_name);
        for column in summarized {
            let result = if matches!(function, "max" | "min" | "sum") { column.scalar.as_str() } else { "Float" };
            summary.fields.push(field(&column.name, Vec::new(), scalar(result)));
        }
        types.push(summary);
        fields.fields.push(field(function, Vec::new(), named("OBJECT", &type_name)));
    }
    types.push(fields);
}

fn comparison_type(scalar_name: &str) -> FullType {
    let mut comparison = FullType::new("INPUT_OBJECT", &format!("{scalar_name}_comparison_exp"));
    for operator in ["_eq", "_gt", "_gte", "_lt", "_lte", "_neq"] {
        comparison.input_fields.push(input_value(operator, scalar(scalar_name)));
    }
    for operator in ["_in", "_nin"] {
        comparison.input_fields.push(input_value(operator, list_of(non_null(scalar(scalar_name)))));
    }
    comparison.input_fields.push(input_value("_is_null", scalar("Boolean")));
    if scalar_name == "String" {
        for operator in TEXT_OPERATORS {
            comparison.input_fields.push(input_value(operator, scalar("String")));
        }
    }
    if scalar_name == "jsonb" {
        for operator in ["_contains", "_contained_in"] {
            comparison.input_fields.push(input_value(operator, scalar("jsonb")));
        }
    }
    comparison.input_fields.sort_by_key(|value| match value {
        Intro::Object(members) => members
            .iter()
            .find(|(key, _)| *key == "name")
            .map(|(_, name)| format!("{name:?}"))
            .unwrap_or_default(),
        _ => String::new(),
    });
    comparison
}

fn root_types(tables: &[TableModel]) -> (FullType, FullType) {
    let mut query = FullType::new("OBJECT", "query_root");
    let mut mutation = FullType::new("OBJECT", "mutation_root");
    for table in tables {
        let name = table.name.as_str();
        let object = || named("OBJECT", name);
        let response = || named("OBJECT", &format!("{name}_mutation_response"));
        let bool_exp = || named("INPUT_OBJECT", &format!("{name}_bool_exp"));
        let set = || input_value("_set", named("INPUT_OBJECT", &format!("{name}_set_input")));
        let increments = || input_value("_inc", named("INPUT_OBJECT", &format!("{name}_inc_input")));
        let conflict = || input_value("on_conflict", named("INPUT_OBJECT", &format!("{name}_on_conflict")));
        let insert_input = || named("INPUT_OBJECT", &format!("{name}_insert_input"));
        query.fields.push(field(name, list_arguments(name), non_null(list_of(non_null(object())))));
        query.fields.push(field(
            &format!("{name}_aggregate"),
            list_arguments(name),
            non_null(named("OBJECT", &format!("{name}_aggregate"))),
        ));
        if !table.primary_key.is_empty() {
            query.fields.push(field(&format!("{name}_by_pk"), key_arguments(table), object()));
        }
        let with_conflict = |mut arguments: Vec<Intro>| {
            if !table.constraints.is_empty() {
                arguments.push(conflict());
            }
            arguments
        };
        mutation.fields.push(field(
            &format!("delete_{name}"),
            vec![input_value("where", non_null(bool_exp()))],
            response(),
        ));
        mutation.fields.push(field(
            &format!("insert_{name}"),
            with_conflict(vec![input_value("objects", non_null(list_of(non_null(insert_input()))))]),
            response(),
        ));
        mutation.fields.push(field(
            &format!("insert_{name}_one"),
            with_conflict(vec![input_value("object", non_null(insert_input()))]),
            object(),
        ));
        mutation.fields.push(field(
            &format!("update_{name}"),
            vec![increments(), set(), input_value("where", non_null(bool_exp()))],
            response(),
        ));
        if !table.primary_key.is_empty() {
            mutation.fields.push(field(&format!("delete_{name}_by_pk"), key_arguments(table), object()));
            mutation.fields.push(field(
                &format!("update_{name}_by_pk"),
                vec![
                    increments(),
                    set(),
                    input_value("pk_columns", non_null(named("INPUT_OBJECT", &format!("{name}_pk_columns_input")))),
                ],
                object(),
            ));
        }
    }
    (query, mutation)
}

fn directive(name: &str, locations: &[&str], arguments: Vec<Intro>) -> Intro {
    Intro::Object(vec![
        ("__typename", text("__Directive")),
        ("name", text(name)),
        ("description", Intro::Null),
        ("locations", Intro::List(locations.iter().map(|location| text(location)).collect())),
        ("args", Intro::List(arguments)),
        ("isRepeatable", Intro::Bool(false)),
    ])
}

/// Every type of the schema generated from `tables`, in name order.
fn all_types(tables: &[TableModel], foreign_keys: &[ForeignKey]) -> Vec<FullType> {
    let mut types = Vec::new();
    let mut scalars: Vec<String> = ["Boolean", "Float", "ID", "Int", "String"].iter().map(|name| (*name).to_owned()).collect();
    for table in tables {
        table_types(table, foreign_keys, &mut types);
        for column in &table.columns {
            if !scalars.contains(&column.scalar) {
                scalars.push(column.scalar.clone());
            }
        }
    }
    for name in &scalars {
        types.push(FullType::new("SCALAR", name));
        types.push(comparison_type(name));
    }
    let mut order = FullType::new("ENUM", "order_by");
    order.enum_values = ORDER_DIRECTIONS.iter().map(|direction| enum_value(direction)).collect();
    types.push(order);
    let (query, mutation) = root_types(tables);
    types.push(query);
    types.push(mutation);
    types.sort_by(|left, right| left.name.cmp(&right.name));
    types
}

fn schema_intro(tables: &[TableModel], foreign_keys: &[ForeignKey]) -> Intro {
    let types = all_types(tables, foreign_keys);
    let find = |name: &str| {
        types
            .iter()
            .find(|candidate| candidate.name == name)
            .map_or(Intro::Null, FullType::intro)
    };
    let condition = || vec![input_value("if", non_null(scalar("Boolean")))];
    Intro::Object(vec![
        ("__typename", text("__Schema")),
        ("description", Intro::Null),
        ("types", Intro::List(types.iter().map(FullType::intro).collect())),
        ("queryType", find("query_root")),
        ("mutationType", find("mutation_root")),
        ("subscriptionType", Intro::Null),
        (
            "directives",
            Intro::List(vec![
                directive("include", &["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"], condition()),
                directive("skip", &["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"], condition()),
                directive(
                    "deprecated",
                    &["FIELD_DEFINITION", "ARGUMENT_DEFINITION", "INPUT_FIELD_DEFINITION", "ENUM_VALUE"],
                    vec![input_value("reason", scalar("String"))],
                ),
            ]),
        ),
    ])
}

fn project(context: &Context<'_>, value: &Intro, selection: &Field) -> Result<String, String> {
    match value {
        Intro::Null => Ok("null".to_owned()),
        Intro::Bool(flag) => Ok(flag.to_string()),
        Intro::Text(text) => Ok(json_string(text)),
        Intro::List(items) => {
            let mut projected = Vec::new();
            for item in items {
                projected.push(project(context, item, selection)?);
            }
            Ok(format!("[{}]", projected.join(",")))
        }
        Intro::Object(members) => {
            let type_name = members
                .iter()
                .find(|(key, _)| *key == "__typename")
                .and_then(|(_, value)| match value {
                    Intro::Text(name) => Some(name.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let mut parts = Vec::new();
            for wanted in context.fields(&selection.selections, 0)? {
                let member = members
                    .iter()
                    .find(|(key, _)| *key == wanted.name)
                    .map(|(_, value)| value)
                    .ok_or_else(|| format!("field '{}' not found in type: '{type_name}'", wanted.name))?;
                parts.push(format!("{}:{}", json_string(wanted.key()), project(context, member, &wanted)?));
            }
            Ok(format!("{{{}}}", parts.join(",")))
        }
    }
}

/// The JSON of a `__schema` or `__type(name: ...)` field.
pub(crate) fn introspect(context: &Context<'_>, tables: &[TableModel], wanted: &Field) -> Result<String, String> {
    let schema = schema_intro(tables, &context.foreign_keys);
    if wanted.name == "__schema" {
        return project(context, &schema, wanted);
    }
    let Some(crate::graphql::GqlValue::String(name)) = context.argument(wanted, "name") else {
        return Err("__type needs a String 'name' argument".to_owned());
    };
    let Intro::Object(members) = &schema else {
        return Ok("null".to_owned());
    };
    let found = members
        .iter()
        .find(|(key, _)| *key == "types")
        .and_then(|(_, types)| match types {
            Intro::List(types) => types.iter().find(|candidate| match candidate {
                Intro::Object(fields) => fields.iter().any(|(key, value)| *key == "name" && *value == Intro::Text(name.clone())),
                _ => false,
            }),
            _ => None,
        });
    match found {
        Some(found) => project(context, found, wanted),
        None => Ok("null".to_owned()),
    }
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    /// The tables of the API's schema with their columns, primary keys and unique
    /// constraints, for introspection.
    pub(crate) fn schema_model(&self, context: &Context<'_>) -> Result<Vec<TableModel>, eggdb_sql::SqlError> {
        let schema = literal(&self.schema);
        let mut tables: Vec<TableModel> = context
            .tables
            .iter()
            .map(|name| TableModel {
                name: name.clone(),
                ..TableModel::default()
            })
            .collect();
        let columns = self.run(&format!(
            "SELECT table_name, column_name, data_type, is_nullable FROM information_schema.columns WHERE table_schema = {schema} ORDER BY table_name, ordinal_position"
        ))?;
        for row in columns
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
        {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            if let Some(table) = tables.iter_mut().find(|table| table.name == text(0)) {
                table.columns.push(ColumnModel {
                    name: text(1),
                    scalar: scalar_of(&text(2)),
                    not_null: text(3) == "NO",
                });
            }
        }
        let keys = self.run(&format!(
            "SELECT tc.table_name, tc.constraint_name, tc.constraint_type, kcu.column_name FROM information_schema.table_constraints tc JOIN information_schema.key_column_usage kcu ON kcu.constraint_schema = tc.constraint_schema AND kcu.constraint_name = tc.constraint_name AND kcu.table_name = tc.table_name WHERE tc.table_schema = {schema} AND tc.constraint_type IN ('PRIMARY KEY', 'UNIQUE') ORDER BY tc.table_name, tc.constraint_name, kcu.ordinal_position"
        ))?;
        for row in keys
            .iter()
            .filter(|result| result.columns.is_some())
            .flat_map(|result| result.rows.iter())
        {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            if let Some(table) = tables.iter_mut().find(|table| table.name == text(0)) {
                let constraint = text(1);
                if !table.constraints.contains(&constraint) {
                    table.constraints.push(constraint);
                }
                if text(2) == "PRIMARY KEY" {
                    table.primary_key.push(text(3));
                }
            }
        }
        Ok(tables)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn books() -> Vec<TableModel> {
        vec![TableModel {
            name: "books".to_owned(),
            columns: vec![
                ColumnModel {
                    name: "id".to_owned(),
                    scalar: "Int".to_owned(),
                    not_null: true,
                },
                ColumnModel {
                    name: "title".to_owned(),
                    scalar: "String".to_owned(),
                    not_null: false,
                },
            ],
            primary_key: vec!["id".to_owned()],
            constraints: vec!["books_pkey".to_owned()],
        }]
    }

    #[test]
    fn sql_types_map_to_hasura_scalars() {
        assert_eq!(scalar_of("integer"), "Int");
        assert_eq!(scalar_of("character varying"), "String");
        assert_eq!(scalar_of("timestamp with time zone"), "timestamptz");
        assert_eq!(scalar_of("numeric"), "numeric");
    }

    #[test]
    fn the_schema_lists_root_fields_and_table_types() {
        let types = all_types(&books(), &[]);
        let names: Vec<&str> = types.iter().map(|found| found.name.as_str()).collect();
        for expected in [
            "books",
            "books_bool_exp",
            "books_order_by",
            "books_pk_columns_input",
            "books_on_conflict",
            "books_aggregate_fields",
            "Int_comparison_exp",
            "String_comparison_exp",
            "order_by",
            "query_root",
            "mutation_root",
        ] {
            assert!(names.contains(&expected), "{expected} in {names:?}");
        }
        let query = types.iter().find(|found| found.name == "query_root").unwrap();
        assert_eq!(query.fields.len(), 3);
    }

    #[test]
    fn type_references_nest_through_of_type() {
        let reference = non_null(list_of(non_null(named("OBJECT", "books")))).intro();
        let Intro::Object(members) = reference else {
            panic!("an object");
        };
        assert!(members.contains(&("kind", text("NON_NULL"))));
    }
}
