use crate::memory_store::MemoryStore;
use crate::tests::render;
use crate::{Dialect, Session, SqlError};

fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
    let mut outputs = Vec::new();
    session.execute(sql, &mut |result| outputs.push(render(&result)))?;
    Ok(outputs)
}

fn last(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => outputs.last().cloned().unwrap_or_default(),
        Err(error) => panic!("{sql}: {error}"),
    }
}

fn refused(session: &mut Session<'_>, sql: &str) -> String {
    match run(session, sql) {
        Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
        Err(error) => error.message().to_owned(),
    }
}

fn with_items_sold(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::new(store);
    last(
        &mut session,
        "CREATE TABLE items_sold (brand TEXT, size TEXT, sales INT); \
         INSERT INTO items_sold VALUES ('Foo', 'L', 10), ('Foo', 'M', 20), ('Bar', 'M', 15), ('Bar', 'L', 5)",
    );
    session
}

#[test]
fn grouping_sets_group_by_each_set_and_leave_the_other_keys_null() {
    let store = MemoryStore::default();
    let mut session = with_items_sold(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, size, sum(sales) FROM items_sold GROUP BY GROUPING SETS ((brand), (size), ())"
        ),
        "brand|size|sum\nFoo|NULL|30\nBar|NULL|20\nNULL|L|15\nNULL|M|35\nNULL|NULL|50\nSELECT 5"
    );
}

#[test]
fn rollup_puts_each_subtotal_after_its_rows_and_the_total_last() {
    let store = MemoryStore::default();
    let mut session = with_items_sold(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, size, sum(sales) FROM items_sold GROUP BY ROLLUP (brand, size)"
        ),
        "brand|size|sum\nBar|L|5\nBar|M|15\nBar|NULL|20\nFoo|L|10\nFoo|M|20\nFoo|NULL|30\nNULL|NULL|50\nSELECT 7"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, size, sum(sales) FROM items_sold GROUP BY brand, ROLLUP (size)"
        ),
        "brand|size|sum\nBar|L|5\nBar|M|15\nBar|NULL|20\nFoo|L|10\nFoo|M|20\nFoo|NULL|30\nSELECT 6"
    );
}

#[test]
fn cube_groups_by_every_subset_of_its_keys() {
    let store = MemoryStore::default();
    let mut session = with_items_sold(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, size, sum(sales) FROM items_sold GROUP BY CUBE (brand, size) ORDER BY brand, size"
        ),
        "brand|size|sum\nBar|L|5\nBar|M|15\nBar|NULL|20\nFoo|L|10\nFoo|M|20\nFoo|NULL|30\n\
         NULL|L|15\nNULL|M|35\nNULL|NULL|50\nSELECT 9"
    );
}

#[test]
fn grouping_returns_a_bit_for_each_key_left_out_of_the_row_set() {
    let store = MemoryStore::default();
    let mut session = with_items_sold(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, size, GROUPING(brand, size) AS g, sum(sales) FROM items_sold \
             GROUP BY CUBE (brand, size) ORDER BY g, brand, size"
        ),
        "brand|size|g|sum\nBar|L|0|5\nBar|M|0|15\nFoo|L|0|10\nFoo|M|0|20\nBar|NULL|1|20\n\
         Foo|NULL|1|30\nNULL|L|2|15\nNULL|M|2|35\nNULL|NULL|3|50\nSELECT 9"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT CASE WHEN GROUPING(brand) = 1 THEN 'all' ELSE brand END AS brand, sum(sales) \
             FROM items_sold GROUP BY ROLLUP (brand)"
        ),
        "brand|sum\nBar|20\nFoo|30\nall|50\nSELECT 3"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT sum(sales) FROM items_sold GROUP BY ROLLUP (brand) HAVING GROUPING(brand) = 1"
        ),
        "sum\n50\nSELECT 1"
    );
}

