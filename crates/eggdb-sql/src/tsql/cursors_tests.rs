use super::cursors::{CursorMove, CursorState, OpenCursor, cursor_status};
use crate::types::Value;

fn three_rows() -> OpenCursor {
    OpenCursor::new(
        Vec::new(),
        (1..=3).map(|id| vec![Value::Integer(id)]).collect(),
    )
}

fn fetched(cursor: &mut OpenCursor, step: CursorMove) -> Option<i32> {
    match cursor.fetch(step)?.first()? {
        Value::Integer(id) => Some(*id),
        _ => None,
    }
}

#[test]
fn next_runs_off_the_end_and_prior_comes_back_to_the_last_row() {
    let mut cursor = three_rows();
    assert_eq!(fetched(&mut cursor, CursorMove::Next), Some(1));
    assert_eq!(fetched(&mut cursor, CursorMove::Next), Some(2));
    assert_eq!(fetched(&mut cursor, CursorMove::Next), Some(3));
    assert_eq!(fetched(&mut cursor, CursorMove::Next), None);
    assert_eq!(fetched(&mut cursor, CursorMove::Next), None);
    assert_eq!(fetched(&mut cursor, CursorMove::Prior), Some(3));
}

#[test]
fn absolute_and_relative_count_from_either_end_and_stop_outside_the_rows() {
    let mut cursor = three_rows();
    assert_eq!(fetched(&mut cursor, CursorMove::Absolute(-1)), Some(3));
    assert_eq!(fetched(&mut cursor, CursorMove::Absolute(2)), Some(2));
    assert_eq!(fetched(&mut cursor, CursorMove::Relative(0)), Some(2));
    assert_eq!(fetched(&mut cursor, CursorMove::Relative(-5)), None);
    assert_eq!(fetched(&mut cursor, CursorMove::Next), Some(1));
    assert_eq!(fetched(&mut cursor, CursorMove::Absolute(0)), None);
    assert_eq!(fetched(&mut cursor, CursorMove::Absolute(9)), None);
    assert_eq!(fetched(&mut cursor, CursorMove::Absolute(i64::MIN)), None);
    assert_eq!(fetched(&mut cursor, CursorMove::Relative(i64::MAX)), None);
    assert_eq!(fetched(&mut cursor, CursorMove::Last), Some(3));
}

#[test]
fn an_empty_cursor_fetches_nothing_in_any_direction() {
    let mut cursor = OpenCursor::new(Vec::new(), Vec::new());
    for step in [
        CursorMove::Next,
        CursorMove::Prior,
        CursorMove::First,
        CursorMove::Last,
        CursorMove::Absolute(1),
        CursorMove::Relative(-1),
    ] {
        assert_eq!(fetched(&mut cursor, step), None);
    }
}

#[test]
fn cursor_status_finds_local_cursors_only_at_their_nesting_level() {
    let states = vec![
        CursorState {
            name: "c".to_owned(),
            global: false,
            nest_level: 1,
            status: 1,
        },
        CursorState {
            name: "g".to_owned(),
            global: true,
            nest_level: 0,
            status: -1,
        },
    ];
    assert_eq!(cursor_status(&states, "local", "C", 1), Some(1));
    assert_eq!(cursor_status(&states, "local", "c", 0), Some(-3));
    assert_eq!(cursor_status(&states, "GLOBAL", "g", 3), Some(-1));
    assert_eq!(cursor_status(&states, "variable", "g", 0), Some(-3));
    assert_eq!(cursor_status(&states, "elsewhere", "g", 0), None);
}
