use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

fn results(session: &mut Session<'_>, text: &str) -> Vec<QueryResult> {
    let mut results = Vec::new();
    session
        .execute(text, &mut |result| results.push(result))
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    results
}

fn last(session: &mut Session<'_>, text: &str) -> QueryResult {
    results(session, text)
        .pop()
        .unwrap_or_else(|| panic!("{text}: no result"))
}

fn lines(session: &mut Session<'_>, text: &str) -> Vec<String> {
    last(session, text)
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| Dialect::Oracle.value_text(value).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn error(session: &mut Session<'_>, text: &str) -> SqlError {
    session.execute(text, &mut |_| {}).expect_err(text)
}

const EMPLOYEES: [(u32, &str, Option<u32>, u32); 8] = [
    (100, "King", None, 90),
    (101, "Kochhar", Some(100), 90),
    (102, "De Haan", Some(100), 90),
    (103, "Hunold", Some(102), 60),
    (104, "Ernst", Some(103), 60),
    (107, "Lorentz", Some(103), 60),
    (108, "Greenberg", Some(101), 100),
    (109, "Faviet", Some(108), 100),
];

fn hr_employees(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    results(
        &mut session,
        "CREATE TABLE employees (employee_id NUMBER PRIMARY KEY, last_name VARCHAR2(25), manager_id NUMBER, department_id NUMBER)",
    );
    for (id, name, manager, department) in EMPLOYEES {
        let manager = manager.map_or_else(|| "NULL".to_owned(), |manager| manager.to_string());
        results(
            &mut session,
            &format!("INSERT INTO employees VALUES ({id}, '{name}', {manager}, {department})"),
        );
    }
    results(&mut session, "COMMIT");
    session
}

#[test]
fn connect_by_walks_the_hierarchy_depth_first_with_level_and_path() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    assert_eq!(
        lines(
            &mut session,
            "SELECT employee_id, last_name, LEVEL, SYS_CONNECT_BY_PATH(last_name, '/') AS path \
             FROM employees START WITH manager_id IS NULL \
             CONNECT BY PRIOR employee_id = manager_id ORDER SIBLINGS BY last_name"
        ),
        vec![
            "100|King|1|/King",
            "102|De Haan|2|/King/De Haan",
            "103|Hunold|3|/King/De Haan/Hunold",
            "104|Ernst|4|/King/De Haan/Hunold/Ernst",
            "107|Lorentz|4|/King/De Haan/Hunold/Lorentz",
            "101|Kochhar|2|/King/Kochhar",
            "108|Greenberg|3|/King/Kochhar/Greenberg",
            "109|Faviet|4|/King/Kochhar/Greenberg/Faviet",
        ]
    );
}

#[test]
fn order_siblings_by_descending_reverses_each_level_only() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    assert_eq!(
        lines(
            &mut session,
            "SELECT last_name FROM employees CONNECT BY PRIOR employee_id = manager_id \
             START WITH employee_id = 100 ORDER SIBLINGS BY last_name DESC"
        ),
        vec![
            "King",
            "Kochhar",
            "Greenberg",
            "Faviet",
            "De Haan",
            "Hunold",
            "Lorentz",
            "Ernst",
        ]
    );
}

#[test]
fn where_filters_after_the_walk_and_root_leaf_and_prior_read_the_path() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    assert_eq!(
        lines(
            &mut session,
            "SELECT last_name, CONNECT_BY_ROOT last_name AS boss, CONNECT_BY_ISLEAF AS leaf, \
             PRIOR last_name AS manager FROM employees e WHERE e.department_id = 60 \
             START WITH e.employee_id = 102 CONNECT BY PRIOR e.employee_id = e.manager_id \
             ORDER BY last_name"
        ),
        vec![
            "Ernst|De Haan|1|Hunold",
            "Hunold|De Haan|0|De Haan",
            "Lorentz|De Haan|1|Hunold",
        ]
    );
}