#[test]
fn grouping_tells_a_null_key_from_a_rolled_up_one() {
    let store = MemoryStore::default();
    let mut session = with_items_sold(&store);
    last(&mut session, "INSERT INTO items_sold VALUES (NULL, 'S', 1)");
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, GROUPING(brand) AS g, sum(sales) FROM items_sold \
             GROUP BY ROLLUP (brand) ORDER BY g, brand"
        ),
        "brand|g|sum\nBar|0|20\nFoo|0|30\nNULL|0|1\nNULL|1|51\nSELECT 4"
    );
}

#[test]
fn the_empty_grouping_set_returns_its_row_over_no_rows() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE empty_sales (brand TEXT, sales INT)",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, count(*) FROM empty_sales GROUP BY ROLLUP (brand)"
        ),
        "brand|count\nNULL|0\nSELECT 1"
    );
    assert_eq!(
        last(&mut session, "SELECT count(*) FROM empty_sales GROUP BY ()"),
        "count\n0\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, count(*) FROM empty_sales GROUP BY GROUPING SETS ((brand))"
        ),
        "brand|count\nSELECT 0"
    );
}

#[test]
fn group_by_distinct_removes_repeated_grouping_sets() {
    let store = MemoryStore::default();
    let mut session = with_items_sold(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM (SELECT brand FROM items_sold \
             GROUP BY GROUPING SETS ((brand), (brand))) AS g"
        ),
        "count\n4\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT count(*) FROM (SELECT brand FROM items_sold \
             GROUP BY DISTINCT GROUPING SETS ((brand), (brand))) AS g"
        ),
        "count\n2\nSELECT 1"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, sum(sales) FROM items_sold GROUP BY ALL brand ORDER BY brand"
        ),
        "brand|sum\nBar|20\nFoo|30\nSELECT 2"
    );
}

#[test]
fn nested_grouping_sets_and_parenthesized_keys_expand_as_the_standard_says() {
    let store = MemoryStore::default();
    let mut session = with_items_sold(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, size, sum(sales) FROM items_sold \
             GROUP BY GROUPING SETS (ROLLUP (brand), (brand, size)) ORDER BY brand, size"
        ),
        "brand|size|sum\nBar|L|5\nBar|M|15\nBar|NULL|20\nFoo|L|10\nFoo|M|20\nFoo|NULL|30\n\
         NULL|NULL|50\nSELECT 7"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT brand, size, sum(sales) FROM items_sold GROUP BY ROLLUP ((brand, size))"
        ),
        "brand|size|sum\nBar|L|5\nBar|M|15\nFoo|L|10\nFoo|M|20\nNULL|NULL|50\nSELECT 5"
    );
}

#[test]
fn grouping_sets_keep_postgresql_errors() {
    let store = MemoryStore::default();
    let mut session = with_items_sold(&store);
    assert_eq!(
        refused(
            &mut session,
            "SELECT GROUPING(sales) FROM items_sold GROUP BY ROLLUP (brand)"
        ),
        "arguments to GROUPING must be grouping expressions of the associated query level"
    );
    assert!(
        refused(
            &mut session,
            "SELECT size, sum(sales) FROM items_sold GROUP BY ROLLUP (brand)"
        )
        .contains("must appear in the GROUP BY clause"),
    );
    assert!(
        refused(
            &mut session,
            "SELECT brand FROM items_sold WHERE GROUPING(brand) = 0 GROUP BY ROLLUP (brand)"
        )
        .contains("not allowed in WHERE"),
    );
    let thirteen = ["brand"; 13].join(", ");
    assert_eq!(
        refused(
            &mut session,
            &format!("SELECT count(*) FROM items_sold GROUP BY CUBE ({thirteen})")
        ),
        "CUBE is limited to 12 elements"
    );
    let twelve = ["brand"; 12].join(", ");
    assert_eq!(
        refused(
            &mut session,
            &format!(
                "SELECT count(*) FROM items_sold GROUP BY CUBE ({twelve}), CUBE (size, sales)"
            )
        ),
        "too many grouping sets present (maximum 4096)"
    );
    let nested = format!(
        "SELECT count(*) FROM items_sold GROUP BY {}brand{}",
        "GROUPING SETS (".repeat(100),
        ")".repeat(100)
    );
    assert_eq!(refused(&mut session, &nested), "stack depth limit exceeded");
}

