//! EggDB's full-text search (D195): analyzers ([`Config`]) that turn text into lexemes with an
//! English, French, German, Finnish, Hungarian, Indonesian, Dutch, Spanish, Portuguese, Romanian, Turkish, Italian, Russian, Swedish, Norwegian and Danish stemmers written from the published Snowball algorithm descriptions, word
//! segmentation for Chinese, Japanese, Korean and Thai ([`segmented_words`]), configurations built from
//! dictionaries ([`Analyzer`]), text-search
//! vectors ([`TsVector`]) and queries ([`TsQuery`]) in PostgreSQL's text forms with phrase and
//! prefix matching, and ranking ([`ts_rank`], [`bm25`]). It depends on nothing, so every
//! protocol and query language can search with it.

mod analyzer;
mod dutch;
mod english;
mod finnish;
mod french;
mod german;
mod hungarian;
mod indonesian;
mod italian;
mod portuguese;
mod romanian;
mod russian;
mod scandinavian;
mod segment;
mod spanish;
mod turkish;
mod headline;
mod query;
mod rank;
mod vector;

pub use english::{is_stop_word, stem};
pub use query::TsQuery;
pub use rank::{DEFAULT_RANK_WEIGHTS, bm25, ts_rank, ts_rank_weighted};
pub use headline::{HeadlineOptions, headline};
pub use segment::segmented_words;
pub use analyzer::{
    Analyzer, Dictionary, Recognition, StopWords, TOKEN_TYPES, is_token_type, synonym_list, token_type_of,
    word_list,
};
pub use vector::{Config, Lexer, TsVector, Weight, words};
