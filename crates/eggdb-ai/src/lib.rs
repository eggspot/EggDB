//! AI helpers that run inside EggDB with nothing to download and nothing sent over the network
//! (D205): [`embed`], a feature-hashing text embedding (stemmed words and word pairs hashed
//! into signed buckets, then normalized), good for lexical similarity search with the vector
//! operators; [`reciprocal_rank_fusion`] to merge a vector ranking with a full-text ranking;
//! and [`chunks`] to cut documents into overlapping pieces for retrieval.

/// The dimensions [`embed`] uses unless told otherwise.
pub const DEFAULT_DIMENSIONS: usize = 256;
/// The `k` constant of reciprocal rank fusion, as its paper (Cormack, Clarke and Büttcher,
/// 2009) recommends.
pub const DEFAULT_RRF_K: f64 = 60.0;

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3))
}

fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    let hash = fnv1a(feature.as_bytes());
    let bucket = (hash % vector.len() as u64) as usize;
    let sign = if hash >> 63 == 0 { 1.0 } else { -1.0 };
    vector[bucket] += sign * weight;
}

/// The embedding of `text` in `dimensions` (at least 1, at most
/// [`eggdb_vector::MAX_DIMENSIONS`]): English words are stemmed and stop words dropped, each word
/// and each pair of neighbouring words adds to a hashed bucket, and the vector is scaled to
/// length 1. Texts sharing words land close by cosine distance; the same text always gives the
/// same vector.
pub fn embed(text: &str, dimensions: usize) -> Vec<f32> {
    let dimensions = dimensions.clamp(1, eggdb_vector::MAX_DIMENSIONS);
    let mut vector = vec![0.0f32; dimensions];
    let lexemes: Vec<String> = eggdb_search::words(text)
        .into_iter()
        .filter_map(|word| eggdb_search::Config::English.lexeme(word))
        .collect();
    for lexeme in &lexemes {
        add_feature(&mut vector, lexeme, 1.0);
    }
    for pair in lexemes.windows(2) {
        add_feature(&mut vector, &format!("{} {}", pair[0], pair[1]), 0.5);
    }
    eggdb_vector::l2_normalize(&vector)
}

/// The reciprocal rank fusion score of one document from its rank in each list (1 for the
/// first; `None` when a list does not hold it): the sum of `1 / (k + rank)`.
pub fn reciprocal_rank_fusion(ranks: &[Option<u64>], k: f64) -> f64 {
    ranks.iter().flatten().map(|rank| 1.0 / (k + *rank as f64)).sum()
}

/// `text` cut into pieces of at most `words` words, each starting `words - overlap` words after
/// the previous one, so a sentence split by a cut is whole in one of the pieces.
pub fn chunks(text: &str, words: usize, overlap: usize) -> Vec<String> {
    let all: Vec<&str> = text.split_whitespace().collect();
    let words = words.max(1);
    let step = words.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while start < all.len() {
        let end = (start + words).min(all.len());
        out.push(all[start..end].join(" "));
        if end == all.len() {
            break;
        }
        start += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RRF_K, chunks, embed, reciprocal_rank_fusion};

    #[test]
    fn related_texts_embed_closer_than_unrelated_ones() {
        let cat = embed("The cats are sleeping on the warm sofa", 256);
        let kitten = embed("A cat sleeps on a sofa", 256);
        let finance = embed("Quarterly revenue grew faster than expenses", 256);
        let near = eggdb_vector::cosine_distance(&cat, &kitten).unwrap();
        let far = eggdb_vector::cosine_distance(&cat, &finance).unwrap();
        assert!(near < far, "{near} {far}");
        assert!((eggdb_vector::norm(&cat) - 1.0).abs() < 1e-6);
        assert_eq!(embed("The cats are sleeping on the warm sofa", 256), cat);
        assert_eq!(embed("", 8), vec![0.0; 8]);
        assert_eq!(embed("words", 0).len(), 1);
    }

    #[test]
    fn rank_fusion_rewards_documents_both_lists_rank_well() {
        let both = reciprocal_rank_fusion(&[Some(2), Some(2)], DEFAULT_RRF_K);
        let one = reciprocal_rank_fusion(&[Some(1), None], DEFAULT_RRF_K);
        assert!(both > one);
        assert_eq!(reciprocal_rank_fusion(&[None, None], DEFAULT_RRF_K), 0.0);
    }

    #[test]
    fn chunks_overlap_and_cover_the_text() {
        let text = "one two three four five six seven";
        assert_eq!(chunks(text, 3, 1), ["one two three", "three four five", "five six seven"]);
        assert_eq!(chunks(text, 10, 2), [text]);
        assert!(chunks("", 3, 1).is_empty());
        assert_eq!(chunks("a b", 1, 5), ["a", "b"]);
    }
}
