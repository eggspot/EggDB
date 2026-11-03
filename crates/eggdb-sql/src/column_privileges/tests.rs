use super::*;
use crate::error::SqlState;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

fn staff(label: &str) -> ScopeItem {
    ScopeItem::relation(
        label.to_owned(),
        "staff".to_owned(),
        names(&["id", "name", "salary"]),
        names(&["salary"]),
    )
}

fn teams(label: &str) -> ScopeItem {
    ScopeItem::relation(
        label.to_owned(),
        "teams".to_owned(),
        names(&["id", "salary"]),
        Vec::new(),
    )
}

fn scopes(levels: Vec<Vec<ScopeItem>>) -> ColumnScopes {
    let mut scopes = ColumnScopes::default();
    for level in levels {
        scopes.open();
        for item in level {
            scopes.add(item);
        }
    }
    scopes
}

fn is_denied(result: SqlResult<()>) -> bool {
    result.is_err_and(|error| error.state() == SqlState::INSUFFICIENT_PRIVILEGE)
}

#[test]
fn a_column_binds_to_the_innermost_entry_that_has_it() {
    let inner_teams = scopes(vec![vec![staff("staff")], vec![teams("teams")]]);
    assert!(inner_teams.check_column(None, "salary").is_ok());
    assert!(inner_teams.check_column(None, "name").is_ok());
    assert!(is_denied(inner_teams.check_column(Some("staff"), "salary")));
    assert!(inner_teams.check_column(Some("teams"), "salary").is_ok());
    let inner_staff = scopes(vec![vec![teams("teams")], vec![staff("s")]]);
    assert!(is_denied(inner_staff.check_column(None, "salary")));
    assert!(is_denied(inner_staff.check_column(Some("s"), "salary")));
    assert!(inner_staff.check_column(Some("s"), "name").is_ok());
}

#[test]
fn an_alias_hides_the_table_name_and_a_same_named_column_elsewhere_still_denies() {
    let joined = scopes(vec![vec![staff("s"), teams("t")]]);
    assert!(is_denied(joined.check_column(None, "salary")));
    assert!(joined.check_column(Some("t"), "salary").is_ok());
    assert!(joined.check_column(Some("staff"), "salary").is_ok());
    let shadowed = scopes(vec![vec![staff("staff")], vec![teams("staff")]]);
    assert!(shadowed.check_column(Some("staff"), "salary").is_ok());
    assert!(is_denied(
        scopes(vec![vec![staff("x")], vec![teams("t")]]).check_column(Some("public.x"), "salary")
    ));
}

#[test]
fn opaque_entries_never_stop_the_search_for_an_unqualified_column() {
    let correlated = scopes(vec![
        vec![staff("staff")],
        vec![ScopeItem::opaque(Some("d".to_owned()))],
    ]);
    assert!(is_denied(correlated.check_column(None, "salary")));
    assert!(correlated.check_column(Some("d"), "salary").is_ok());
}

#[test]
fn wildcards_and_whole_rows_need_every_column() {
    let joined = scopes(vec![vec![staff("s"), teams("t")]]);
    assert!(is_denied(joined.check_wildcard(None, true)));
    assert!(is_denied(joined.check_wildcard(Some("s"), true)));
    assert!(joined.check_wildcard(Some("t"), true).is_ok());
    assert!(is_denied(joined.check_column(Some("s"), "*")));
    assert!(is_denied(joined.check_column(None, "s")));
    assert!(joined.check_column(None, "t").is_ok());
    let outer_only = scopes(vec![vec![staff("s")], vec![teams("t")]]);
    assert!(outer_only.check_wildcard(None, true).is_ok());
}

#[test]
fn join_using_reads_the_column_of_every_entry() {
    let joined = scopes(vec![vec![staff("s"), teams("t")]]);
    let using = |column: &str| {
        vec![Name {
            text: column.to_owned(),
            position: 0,
        }]
    };
    assert!(is_denied(joined.check_using(&using("salary"))));
    assert!(joined.check_using(&using("id")).is_ok());
}

#[test]
fn suspended_scopes_check_nothing_until_resumed() {
    let mut nested = scopes(vec![vec![staff("staff")]]);
    assert!(nested.has_unreadable());
    let outer = nested.suspend();
    assert!(!nested.has_unreadable());
    assert!(nested.check_column(None, "salary").is_ok());
    nested.resume(outer);
    assert!(is_denied(nested.check_column(None, "salary")));
    nested.close();
    assert!(nested.check_column(None, "salary").is_ok());
}

#[test]
fn unreadable_columns_keep_table_order() {
    let reach = crate::roles::ColumnReach::Only(["name".to_owned()].into_iter().collect());
    assert_eq!(
        unreadable_columns(&names(&["id", "name", "salary"]), &reach),
        names(&["id", "salary"])
    );
    assert!(unreadable_columns(&names(&["id"]), &crate::roles::ColumnReach::Every).is_empty());
}
