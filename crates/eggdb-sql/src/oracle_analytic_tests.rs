use crate::memory_store::MemoryStore;
use crate::{Dialect, QueryResult, Session, SqlError};

fn rows(session: &mut Session<'_>, text: &str) -> Vec<String> {
    let mut last = None;
    session
        .execute(text, &mut |result: QueryResult| last = Some(result.rows))
        .unwrap_or_else(|error| panic!("{text}: {error}"));
    last.unwrap_or_default()
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| {
                    Dialect::Oracle
                        .value_text(value)
                        .unwrap_or_else(|| "NULL".to_owned())
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn run(session: &mut Session<'_>, text: &str) {
    session
        .execute(text, &mut |_| {})
        .unwrap_or_else(|error| panic!("{text}: {error}"));
}

fn failure(session: &mut Session<'_>, text: &str) -> SqlError {
    session
        .execute(text, &mut |_| {})
        .expect_err("the statement fails")
}

const EMPLOYEES: [&str; 9] = [
    "(100, 'King', 90, 24000, NULL)",
    "(101, 'Kochhar', 90, 17000, NULL)",
    "(102, 'De Haan', 90, 17000, NULL)",
    "(103, 'Hunold', 60, 9000, NULL)",
    "(104, 'Ernst', 60, 6000, NULL)",
    "(105, 'Austin', 60, 4800, NULL)",
    "(145, 'Russell', 80, 14000, .4)",
    "(146, 'Partners', 80, 13500, .3)",
    "(147, 'Errazuriz', 80, 12000, .3)",
];

fn employees(store: &MemoryStore) -> Session<'_> {
    let mut session = Session::with_dialect(store, Dialect::Oracle);
    run(
        &mut session,
        "CREATE TABLE employees (employee_id NUMBER PRIMARY KEY, last_name VARCHAR2(20), \
         department_id NUMBER, salary NUMBER, commission_pct NUMBER)",
    );
    for row in EMPLOYEES {
        run(&mut session, &format!("INSERT INTO employees VALUES {row}"));
    }
    session
}

#[test]
fn ranking_functions_number_the_rows_of_each_department() {
    let store = MemoryStore::default();
    let mut session = employees(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT last_name, RANK() OVER (PARTITION BY department_id ORDER BY salary DESC) r, \
             DENSE_RANK() OVER (PARTITION BY department_id ORDER BY salary DESC) d, \
             ROW_NUMBER() OVER (PARTITION BY department_id ORDER BY salary DESC, employee_id) n \
             FROM employees WHERE department_id = 90 ORDER BY employee_id"
        ),
        ["King|1|1|1", "Kochhar|2|2|2", "De Haan|2|2|3"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT NTILE(2) OVER (ORDER BY salary DESC) bucket, \
             PERCENT_RANK() OVER (ORDER BY salary) pr FROM employees \
             WHERE department_id = 60 ORDER BY salary DESC"
        ),
        ["1|1", "1|.5", "2|0"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT CUME_DIST() OVER (ORDER BY salary) cd FROM employees \
             WHERE department_id = 90 OR employee_id = 103 ORDER BY salary, employee_id"
        ),
        [".25", ".75", ".75", "1"]
    );
}

