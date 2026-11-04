use eggdb::sql::Dialect;

/// The SQL dialects a query tab runs in, in the dialect pill's order
/// (`docs/ux/workspace.md` 2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryDialect {
    /// PostgreSQL's SQL, the default.
    #[default]
    PostgreSql,
    /// MySQL's SQL.
    MySql,
    /// Microsoft SQL Server's Transact-SQL.
    SqlServer,
    /// Oracle's SQL.
    Oracle,
    /// SQLite's SQL.
    Sqlite,
}

impl QueryDialect {
    /// Every dialect, in the menu's order.
    pub const ALL: [Self; 5] = [
        Self::PostgreSql,
        Self::MySql,
        Self::SqlServer,
        Self::Oracle,
        Self::Sqlite,
    ];

    /// The name the pill, the menu and the status bar show.
    pub fn label(self) -> &'static str {
        match self {
            Self::PostgreSql => "PostgreSQL",
            Self::MySql => "MySQL",
            Self::SqlServer => "SQL Server",
            Self::Oracle => "Oracle",
            Self::Sqlite => "SQLite",
        }
    }

    /// The engine's dialect a run uses.
    pub fn engine(self) -> Dialect {
        match self {
            Self::PostgreSql => Dialect::PostgreSql,
            Self::MySql => Dialect::MySql,
            Self::SqlServer => Dialect::SqlServer,
            Self::Oracle => Dialect::Oracle,
            Self::Sqlite => Dialect::Sqlite,
        }
    }

    /// The pill's dialect that runs as the engine's `dialect`; PostgreSQL's for one the pill
    /// does not list.
    pub fn of_engine(dialect: Dialect) -> Self {
        Self::ALL
            .into_iter()
            .find(|listed| listed.engine() == dialect)
            .unwrap_or_default()
    }

    /// Its place in [`QueryDialect::ALL`].
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|dialect| *dialect == self)
            .unwrap_or(0)
    }

    /// The dialect at `index` in [`QueryDialect::ALL`], the default past the end.
    pub fn at(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or_default()
    }

    /// The first dialect after `from` (wrapping) whose label starts with `letter`, as typing
    /// a letter in the open menu jumps.
    pub fn starting_with(letter: char, from: usize) -> Option<usize> {
        let letter = letter.to_ascii_lowercase();
        (1..=Self::ALL.len())
            .map(|step| (from + step) % Self::ALL.len())
            .find(|&index| {
                Self::ALL[index]
                    .label()
                    .chars()
                    .next()
                    .is_some_and(|first| first.to_ascii_lowercase() == letter)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_lists_five_dialects_and_a_letter_jumps_to_the_next_match() {
        let labels: Vec<&str> = QueryDialect::ALL.iter().map(|d| d.label()).collect();
        assert_eq!(
            labels,
            ["PostgreSQL", "MySQL", "SQL Server", "Oracle", "SQLite"]
        );
        assert_eq!(QueryDialect::starting_with('s', 0), Some(2));
        assert_eq!(QueryDialect::starting_with('S', 2), Some(4));
        assert_eq!(QueryDialect::starting_with('s', 4), Some(2));
        assert_eq!(QueryDialect::starting_with('x', 0), None);
        assert_eq!(QueryDialect::at(9), QueryDialect::PostgreSql);
        assert_eq!(QueryDialect::Oracle.index(), 3);
        assert_eq!(QueryDialect::SqlServer.engine(), Dialect::SqlServer);
        for dialect in QueryDialect::ALL {
            assert_eq!(QueryDialect::of_engine(dialect.engine()), dialect);
        }
    }
}
