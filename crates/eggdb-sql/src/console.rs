pub(crate) const CONSOLE_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_console;
CREATE FUNCTION eggdb_console.er_diagram(schema_name text, diagram_format text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    entities text;
    relations text;
BEGIN
    IF diagram_format NOT IN ('mermaid', 'dot') THEN
        RAISE EXCEPTION 'diagram format % is not mermaid or dot', diagram_format USING ERRCODE = '22023';
    END IF;
    SELECT string_agg(entity, chr(10) ORDER BY table_name) INTO entities FROM (
        SELECT c.table_name,
            CASE WHEN diagram_format = 'mermaid'
                THEN '    ' || c.table_name || ' {' || chr(10) || string_agg('        ' || replace(c.data_type, ' ', '_') || ' ' || c.column_name || CASE WHEN k.column_name IS NOT NULL THEN ' PK' ELSE '' END, chr(10) ORDER BY c.ordinal_position) || chr(10) || '    }'
                ELSE '    "' || c.table_name || '" [shape=record, label="{' || c.table_name || '|' || string_agg(CASE WHEN k.column_name IS NOT NULL THEN '+ ' ELSE '' END || c.column_name || ' : ' || c.data_type || '\l', '' ORDER BY c.ordinal_position) || '}"];'
            END AS entity
        FROM information_schema.columns c
        LEFT JOIN (
            SELECT u.table_name, u.column_name
            FROM information_schema.table_constraints t
            JOIN information_schema.key_column_usage u ON u.constraint_name = t.constraint_name AND u.table_schema = t.table_schema AND u.table_name = t.table_name
            WHERE t.constraint_type = 'PRIMARY KEY' AND t.table_schema = schema_name
        ) k ON k.table_name = c.table_name AND k.column_name = c.column_name
        WHERE c.table_schema = schema_name
        GROUP BY c.table_name
    ) described;
    SELECT string_agg(
        CASE WHEN diagram_format = 'mermaid'
            THEN '    ' || child.table_name || ' }o--|| ' || parent.table_name || ' : "' || child.column_name || '"'
            ELSE '    "' || child.table_name || '" -> "' || parent.table_name || '" [label="' || child.column_name || '"];'
        END, chr(10) ORDER BY child.table_name, child.column_name) INTO relations
    FROM information_schema.referential_constraints r
    JOIN information_schema.key_column_usage child ON child.constraint_name = r.constraint_name AND child.constraint_schema = r.constraint_schema
    JOIN information_schema.constraint_column_usage parent ON parent.constraint_name = r.constraint_name AND parent.constraint_schema = r.constraint_schema
    WHERE r.constraint_schema = schema_name;
    IF diagram_format = 'mermaid' THEN
        RETURN 'erDiagram' || coalesce(chr(10) || entities, '') || coalesce(chr(10) || relations, '');
    END IF;
    RETURN 'digraph ' || schema_name || ' {' || coalesce(chr(10) || entities, '') || coalesce(chr(10) || relations, '') || chr(10) || '}';
END
$$;
"#;

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::{Session, Value};

    fn diagram(session: &mut Session<'_, MemoryStore>, format: &str) -> String {
        let mut text = String::new();
        session
            .execute(&format!("SELECT eggdb_console.er_diagram('public', '{format}')"), &mut |result| {
                text = result.rows.first().and_then(|row| row.first()).and_then(Value::to_text).unwrap_or_default();
            })
            .unwrap();
        text
    }

    #[test]
    fn the_schema_draws_as_mermaid_and_graphviz() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        session
            .execute(
                "CREATE EXTENSION eggdb_console; CREATE TABLE customers (id integer PRIMARY KEY, name text); CREATE TABLE orders (id integer PRIMARY KEY, customer_id integer REFERENCES customers (id), total numeric)",
                &mut |_| {},
            )
            .unwrap();
        let mermaid = diagram(&mut session, "mermaid");
        assert!(mermaid.starts_with("erDiagram\n    customers {\n        integer id PK\n        text name\n    }"), "{mermaid}");
        assert!(mermaid.ends_with("    orders }o--|| customers : \"customer_id\""), "{mermaid}");
        let dot = diagram(&mut session, "dot");
        assert!(dot.starts_with("digraph public {") && dot.contains("\"orders\" -> \"customers\" [label=\"customer_id\"];") && dot.ends_with('}'), "{dot}");
        assert!(session.execute("SELECT eggdb_console.er_diagram('public', 'svg')", &mut |_| {}).is_err());
    }
}