#[test]
fn lag_lead_and_the_value_functions_read_neighbours_and_skip_nulls_when_asked() {
    let store = MemoryStore::default();
    let mut session = employees(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT last_name, LAG(salary, 1, 0) OVER (ORDER BY employee_id) prev, \
             LEAD(salary) OVER (ORDER BY employee_id) next FROM employees \
             WHERE department_id = 60 ORDER BY employee_id"
        ),
        ["Hunold|0|6000", "Ernst|9000|4800", "Austin|6000|NULL"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT employee_id, LAG(commission_pct IGNORE NULLS) OVER (ORDER BY employee_id) a, \
             LAG(commission_pct) IGNORE NULLS OVER (ORDER BY employee_id) b, \
             LAG(commission_pct, 2) RESPECT NULLS OVER (ORDER BY employee_id) c \
             FROM employees WHERE employee_id >= 105 ORDER BY employee_id"
        ),
        [
            "105|NULL|NULL|NULL",
            "145|NULL|NULL|NULL",
            "146|.4|.4|NULL",
            "147|.3|.3|.4"
        ]
    );
    let order = "OVER (ORDER BY employee_id)";
    for (negative, positive) in [
        (
            "LAG(commission_pct, -1) IGNORE NULLS",
            "LEAD(commission_pct) IGNORE NULLS",
        ),
        (
            "LEAD(commission_pct, -1) IGNORE NULLS",
            "LAG(commission_pct) IGNORE NULLS",
        ),
    ] {
        let read = |session: &mut Session<'_>, call: &str| {
            rows(
                session,
                &format!(
                    "SELECT employee_id, {call} {order} v FROM employees \
                     WHERE employee_id >= 100 ORDER BY employee_id"
                ),
            )
        };
        let flipped = read(&mut session, negative);
        assert_eq!(flipped, read(&mut session, positive), "{negative}");
        assert!(
            flipped.iter().any(|row| !row.ends_with("|NULL")),
            "{flipped:?}"
        );
    }
    let whole =
        "OVER (ORDER BY employee_id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)";
    assert_eq!(
        rows(
            &mut session,
            &format!(
                "SELECT FIRST_VALUE(commission_pct) {whole} f, \
                 FIRST_VALUE(commission_pct IGNORE NULLS) {whole} fi, \
                 LAST_VALUE(commission_pct) IGNORE NULLS {whole} li \
                 FROM employees WHERE employee_id IN (105, 145, 147) ORDER BY employee_id"
            )
        ),
        ["NULL|.4|.3"; 3]
    );
    let department = "OVER (PARTITION BY department_id ORDER BY salary \
                      ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)";
    assert_eq!(
        rows(
            &mut session,
            &format!(
                "SELECT last_name, NTH_VALUE(salary, 1) FROM FIRST {department} lowest, \
                 NTH_VALUE(salary, 1) FROM LAST {department} highest, \
                 NTH_VALUE(commission_pct, 1) FROM LAST IGNORE NULLS {department} kept \
                 FROM employees WHERE department_id = 60 ORDER BY employee_id"
            )
        ),
        [
            "Hunold|4800|9000|NULL",
            "Ernst|4800|9000|NULL",
            "Austin|4800|9000|NULL"
        ]
    );
}

#[test]
fn aggregates_over_windows_and_ratio_to_report_share_each_partition() {
    let store = MemoryStore::default();
    let mut session = employees(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT last_name, SUM(salary) OVER (PARTITION BY department_id) total, \
             AVG(salary) OVER (PARTITION BY department_id) average, \
             SUM(salary) OVER (ORDER BY employee_id ROWS UNBOUNDED PRECEDING) running, \
             COUNT(*) OVER () everyone FROM employees WHERE department_id = 60 \
             ORDER BY employee_id"
        ),
        [
            "Hunold|19800|6600|9000|3",
            "Ernst|19800|6600|15000|3",
            "Austin|19800|6600|19800|3"
        ]
    );
    run(
        &mut session,
        "CREATE TABLE sales (id NUMBER, region VARCHAR2(1), amount NUMBER)",
    );
    for row in [
        "(1, 'E', 1000)",
        "(2, 'E', 3000)",
        "(3, 'W', 500)",
        "(4, 'W', 500)",
        "(5, 'W', NULL)",
        "(6, 'Z', 0)",
    ] {
        run(&mut session, &format!("INSERT INTO sales VALUES {row}"));
    }
    assert_eq!(
        rows(
            &mut session,
            "SELECT id, RATIO_TO_REPORT(amount) OVER (PARTITION BY region) ratio \
             FROM sales ORDER BY id"
        ),
        ["1|.25", "2|.75", "3|.5", "4|.5", "5|NULL", "6|NULL"]
    );
    assert_eq!(
        failure(
            &mut session,
            "SELECT RATIO_TO_REPORT(amount) OVER (ORDER BY id) FROM sales"
        )
        .vendor_code(),
        Some(30487)
    );
}

