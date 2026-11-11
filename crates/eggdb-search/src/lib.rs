//! EggDB's full-text search (D195): analyzers ([`Config`]) that turn text into lexemes with an
//! English stemmer written from the published Snowball algorithm description, text-search
//! vectors ([`TsVector`]) and queries ([`TsQuery`]) in PostgreSQL's text forms with phrase and
//! prefix matching, and ranking ([`ts_rank`], [`bm25`]). It depends on nothing, so every
//! protocol and query language can search with it.

mod english;
mod headline;
mod query;
mod rank;
mod vector;

pub use english::{is_stop_word, stem};
pub use query::TsQuery;
pub use rank::{DEFAULT_RANK_WEIGHTS, bm25, ts_rank, ts_rank_weighted};
pub use headline::{HeadlineOptions, headline};
pub use vector::{Config, TsVector, Weight, words};