#[test]
fn a_rolled_up_primary_key_no_longer_determines_its_columns() {
    let store = MemoryStore::default();
    let mut session = Session::new(&store);
    last(
        &mut session,
        "CREATE TABLE brands (id INT PRIMARY KEY, name TEXT); \
         INSERT INTO brands VALUES (1, 'Foo'), (2, 'Bar')",
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT id, name, count(*) FROM brands GROUP BY GROUPING SETS ((id)) ORDER BY id"
        ),
        "id|name|count\n1|Foo|1\n2|Bar|1\nSELECT 2"
    );
    assert!(
        refused(
            &mut session,
            "SELECT id, name, count(*) FROM brands GROUP BY ROLLUP (id)"
        )
        .contains("must appear in the GROUP BY clause"),
    );
}

fn mysql_sales(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    last(
        &mut session,
        "CREATE TABLE sales (year INT, country VARCHAR(20), product VARCHAR(32), profit INT); \
         INSERT INTO sales VALUES \
         (2000, 'Finland', 'Computer', 1500), (2000, 'Finland', 'Phone', 100), \
         (2001, 'Finland', 'Phone', 10), (2000, 'India', 'Calculator', 75), \
         (2000, 'India', 'Calculator', 75), (2000, 'India', 'Computer', 1200), \
         (2000, 'USA', 'Calculator', 75), (2000, 'USA', 'Computer', 1500), \
         (2001, 'USA', 'Calculator', 50), (2001, 'USA', 'Computer', 1500), \
         (2001, 'USA', 'Computer', 1200), (2001, 'USA', 'TV', 150), (2001, 'USA', 'TV', 100)",
    );
    session
}

#[test]
fn mysql_with_rollup_adds_the_super_aggregate_rows_of_its_manual() {
    let store = MemoryStore::default();
    let mut session = mysql_sales(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT year, SUM(profit) AS profit FROM sales GROUP BY year WITH ROLLUP"
        ),
        "year|profit\n2000|4525\n2001|3010\nNULL|7535\nSELECT 3"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT year, country, SUM(profit) AS profit FROM sales GROUP BY year, country WITH ROLLUP"
        ),
        "year|country|profit\n2000|Finland|1600\n2000|India|1350\n2000|USA|1575\n2000|NULL|4525\n\
         2001|Finland|10\n2001|USA|3000\n2001|NULL|3010\nNULL|NULL|7535\nSELECT 8"
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT year, GROUPING(year) AS g, SUM(profit) AS profit FROM sales GROUP BY year WITH ROLLUP"
        ),
        "year|g|profit\n2000|0|4525\n2001|0|3010\nNULL|1|7535\nSELECT 3"
    );
}

#[test]
fn mysql_and_sqlite_refuse_the_standard_grouping_constructs() {
    let store = MemoryStore::default();
    let mut session = mysql_sales(&store);
    assert!(
        run(
            &mut session,
            "SELECT year FROM sales GROUP BY ROLLUP (year)"
        )
        .is_err()
    );
    let sqlite_store = MemoryStore::default();
    let mut sqlite = Session::with_dialect(&sqlite_store, Dialect::Sqlite);
    last(&mut sqlite, "CREATE TABLE t (a INTEGER)");
    assert!(run(&mut sqlite, "SELECT a FROM t GROUP BY GROUPING SETS ((a))").is_err());
}

fn sqlserver_sales(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    last(
        &mut session,
        "CREATE TABLE sales (country VARCHAR(50), region VARCHAR(50), amount INT); \
         INSERT INTO sales VALUES (N'Canada', N'Alberta', 100), (N'Canada', N'British Columbia', 200), \
         (N'Canada', N'British Columbia', 300), (N'United States', N'Montana', 100)",
    );
    session
}

