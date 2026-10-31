const MIN_COLUMN: f32 = 48.0;
const MAX_COLUMN: f32 = 480.0;
const SAMPLED_ROWS: usize = 200;
const CELL_PADDING: f32 = 12.0;

/// Each column's width (`docs/ux/gui.md`, "Grid"): the wider of its header and its first 200
/// cells, measured by `width`, plus padding on both sides, between 48 and 480 pixels.
pub fn column_widths(
    headers: &[String],
    rows: &[Vec<String>],
    width: &dyn Fn(&str) -> f32,
) -> Vec<f32> {
    headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let widest = rows
                .iter()
                .take(SAMPLED_ROWS)
                .filter_map(|row| row.get(column))
                .map(|cell| width(cell))
                .fold(width(header), f32::max);
            (widest + 2.0 * CELL_PADDING).clamp(MIN_COLUMN, MAX_COLUMN).ceil()
        })
        .collect()
}

/// The rows a grid `height` pixels tall shows from `first`, at 24 pixels a row.
pub fn visible_rows(total: usize, first: usize, height: f32) -> std::ops::Range<usize> {
    let fits = (height / 24.0).floor().max(0.0) as usize;
    let start = first.min(total);
    start..(start + fits).min(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_fit_their_widest_sampled_cell_within_the_limits() {
        let measure = |text: &str| text.chars().count() as f32 * 8.0;
        let headers = vec!["id".to_owned(), "name".to_owned(), "notes".to_owned()];
        let rows = vec![
            vec!["1".into(), "Ada Lovelace".into(), "x".repeat(200)],
            vec!["2".into(), "Grace".into(), String::new()],
        ];
        assert_eq!(column_widths(&headers, &rows, &measure), vec![48.0, 120.0, 480.0]);
        assert_eq!(visible_rows(10, 8, 100.0), 8..10);
        assert_eq!(visible_rows(10, 20, 100.0), 10..10);
    }
}
