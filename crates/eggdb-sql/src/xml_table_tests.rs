use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session};

fn last(session: &mut Session<'_>, sql: &str) -> String {
    let mut outputs = Vec::new();
    match session.execute(sql, &mut |result| outputs.push(render(&result))) {
        Ok(()) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

const SHOP: &str = "'<shop><item id=\"1\"><name>egg</name><price>2.5</price></item><item id=\"2\"><name>milk</name></item></shop>'";

#[test]
fn xmltable_makes_rows_with_typed_columns_defaults_and_ordinality() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::PostgreSql);
    let rows = last(
        &mut session,
        &format!(
            "SELECT * FROM XMLTABLE('/shop/item' PASSING {SHOP} COLUMNS n FOR ORDINALITY, id integer PATH '@id', \
             name text, price numeric PATH 'price' DEFAULT 0)"
        ),
    );
    for expected in ["egg", "milk", "2.5"] {
        assert!(rows.contains(expected), "{expected} in {rows}");
    }
    let count = last(
        &mut session,
        &format!("SELECT count(*) FROM XMLTABLE('//item[price]' PASSING BY VALUE {SHOP} COLUMNS name text)"),
    );
    assert!(count.contains('1'), "{count}");
}

#[test]
fn oracle_xmltable_without_columns_returns_column_value() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    let rows = last(&mut session, &format!("SELECT * FROM XMLTABLE('/shop/item/name' PASSING {SHOP})"));
    assert!(rows.contains("<name>egg</name>"), "{rows}");
}

#[test]
fn xmlquery_serializes_and_xmlexists_tests() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    let rows = last(
        &mut session,
        &format!("SELECT XMLQUERY('/shop/item[@id=\"2\"]/name' PASSING {SHOP} RETURNING CONTENT) FROM dual"),
    );
    assert!(rows.contains("<name>milk</name>"), "{rows}");
    let mut postgres = Session::with_dialect(&store, Dialect::PostgreSql);
    let exists = last(&mut postgres, &format!("SELECT XMLEXISTS('//item[@id=\"9\"]' PASSING BY REF {SHOP})"));
    assert!(exists.contains('f'), "{exists}");
}

#[test]
fn sqlserver_xml_columns_answer_value_query_and_exist() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::SqlServer);
    last(&mut session, "CREATE TABLE orders (id int, doc xml)");
    last(
        &mut session,
        "INSERT INTO orders VALUES (1, '<order total=\"12.5\"><line sku=\"egg\"/><line sku=\"milk\"/></order>')",
    );
    let rows = last(
        &mut session,
        "SELECT doc.value('(/order/@total)[1]', 'decimal(9,2)') + 1, o.doc.exist('//line[@sku=\"milk\"]'), \
         doc.query('/order/line[1]') FROM orders o",
    );
    for expected in ["13.50", "1", "<line sku=\"egg\"/>"] {
        assert!(rows.contains(expected), "{expected} in {rows}");
    }
}