#[test]
fn sqlserver_rollup_returns_the_rows_of_its_documentation() {
    let store = MemoryStore::default();
    let mut session = sqlserver_sales(&store);
    let expected = "country|region|total\nCanada|Alberta|100\nCanada|British Columbia|500\n\
                    Canada|NULL|600\nUnited States|Montana|100\nUnited States|NULL|100\n\
                    NULL|NULL|700\nSELECT 6";
    assert_eq!(
        last(
            &mut session,
            "SELECT country, region, SUM(amount) AS total FROM sales GROUP BY ROLLUP (country, region)"
        ),
        expected
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT country, region, SUM(amount) AS total FROM sales GROUP BY country, region WITH ROLLUP"
        ),
        expected
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT SUM(amount) AS total FROM sales GROUP BY ()"
        ),
        "total\n700\nSELECT 1"
    );
}

#[test]
fn sqlserver_cube_and_grouping_id_number_each_grouping_set() {
    let store = MemoryStore::default();
    let mut session = sqlserver_sales(&store);
    let expected = "country|region|grouping_level|total\nCanada|Alberta|0|100\n\
                    Canada|British Columbia|0|500\nUnited States|Montana|0|100\n\
                    Canada|NULL|1|600\nUnited States|NULL|1|100\nNULL|Alberta|2|100\n\
                    NULL|British Columbia|2|500\nNULL|Montana|2|100\nNULL|NULL|3|700\nSELECT 9";
    assert_eq!(
        last(
            &mut session,
            "SELECT country, region, GROUPING_ID(country, region) AS grouping_level, SUM(amount) AS total \
             FROM sales GROUP BY CUBE (country, region) ORDER BY grouping_level, country, region"
        ),
        expected
    );
    assert_eq!(
        last(
            &mut session,
            "SELECT country, region, GROUPING_ID(country, region) AS grouping_level, SUM(amount) AS total \
             FROM sales GROUP BY country, region WITH CUBE ORDER BY grouping_level, country, region"
        ),
        expected
    );
}

fn oracle_emp(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    last(
        &mut session,
        "CREATE TABLE emp (dept NUMBER(4), job VARCHAR2(10), sal NUMBER(8))",
    );
    last(&mut session, "INSERT INTO emp VALUES (10, 'CLERK', 100)");
    last(&mut session, "INSERT INTO emp VALUES (10, 'MANAGER', 300)");
    last(&mut session, "INSERT INTO emp VALUES (20, 'CLERK', 50)");
    session
}

#[test]
fn oracle_group_id_numbers_repeated_grouping_sets() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT dept, job, SUM(sal) AS total, GROUP_ID() AS g FROM emp \
             GROUP BY dept, ROLLUP (dept, job) ORDER BY dept, g, GROUPING(job), job"
        ),
        "DEPT|JOB|TOTAL|G\n10|CLERK|100|0\n10|MANAGER|300|0\n10|NULL|400|0\n10|NULL|400|1\n\
         20|CLERK|50|0\n20|NULL|50|0\n20|NULL|50|1\nSELECT 7"
    );
}

#[test]
fn oracle_grouping_id_and_cube() {
    let store = MemoryStore::default();
    let mut session = oracle_emp(&store);
    assert_eq!(
        last(
            &mut session,
            "SELECT dept, job, GROUPING_ID(dept, job) AS gid, SUM(sal) AS total FROM emp \
             GROUP BY CUBE (dept, job) ORDER BY gid, dept, job"
        ),
        "DEPT|JOB|GID|TOTAL\n10|CLERK|0|100\n10|MANAGER|0|300\n20|CLERK|0|50\n10|NULL|1|400\n\
         20|NULL|1|50\nNULL|CLERK|2|150\nNULL|MANAGER|2|300\nNULL|NULL|3|450\nSELECT 8"
    );
}
