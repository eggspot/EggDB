use std::collections::HashMap;

/// Distinct values of a column in first-seen order, with each row's index into them, as
/// dictionary encodings store low-cardinality columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dictionary {
    /// The distinct values.
    pub values: Vec<Vec<u8>>,
    /// Each row's index into `values`.
    pub indexes: Vec<u64>,
}

impl Dictionary {
    /// The dictionary of `rows`.
    pub fn of<'r>(rows: impl IntoIterator<Item = &'r [u8]>) -> Self {
        let mut seen: HashMap<&'r [u8], u64> = HashMap::new();
        let mut dictionary = Self::default();
        for row in rows {
            let next = dictionary.values.len() as u64;
            let index = *seen.entry(row).or_insert_with(|| {
                dictionary.values.push(row.to_vec());
                next
            });
            dictionary.indexes.push(index);
        }
        dictionary
    }

    /// Whether the dictionary is worth it: fewer distinct values than half the rows.
    pub fn pays_off(&self) -> bool {
        self.values.len() * 2 < self.indexes.len()
    }

    /// The row at `at`.
    pub fn row(&self, at: usize) -> Option<&[u8]> {
        let index = usize::try_from(*self.indexes.get(at)?).ok()?;
        self.values.get(index).map(Vec::as_slice)
    }
}
