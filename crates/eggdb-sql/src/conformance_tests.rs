const CONFORMANCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/sql-conformance.md"
));
const STATUSES: [&str; 3] = ["done", "partial", "not yet"];

fn feature_rows() -> Vec<Vec<String>> {
    CONFORMANCE
        .lines()
        .filter(|line| line.starts_with("| ") && !line.starts_with("| Feature"))
        .map(|line| {
            line.trim_matches('|')
                .split(" | ")
                .map(|cell| cell.trim().to_owned())
                .collect()
        })
        .collect()
}

#[test]
fn every_feature_has_a_status_and_every_done_feature_names_a_test_that_exists() {
    let rows = feature_rows();
    assert!(!rows.is_empty(), "the conformance list has no features");
    let mut seen = std::collections::HashSet::new();
    for row in &rows {
        let [feature, _, status, test] = row.as_slice() else {
            panic!("a conformance row has the wrong number of cells: {row:?}");
        };
        assert!(seen.insert(feature.clone()), "{feature} is listed twice");
        assert!(STATUSES.contains(&status.as_str()), "{feature} has the status {status:?}");
        if status != "done" {
            continue;
        }
        let Some((file, function)) = test.split_once("::") else {
            panic!("{feature} is done but names no test as <file>::<function>: {test:?}");
        };
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(file);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{feature}: cannot read {}: {error}", path.display()));
        let declared = format!("fn {function}()");
        let tested = source
            .match_indices(&declared)
            .any(|(at, _)| source[..at].trim_end().ends_with("#[test]"));
        assert!(tested, "{feature}: {file} has no #[test] fn {function}");
    }
}
