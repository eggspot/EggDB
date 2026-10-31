use crate::query::TsQuery;
use crate::vector::TsVector;

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// A rank for `vector` against `query` from how often the query's lexemes occur, scaled to
/// (0, 1): each matched term adds `tf / (tf + 1)`, the sum divided by the number of terms, and
/// `0` when the query does not match. It orders documents as PostgreSQL's `ts_rank` does for
/// plain term queries, but its values are EggDB's own.
pub fn ts_rank(vector: &TsVector, query: &TsQuery) -> f64 {
    if !query.matches(vector) {
        return 0.0;
    }
    let terms = query.terms();
    if terms.is_empty() {
        return 0.0;
    }
    let total: f64 = terms
        .iter()
        .map(|(lexeme, prefix)| {
            let frequency = vector.positions(lexeme, *prefix).len() as f64;
            frequency / (frequency + 1.0)
        })
        .sum();
    total / terms.len() as f64
}

/// Okapi BM25 (Robertson and Zaragoza) of one document: `frequencies` are each query term's
/// count in the document with the number of documents holding it, over `documents` documents
/// whose average length is `average_length`.
pub fn bm25(frequencies: &[(u64, u64)], length: u64, documents: u64, average_length: f64) -> f64 {
    frequencies
        .iter()
        .map(|&(frequency, holding)| {
            let idf = (1.0 + (documents as f64 - holding as f64 + 0.5) / (holding as f64 + 0.5)).ln();
            let frequency = frequency as f64;
            let norm = 1.0 - B + B * length as f64 / average_length.max(1.0);
            idf * frequency * (K1 + 1.0) / (frequency + K1 * norm)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::Config;

    #[test]
    fn more_occurrences_rank_higher_and_bm25_prefers_rare_terms() {
        let query = TsQuery::parse(Config::English, "rat").unwrap();
        let once = TsVector::from_text(Config::English, "a rat");
        let twice = TsVector::from_text(Config::English, "a rat and another rat");
        let none = TsVector::from_text(Config::English, "a cat");
        assert!(ts_rank(&twice, &query) > ts_rank(&once, &query));
        assert_eq!(ts_rank(&none, &query), 0.0);
        let rare = bm25(&[(1, 1)], 10, 100, 10.0);
        let common = bm25(&[(1, 90)], 10, 100, 10.0);
        assert!(rare > common && common > 0.0);
    }
}
