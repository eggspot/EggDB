use crate::{Hnsw, HnswOptions, combine, cosine_distance, format, inner_product, l1_distance, l2_distance, l2_normalize, parse};

#[test]
fn text_round_trips_as_pgvector_writes_it() {
    assert_eq!(parse("[1, 2.5,-3e2]").unwrap(), vec![1.0, 2.5, -300.0]);
    assert_eq!(format(&[1.0, 2.5, -300.0]), "[1,2.5,-300]");
    for bad in ["", "[]", "[1,,2]", "1,2", "[1,a]", "[NaN]", "[inf]", "[1,2"] {
        assert!(parse(bad).is_err(), "{bad}");
    }
    assert!(parse(&format!("[{}]", vec!["1"; 16_001].join(","))).is_err());
}

#[test]
fn distances_match_their_definitions() {
    let a = [1.0, 2.0, 3.0];
    let b = [4.0, 6.0, 3.0];
    assert_eq!(l2_distance(&a, &b).unwrap(), 5.0);
    assert_eq!(l1_distance(&a, &b).unwrap(), 7.0);
    assert_eq!(inner_product(&a, &b).unwrap(), 25.0);
    assert!((cosine_distance(&[1.0, 0.0], &[0.0, 1.0]).unwrap() - 1.0).abs() < 1e-12);
    assert!(cosine_distance(&[0.0, 0.0], &[1.0, 1.0]).unwrap().is_nan());
    assert!(l2_distance(&a, &[1.0]).is_err());
    assert_eq!(l2_normalize(&[3.0, 4.0]), vec![0.6, 0.8]);
    assert_eq!(combine(&a, &b, true).unwrap(), vec![-3.0, -4.0, 0.0]);
    assert!(combine(&[f32::MAX], &[f32::MAX], false).is_err());
}

#[test]
fn hnsw_finds_the_nearest_neighbours_with_high_recall() {
    let dimensions = 16;
    let mut state = 7u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % 10_000) as f32 / 10_000.0
    };
    let points: Vec<Vec<f32>> = (0..2_000).map(|_| (0..dimensions).map(|_| next()).collect()).collect();
    let mut graph = Hnsw::new(dimensions, HnswOptions::default());
    for (id, point) in points.iter().enumerate() {
        graph.insert(id as u64, point.clone()).unwrap();
    }
    assert_eq!(graph.len(), 2_000);
    let mut hits = 0;
    let queries = 50;
    for _ in 0..queries {
        let query: Vec<f32> = (0..dimensions).map(|_| next()).collect();
        let mut exact: Vec<(u64, f64)> = points.iter().enumerate().map(|(id, point)| (id as u64, l2_distance(&query, point).unwrap())).collect();
        exact.sort_by(|a, b| a.1.total_cmp(&b.1));
        let wanted: Vec<u64> = exact.iter().take(10).map(|(id, _)| *id).collect();
        let found = graph.search(&query, 10, 64);
        assert!(found.windows(2).all(|pair| pair[0].1 <= pair[1].1));
        hits += found.iter().filter(|(id, _)| wanted.contains(id)).count();
    }
    let recall = hits as f64 / (queries * 10) as f64;
    assert!(recall > 0.9, "recall {recall}");
    assert!(graph.insert(9, vec![0.0; 3]).is_err());
    assert!(graph.search(&[0.0; 3], 5, 10).is_empty());
}
