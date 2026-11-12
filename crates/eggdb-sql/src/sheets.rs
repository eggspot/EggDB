/// The statements `CREATE EXTENSION eggdb_sheets` runs: spreadsheet-style tables of cells
/// with formulas, and formula columns filled down every row, recalculated by
/// `spreadsheet_recalculate` (D38).
pub(crate) const SHEETS_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_sheets;
CREATE TABLE eggdb_sheets.sheets (sheet text PRIMARY KEY, created_by text NOT NULL DEFAULT current_user, created_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE eggdb_sheets.cells (sheet text NOT NULL REFERENCES eggdb_sheets.sheets (sheet) ON DELETE CASCADE, cell text NOT NULL CHECK (cell ~ '^[A-Z]{1,3}[1-9][0-9]{0,6}$'), raw text NOT NULL, updated_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (sheet, cell));
CREATE TABLE eggdb_sheets.formula_columns (sheet text NOT NULL REFERENCES eggdb_sheets.sheets (sheet) ON DELETE CASCADE, column_letters text NOT NULL CHECK (column_letters ~ '^[A-Z]{1,3}$'), formula text NOT NULL CHECK (formula LIKE '=%'), first_row integer NOT NULL DEFAULT 2 CHECK (first_row >= 1), PRIMARY KEY (sheet, column_letters));
CREATE FUNCTION eggdb_sheets.set_cell(sheet_name text, cell_name text, cell_raw text) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO eggdb_sheets.sheets (sheet) VALUES (sheet_name) ON CONFLICT (sheet) DO NOTHING;
    IF cell_raw IS NULL OR cell_raw = '' THEN
        DELETE FROM eggdb_sheets.cells c WHERE c.sheet = sheet_name AND c.cell = upper(cell_name);
    ELSE
        INSERT INTO eggdb_sheets.cells (sheet, cell, raw) VALUES (sheet_name, upper(cell_name), cell_raw)
            ON CONFLICT (sheet, cell) DO UPDATE SET raw = EXCLUDED.raw, updated_at = now();
    END IF;
END
$$;
CREATE FUNCTION eggdb_sheets.raw_cells(sheet_name text) RETURNS jsonb LANGUAGE plpgsql AS $$
DECLARE
    entered jsonb;
    last_row integer;
    filled record;
BEGIN
    SELECT coalesce(jsonb_object_agg(c.cell, c.raw), '{}'::jsonb), coalesce(max(ltrim(c.cell, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ')::integer), 0)
        INTO entered, last_row FROM eggdb_sheets.cells c WHERE c.sheet = sheet_name;
    FOR filled IN SELECT f.column_letters, f.formula, f.first_row FROM eggdb_sheets.formula_columns f WHERE f.sheet = sheet_name ORDER BY f.column_letters LOOP
        FOR filled_row IN filled.first_row .. last_row LOOP
            IF (entered -> (filled.column_letters || filled_row)) IS NULL THEN
                entered := entered || jsonb_build_object(filled.column_letters || filled_row, replace(filled.formula, '{row}', filled_row::text));
            END IF;
        END LOOP;
    END LOOP;
    RETURN entered;
END
$$;
CREATE FUNCTION eggdb_sheets.cell_values(sheet_name text) RETURNS TABLE (cell text, raw text, value jsonb) LANGUAGE plpgsql AS $$
DECLARE
    entered jsonb;
    computed jsonb;
BEGIN
    entered := eggdb_sheets.raw_cells(sheet_name);
    computed := spreadsheet_recalculate(entered);
    RETURN QUERY SELECT v.key, entered ->> v.key, v.value FROM jsonb_each(computed) v;
END
$$;
CREATE FUNCTION eggdb_sheets.grid(sheet_name text) RETURNS TABLE (row_number integer, cells jsonb) LANGUAGE plpgsql AS $$
BEGIN
    RETURN QUERY SELECT ltrim(v.cell, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ')::integer, jsonb_object_agg(rtrim(v.cell, '0123456789'), v.value)
        FROM eggdb_sheets.cell_values(sheet_name) v
        GROUP BY 1
        ORDER BY 1;
END
$$;
CREATE FUNCTION eggdb_sheets.import_table(sheet_name text, query text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found_row record;
    field record;
    row_at integer := 1;
    column_at integer;
    header_written boolean := false;
    letters text;
BEGIN
    INSERT INTO eggdb_sheets.sheets (sheet) VALUES (sheet_name) ON CONFLICT (sheet) DO NOTHING;
    FOR found_row IN EXECUTE format('SELECT to_json(q)::text AS body FROM (%s) q', query) LOOP
        IF NOT header_written THEN
            column_at := 0;
            FOR field IN SELECT e.key FROM json_each_text(found_row.body::json) e LOOP
                column_at := column_at + 1;
                letters := CASE WHEN column_at > 26 THEN chr(64 + (column_at - 1) / 26) ELSE '' END || chr(65 + (column_at - 1) % 26);
                PERFORM eggdb_sheets.set_cell(sheet_name, letters || '1', field.key);
            END LOOP;
            header_written := true;
        END IF;
        row_at := row_at + 1;
        column_at := 0;
        FOR field IN SELECT e.value FROM json_each_text(found_row.body::json) e LOOP
            column_at := column_at + 1;
            letters := CASE WHEN column_at > 26 THEN chr(64 + (column_at - 1) / 26) ELSE '' END || chr(65 + (column_at - 1) % 26);
            IF field.value IS NOT NULL THEN
                PERFORM eggdb_sheets.set_cell(sheet_name, letters || row_at, field.value);
            END IF;
        END LOOP;
    END LOOP;
    RETURN row_at - 1;
END
$$;
"#;

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn formula_columns_fill_down_and_recalculate() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_sheets",
            "SELECT eggdb_sheets.set_cell('orders', 'A1', 'item'), eggdb_sheets.set_cell('orders', 'B1', 'price'), eggdb_sheets.set_cell('orders', 'C1', 'qty'), eggdb_sheets.set_cell('orders', 'D1', 'total')",
            "SELECT eggdb_sheets.set_cell('orders', 'A2', 'eggs'), eggdb_sheets.set_cell('orders', 'B2', '0.5'), eggdb_sheets.set_cell('orders', 'C2', '12')",
            "SELECT eggdb_sheets.set_cell('orders', 'A3', 'milk'), eggdb_sheets.set_cell('orders', 'B3', '1.25'), eggdb_sheets.set_cell('orders', 'C3', '2')",
            "INSERT INTO eggdb_sheets.formula_columns (sheet, column_letters, formula) VALUES ('orders', 'D', '=B{row}*C{row}')",
            "SELECT eggdb_sheets.set_cell('orders', 'D4', '=SUM(D2:D3)')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT cell, value FROM eggdb_sheets.cell_values('orders') WHERE cell LIKE 'D%' ORDER BY cell").unwrap(),
            "cell|value\nD1|\"total\"\nD2|6\nD3|2.5\nD4|8.5\nSELECT 4"
        );
    }
}
