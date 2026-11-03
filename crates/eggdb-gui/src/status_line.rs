use std::time::Duration;

/// `n` with a `,` between each group of three digits: `2,104,311`.
pub fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `1 row`, `0 rows`, `12,400 rows`.
pub fn rows_text(n: usize) -> String {
    match n {
        1 => "1 row".to_owned(),
        _ => format!("{} rows", thousands(n)),
    }
}

/// A run's time as the status line shows it (`docs/ux/workspace.md` 2.4): one decimal under
/// 10 ms (`0.4 ms`), none from 10 ms (`12 ms`), seconds with two decimals from 1,000 ms
/// (`1.23 s`).
pub fn elapsed_text(elapsed: Duration) -> String {
    let milliseconds = elapsed.as_secs_f64() * 1000.0;
    if milliseconds < 9.95 {
        format!("{milliseconds:.1} ms")
    } else if milliseconds < 999.5 {
        format!("{milliseconds:.0} ms")
    } else {
        format!("{:.2} s", elapsed.as_secs_f64())
    }
}

/// A size in bytes, 1,024-based: `812 B`, `1.2 KB`, `8.2 MB`, `1.0 GB`.
pub fn bytes_text(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// The bytes `rows` take on the wire as PostgreSQL `DataRow` messages: a type byte, a length
/// and a column count per row, and a length and the text per value.
pub fn wire_bytes(rows: &[Vec<Option<String>>]) -> u64 {
    rows.iter()
        .map(|row| {
            7 + row
                .iter()
                .map(|value| 4 + value.as_ref().map_or(0, |text| text.len() as u64))
                .sum::<u64>()
        })
        .sum()
}

/// A result set's summary: `3 rows · 0.4 ms · 1.2 KB`.
pub fn result_summary(rows: usize, elapsed: Duration, bytes: u64) -> String {
    format!(
        "{} · {} · {}",
        rows_text(rows),
        elapsed_text(elapsed),
        bytes_text(bytes)
    )
}

/// A result cut at the fetch limit: `100,000 rows shown of 2,104,311 · 1.4 s · 8.2 MB`.
pub fn truncated_summary(shown: usize, total: usize, elapsed: Duration, bytes: u64) -> String {
    format!(
        "{} shown of {} · {} · {}",
        rows_text(shown),
        thousands(total),
        elapsed_text(elapsed),
        bytes_text(bytes)
    )
}

/// A statement without rows: its tag and time, `INSERT 0 2 · 0.3 ms`.
pub fn tag_summary(tag: &str, elapsed: Duration) -> String {
    format!("{tag} · {}", elapsed_text(elapsed))
}

/// A `Run all`: `5 statements · 3 rows · 12 ms`.
pub fn run_all_summary(statements: usize, rows: usize, elapsed: Duration) -> String {
    let statements = match statements {
        1 => "1 statement".to_owned(),
        n => format!("{} statements", thousands(n)),
    };
    format!(
        "{statements} · {} · {}",
        rows_text(rows),
        elapsed_text(elapsed)
    )
}

/// While a statement runs: `Running · 1.2 s`, updated every 100 ms.
pub fn running_summary(elapsed: Duration) -> String {
    format!("Running · {:.1} s", elapsed.as_secs_f64())
}

/// After `Stop`: `Stopped · 12,400 rows (partial)`.
pub fn stopped_summary(rows: usize) -> String {
    format!("Stopped · {} (partial)", rows_text(rows))
}

/// The truncated-results banner's text (`docs/ux/workspace.md` 3.7).
pub fn truncated_banner(shown: usize, total: usize) -> String {
    format!(
        "Showing the first {} rows of {}. Add LIMIT, export, or sort and filter in the query.",
        thousands(shown),
        thousands(total)
    )
}

/// The status bar's selection item: `4 × 3 cells`.
pub fn selection_text(rows: usize, columns: usize) -> String {
    format!("{} × {} cells", thousands(rows), thousands(columns))
}

/// The copy toast: `Copied 12 cells`, `Copied 1 cell`.
pub fn copied_cells_text(cells: usize) -> String {
    match cells {
        1 => "Copied 1 cell".to_owned(),
        n => format!("Copied {} cells", thousands(n)),
    }
}

/// The `Copy as INSERT` toast: `Copied 3 INSERT statements`.
pub fn copied_inserts_text(statements: usize) -> String {
    match statements {
        1 => "Copied 1 INSERT statement".to_owned(),
        n => format!("Copied {} INSERT statements", thousands(n)),
    }
}

/// A summary as a screen reader announces it: `3 rows · 0.4 ms` reads `3 rows, 0.4
/// milliseconds`, the `·` as a pause and the units in words.
pub fn spoken(summary: &str) -> String {
    summary
        .split(" · ")
        .map(|part| {
            let words: Vec<&str> = part.split(' ').collect();
            words
                .iter()
                .map(|word| match *word {
                    "ms" => "milliseconds",
                    "s" if words.len() == 2 => "seconds",
                    "B" => "bytes",
                    "KB" => "kilobytes",
                    "MB" => "megabytes",
                    "GB" => "gigabytes",
                    other => other,
                })
                .collect::<Vec<&str>>()
                .join(" ")
        })
        .collect::<Vec<String>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(milliseconds: f64) -> Duration {
        Duration::from_secs_f64(milliseconds / 1000.0)
    }

    #[test]
    fn the_status_line_strings_match_the_spec() {
        assert_eq!(result_summary(3, ms(0.4), 1229), "3 rows · 0.4 ms · 1.2 KB");
        assert_eq!(result_summary(1, ms(9.9), 12), "1 row · 9.9 ms · 12 B");
        assert_eq!(result_summary(0, ms(12.4), 0), "0 rows · 12 ms · 0 B");
        assert_eq!(
            result_summary(1234, ms(1234.0), 9 * 1024 * 1024),
            "1,234 rows · 1.23 s · 9.0 MB"
        );
        assert_eq!(tag_summary("INSERT 0 2", ms(0.3)), "INSERT 0 2 · 0.3 ms");
        assert_eq!(
            run_all_summary(5, 3, ms(12.0)),
            "5 statements · 3 rows · 12 ms"
        );
        assert_eq!(
            run_all_summary(1, 1, ms(2.0)),
            "1 statement · 1 row · 2.0 ms"
        );
        assert_eq!(stopped_summary(12_400), "Stopped · 12,400 rows (partial)");
        assert_eq!(running_summary(ms(1200.0)), "Running · 1.2 s");
        assert_eq!(
            truncated_summary(100_000, 2_104_311, ms(1400.0), 8_598_323),
            "100,000 rows shown of 2,104,311 · 1.40 s · 8.2 MB"
        );
        assert_eq!(
            truncated_banner(100_000, 100_001),
            "Showing the first 100,000 rows of 100,001. Add LIMIT, export, or sort and filter in the query."
        );
        assert_eq!(selection_text(4, 3), "4 × 3 cells");
        assert_eq!(copied_cells_text(12), "Copied 12 cells");
        assert_eq!(copied_inserts_text(3), "Copied 3 INSERT statements");
    }

    #[test]
    fn numbers_and_sizes_are_grouped_and_scaled() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_000_000), "1,000,000");
        assert_eq!(bytes_text(1023), "1023 B");
        assert_eq!(bytes_text(1024), "1.0 KB");
        assert_eq!(bytes_text(3 * 1024 * 1024 * 1024 * 1024), "3072.0 GB");
        assert_eq!(elapsed_text(ms(9.96)), "10 ms");
        assert_eq!(elapsed_text(ms(999.6)), "1.00 s");
        assert_eq!(wire_bytes(&[vec![Some("1".into()), None]]), 7 + 5 + 4);
    }

    #[test]
    fn a_summary_is_spoken_with_pauses_and_units_in_words() {
        assert_eq!(spoken("3 rows · 0.4 ms"), "3 rows, 0.4 milliseconds");
        assert_eq!(
            spoken("1 row · 1.23 s · 1.2 KB"),
            "1 row, 1.23 seconds, 1.2 kilobytes"
        );
        assert_eq!(spoken("Error"), "Error");
    }
}
