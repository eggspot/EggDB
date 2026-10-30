use super::{
    DeclaredType, NO_DATA_FOUND_CODE, PlsqlRoutine, USER_EXCEPTION_CODE, application_error_code,
    predefined_by_code, predefined_by_name, sqlcode_of,
};
use crate::routines::{ParameterMode, RoutineParameter};
use crate::types::SqlType;

fn parameter(name: &str, mode: ParameterMode, sql_type: SqlType) -> RoutineParameter {
    RoutineParameter {
        name: Some(name.to_owned()),
        mode,
        sql_type,
        default: None,
        declared: None,
    }
}

#[test]
fn predefined_exceptions_are_found_by_name_and_by_number() {
    let found = predefined_by_name("no_data_found").expect("NO_DATA_FOUND is predefined");
    assert_eq!(found.code, NO_DATA_FOUND_CODE);
    assert_eq!(found.message, "no data found");
    assert_eq!(predefined_by_name("zero_divide").map(|found| found.code), Some(1476));
    assert_eq!(predefined_by_name("dup_val_on_index").map(|found| found.code), Some(1));
    assert_eq!(
        predefined_by_code(1422).map(|found| found.name),
        Some("too_many_rows")
    );
    assert_eq!(predefined_by_name("NO_DATA_FOUND"), None);
    assert_eq!(predefined_by_name("not_an_exception"), None);
}

#[test]
fn sqlcode_is_negative_except_for_no_data_found_and_user_exceptions() {
    assert_eq!(sqlcode_of(NO_DATA_FOUND_CODE), 100);
    assert_eq!(sqlcode_of(USER_EXCEPTION_CODE), 1);
    assert_eq!(sqlcode_of(1476), -1476);
    assert_eq!(sqlcode_of(20001), -20001);
}

#[test]
fn application_errors_take_numbers_from_minus_20999_to_minus_20000() {
    assert_eq!(application_error_code(-20000), Some(20000));
    assert_eq!(application_error_code(-20999), Some(20999));
    assert_eq!(application_error_code(-20205), Some(20205));
    assert_eq!(application_error_code(-19999), None);
    assert_eq!(application_error_code(-21000), None);
    assert_eq!(application_error_code(20001), None);
    assert_eq!(application_error_code(i64::MIN), None);
}

#[test]
fn declared_types_show_as_oracle_writes_them() {
    assert_eq!(
        DeclaredType::sql(SqlType::Varchar(None), "varchar2").text(),
        "VARCHAR2"
    );
    let column = DeclaredType::ColumnType(vec!["job_history".to_owned(), "employee_id".to_owned()]);
    assert_eq!(column.text(), "JOB_HISTORY.EMPLOYEE_ID%TYPE");
    assert_eq!(column.engine_type(), SqlType::Dynamic);
    assert!(column.needs_resolution());
    assert_eq!(
        DeclaredType::RowType(vec!["employees".to_owned()]).text(),
        "EMPLOYEES%ROWTYPE"
    );
    assert_eq!(
        DeclaredType::Named(vec!["pkg".to_owned(), "Rec".to_owned()]).text(),
        "PKG.Rec"
    );
    assert!(!DeclaredType::sql(SqlType::Integer, "pls_integer").needs_resolution());
}

#[test]
fn identity_lists_the_declared_types_of_inputs_only() {
    let parameters = vec![
        parameter("p_id", ParameterMode::In, SqlType::Dynamic),
        parameter("p_name", ParameterMode::Out, SqlType::Varchar(None)),
        parameter("p_total", ParameterMode::InOut, SqlType::Double),
    ];
    let routine = PlsqlRoutine {
        declared: vec![
            DeclaredType::ColumnType(vec!["emp".to_owned(), "id".to_owned()]),
            DeclaredType::sql(SqlType::Varchar(None), "varchar2"),
            DeclaredType::sql(SqlType::Double, "number"),
        ],
        returns: None,
        package: None,
        deterministic: false,
        pipelined: false,
    };
    assert_eq!(routine.identity(&parameters), "EMP.ID%TYPE, NUMBER");
    assert_eq!(
        routine.declared_type(1).map(DeclaredType::text),
        Some("VARCHAR2".to_owned())
    );
    assert_eq!(routine.declared_type(3), None);
}