#[test]
fn keep_dense_rank_first_and_last_aggregate_the_rows_that_rank_at_either_end() {
    let store = MemoryStore::default();
    let mut session = employees(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT department_id, \
             MIN(salary) KEEP (DENSE_RANK FIRST ORDER BY commission_pct) \"Worst\", \
             MAX(salary) KEEP (DENSE_RANK LAST ORDER BY commission_pct) \"Best\", \
             COUNT(*) KEEP (DENSE_RANK FIRST ORDER BY commission_pct) \"Tied\" \
             FROM employees GROUP BY department_id ORDER BY department_id"
        ),
        ["60|4800|9000|3", "80|12000|14000|2", "90|17000|24000|3"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT last_name, MIN(salary) KEEP (DENSE_RANK FIRST ORDER BY commission_pct) \
             OVER (PARTITION BY department_id) \"Worst\" FROM employees \
             WHERE department_id = 80 ORDER BY employee_id"
        ),
        ["Russell|12000", "Partners|12000", "Errazuriz|12000"]
    );
}

#[test]
fn keep_over_a_window_ranks_each_partition_once_in_linear_memory() {
    let store = MemoryStore::default();
    let mut session = employees(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT last_name, \
             COUNT(*) KEEP (DENSE_RANK LAST ORDER BY commission_pct) OVER () \"Untied\", \
             SUM(salary) KEEP (DENSE_RANK FIRST ORDER BY commission_pct DESC) \
             OVER (PARTITION BY department_id) \"Top\" \
             FROM employees WHERE department_id IN (60, 80) ORDER BY employee_id"
        ),
        [
            "Hunold|3|19800",
            "Ernst|3|19800",
            "Austin|3|19800",
            "Russell|3|14000",
            "Partners|3|14000",
            "Errazuriz|3|14000"
        ]
    );
    run(&mut session, "CREATE TABLE big (n NUMBER)");
    run(
        &mut session,
        "INSERT INTO big SELECT LEVEL FROM dual CONNECT BY LEVEL <= 20000",
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT MIN(m), MAX(m), COUNT(*) FROM (SELECT \
             MAX(n) KEEP (DENSE_RANK FIRST ORDER BY MOD(n, 1000)) OVER (PARTITION BY MOD(n, 2)) m \
             FROM big)"
        ),
        ["19001|20000|20000"]
    );
}

#[test]
fn window_functions_need_oracles_window_specification() {
    let store = MemoryStore::default();
    let mut session = employees(&store);
    let code = |session: &mut Session<'_>, text: &str| failure(session, text).vendor_code();
    assert_eq!(
        code(
            &mut session,
            "SELECT ROW_NUMBER() OVER (PARTITION BY department_id) FROM employees"
        ),
        Some(30485)
    );
    assert_eq!(
        code(&mut session, "SELECT RANK() FROM employees"),
        Some(30484)
    );
    assert_eq!(
        code(
            &mut session,
            "SELECT NTILE() OVER (ORDER BY salary) FROM employees"
        ),
        Some(909)
    );
}

#[test]
fn regular_expression_functions_take_oracles_arguments() {
    let store = MemoryStore::default();
    let mut session = employees(&store);
    assert_eq!(
        rows(
            &mut session,
            "SELECT last_name FROM employees WHERE REGEXP_LIKE(last_name, '^k', 'i') \
             ORDER BY employee_id"
        ),
        ["King", "Kochhar"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT REGEXP_SUBSTR('http://www.example.com/products', \
             'http://([[:alnum:]]+\\.?){3,4}/?') url, \
             REGEXP_COUNT('123123123123123', '(12)3', 1, 'i') n, \
             REGEXP_INSTR('500 Oracle Parkway, Redwood Shores, CA', '[^ ]+', 1, 6) at6, \
             REGEXP_REPLACE('500   Oracle     Parkway', '( ){2,}', ' ') squeezed FROM dual"
        ),
        ["http://www.example.com/|5|37|500 Oracle Parkway"]
    );
    assert_eq!(
        rows(
            &mut session,
            "SELECT REGEXP_SUBSTR('abc', 'x') none, REGEXP_REPLACE('aaa', 'a') emptied FROM dual"
        ),
        ["NULL|NULL"]
    );
    assert_eq!(
        failure(
            &mut session,
            "SELECT REGEXP_SUBSTR('abc', 'b', 0) FROM dual"
        )
        .vendor_code(),
        Some(1428)
    );
}