#[test]
fn without_start_with_every_row_is_a_root() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    assert_eq!(
        lines(
            &mut session,
            "SELECT COUNT(*) FROM employees CONNECT BY PRIOR employee_id = manager_id"
        ),
        vec!["23"]
    );
}

#[test]
fn group_by_and_order_by_read_level() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    assert_eq!(
        lines(
            &mut session,
            "SELECT LEVEL AS depth, COUNT(*) AS people FROM employees \
             START WITH manager_id IS NULL CONNECT BY PRIOR employee_id = manager_id \
             GROUP BY LEVEL ORDER BY LEVEL"
        ),
        vec!["1|1", "2|2", "3|2", "4|3"]
    );
}

#[test]
fn connect_by_level_counts_rows_from_dual() {
    let store = MemoryStore::default();
    let mut session = Session::with_dialect(&store, Dialect::Oracle);
    assert_eq!(
        lines(&mut session, "SELECT LEVEL FROM dual CONNECT BY LEVEL <= 4"),
        vec!["1", "2", "3", "4"]
    );
}

#[test]
fn a_star_shows_only_the_table_columns() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    let result = last(
        &mut session,
        "SELECT * FROM employees START WITH employee_id = 108 CONNECT BY PRIOR employee_id = manager_id",
    );
    assert_eq!(result.columns.map(|columns| columns.len()), Some(4));
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn a_hierarchical_subquery_feeds_the_outer_query() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    assert_eq!(
        lines(
            &mut session,
            "SELECT COUNT(*) FROM employees WHERE employee_id IN \
             (SELECT employee_id FROM employees START WITH employee_id = 101 \
             CONNECT BY PRIOR employee_id = manager_id)"
        ),
        vec!["3"]
    );
}

#[test]
fn a_loop_fails_without_nocycle_and_is_marked_with_it() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    results(
        &mut session,
        "UPDATE employees SET manager_id = 109 WHERE employee_id = 100",
    );
    results(&mut session, "COMMIT");
    let failed = error(
        &mut session,
        "SELECT last_name FROM employees START WITH employee_id = 100 \
         CONNECT BY PRIOR employee_id = manager_id",
    );
    assert_eq!(failed.vendor_code(), Some(1436));
    assert_eq!(
        lines(
            &mut session,
            "SELECT last_name, CONNECT_BY_ISCYCLE AS looped FROM employees \
             START WITH employee_id = 100 CONNECT BY NOCYCLE PRIOR employee_id = manager_id \
             ORDER SIBLINGS BY last_name"
        ),
        vec![
            "King|0",
            "De Haan|0",
            "Hunold|0",
            "Ernst|0",
            "Lorentz|0",
            "Kochhar|0",
            "Greenberg|0",
            "Faviet|1",
        ]
    );
}

#[test]
fn hierarchical_operators_are_refused_where_oracle_refuses_them() {
    let store = MemoryStore::default();
    let mut session = hr_employees(&store);
    let cases = [
        (
            "SELECT CONNECT_BY_ISCYCLE FROM employees CONNECT BY PRIOR employee_id = manager_id",
            30930,
        ),
        ("SELECT LEVEL FROM employees", 1788),
        ("SELECT last_name FROM employees ORDER SIBLINGS BY last_name", 30929),
        (
            "SELECT SYS_CONNECT_BY_PATH(last_name, ' ') FROM employees \
             START WITH employee_id = 102 CONNECT BY PRIOR employee_id = manager_id",
            30004,
        ),
        (
            "SELECT SYS_CONNECT_BY_PATH(last_name, last_name) FROM employees \
             CONNECT BY PRIOR employee_id = manager_id",
            30003,
        ),
    ];
    for (text, code) in cases {
        assert_eq!(error(&mut session, text).vendor_code(), Some(code), "{text}");
    }
    assert_eq!(
        error(
            &mut session,
            "SELECT e.last_name FROM employees e JOIN employees m ON e.manager_id = m.employee_id \
             CONNECT BY PRIOR e.employee_id = e.manager_id"
        )
        .state()
        .code(),
        "0A000"
    );
}
