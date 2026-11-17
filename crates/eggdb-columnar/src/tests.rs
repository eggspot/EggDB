use crate::{
    Dictionary, Encoding, FloatChunk, FloatEncoding, IntegerChunk, StringChunk, StringEncoding, bit_width, decode_deltas, decode_hybrid, encode_deltas,
    encode_hybrid, pack, unpack,
};

#[test]
fn bit_packing_round_trips_every_width() {
    for width in 0..=64u8 {
        let mask = if width == 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let values: Vec<u64> = (0..37u64)
            .map(|at| at.wrapping_mul(0x9e37_79b9_7f4a_7c15) & mask)
            .collect();
        let mut packed = Vec::new();
        pack(&values, width, &mut packed);
        assert_eq!(packed.len(), (37 * usize::from(width)).div_ceil(8));
        assert_eq!(
            unpack(&packed, width, values.len()).unwrap(),
            values,
            "width {width}"
        );
    }
    assert_eq!(bit_width(&[0, 1, 5]), 3);
    assert_eq!(bit_width(&[0]), 0);
    assert!(unpack(&[0xff], 7, 2).is_err());
    assert!(unpack(&[], 65, 1).is_err());
}

#[test]
fn the_parquet_specification_example_decodes() {
    let mut packed = Vec::new();
    pack(&[0, 1, 2, 3, 4, 5, 6, 7], 3, &mut packed);
    assert_eq!(packed, [0b1000_1000, 0b1100_0110, 0b1111_1010]);
}

#[test]
fn hybrid_runs_round_trip_and_compress_repeats() {
    let mut values: Vec<u64> = (0..13).map(|at| at % 4).collect();
    values.extend(std::iter::repeat_n(3, 1_000));
    values.extend([1, 2, 1]);
    let mut encoded = Vec::new();
    encode_hybrid(&values, 2, &mut encoded);
    assert!(encoded.len() < 20, "{} bytes", encoded.len());
    let (decoded, used) = decode_hybrid(&encoded, 2, values.len()).unwrap();
    assert_eq!(decoded, values);
    assert_eq!(used, encoded.len());
    for length in 0..encoded.len() {
        assert!(decode_hybrid(&encoded[..length], 2, values.len()).is_err());
    }
    assert!(decode_hybrid(&[0x00], 2, 1).is_err());
    assert!(
        decode_hybrid(
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f],
            0,
            1 << 20
        )
        .map(|(found, _)| found.len())
        .unwrap_or(0)
            <= 1 << 20
    );
}

#[test]
fn deltas_round_trip_including_extremes() {
    let values = [i64::MIN, -1, 0, 1, i64::MAX, 1_700_000_000, 1_700_000_060];
    let mut encoded = Vec::new();
    encode_deltas(&values, &mut encoded);
    assert_eq!(decode_deltas(&encoded).unwrap().0, values);
    assert!(decode_deltas(&encoded[..encoded.len() - 1]).is_err());
    assert!(decode_deltas(&[0xff; 12]).is_err());
}

#[test]
fn chunks_pick_the_smallest_encoding() {
    let times: Vec<i64> = (0..10_000).map(|at| 1_700_000_000 + at * 60).collect();
    let flags: Vec<i64> = (0..10_000).map(|at| i64::from(at >= 9_000)).collect();
    let noise: Vec<i64> = (0..10_000i64)
        .map(|at| (at.wrapping_mul(2_654_435_761) >> 7) % 1_000)
        .collect();
    let time_chunk = IntegerChunk::encode(&times);
    let flag_chunk = IntegerChunk::encode(&flags);
    let noise_chunk = IntegerChunk::encode(&noise);
    assert_eq!(flag_chunk.encoding(), Encoding::Runs);
    assert!(flag_chunk.encoded_bytes() < 32);
    assert!(time_chunk.encoded_bytes() < 10_000 * 3);
    assert_eq!(noise_chunk.encoding(), Encoding::FrameOfReference);
    for (chunk, values) in [
        (&time_chunk, &times),
        (&flag_chunk, &flags),
        (&noise_chunk, &noise),
    ] {
        assert_eq!(&chunk.decode().unwrap(), values);
        assert_eq!(IntegerChunk::from_bytes(&chunk.to_bytes()).unwrap(), *chunk);
    }
    assert_eq!(flag_chunk.sum_at_least(1).unwrap(), 1_000);
    assert_eq!(
        time_chunk.sum_at_least(i64::MIN).unwrap(),
        times.iter().map(|value| i128::from(*value)).sum()
    );
    assert_eq!(time_chunk.sum_at_least(i64::MAX).unwrap(), 0);
    assert!(IntegerChunk::encode(&[]).decode().unwrap().is_empty());
}

#[test]
fn damaged_chunks_are_errors() {
    let bytes = IntegerChunk::encode(&[5, 6, 7, 8, 1_000]).to_bytes();
    for length in 0..bytes.len() {
        assert!(
            IntegerChunk::from_bytes(&bytes[..length]).is_err(),
            "length {length}"
        );
    }
    for at in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x5a;
        let _ = IntegerChunk::from_bytes(&damaged);
    }
    let mut huge = vec![0u8];
    crate::write_varint(u64::MAX, &mut huge);
    huge.extend_from_slice(&[0; 33]);
    assert!(IntegerChunk::from_bytes(&huge).is_err());
}

#[test]
fn dictionaries_index_repeated_values() {
    let rows: Vec<&[u8]> = vec![b"red", b"green", b"red", b"red", b"blue", b"red"];
    let dictionary = Dictionary::of(rows.iter().copied());
    assert_eq!(dictionary.values.len(), 3);
    assert_eq!(dictionary.indexes, [0, 1, 0, 0, 2, 0]);
    assert_eq!(dictionary.row(4), Some(&b"blue"[..]));
    assert!(!dictionary.pays_off());
    assert!(Dictionary::of(std::iter::repeat_n(&b"x"[..], 10)).pays_off());
}

#[test]
fn snappy_blocks_decompress_and_literal_blocks_round_trip() {
    let block = [12, 0x08, b'a', b'b', b'c', 0x15, 0x03];
    assert_eq!(
        crate::snappy_decompress(&block, 100).unwrap(),
        b"abcabcabcabc"
    );
    assert!(crate::snappy_decompress(&block, 5).is_err());
    assert!(crate::snappy_decompress(&[12, 0x08, b'a', b'b', b'c', 0x15, 0x09], 100).is_err());
    assert!(crate::snappy_decompress(&[12, 0x08, b'a'], 100).is_err());
    for size in [0, 1, 59, 60, 61, 255, 256, 257, 70_000, 200_000] {
        let input: Vec<u8> = (0..size).map(|at| (at % 251) as u8).collect();
        assert_eq!(
            crate::snappy_decompress(&crate::snappy_compress(&input), size).unwrap(),
            input,
            "{size}"
        );
    }
}

#[test]
fn float_chunks_round_trip_bit_for_bit_and_compress_slow_series() {
    let series: Vec<f64> = (0..1000).map(|at| 20.0 + f64::from(at % 7) * 0.5).collect();
    let chunk = FloatChunk::encode(&series);
    assert_eq!(chunk.encoding(), FloatEncoding::Xor);
    assert!(chunk.encoded_bytes() < series.len() * 2, "{}", chunk.encoded_bytes());
    assert_eq!(chunk.decode().unwrap(), series);
    let odd = [f64::NAN, -0.0, f64::INFINITY, 1.0e-310, f64::MIN, 3.5];
    let chunk = FloatChunk::encode(&odd);
    let back = FloatChunk::from_bytes(&chunk.to_bytes()).unwrap().decode().unwrap();
    assert!(back.iter().zip(odd.iter()).all(|(a, b)| a.to_bits() == b.to_bits()));
    let stats = chunk.stats().unwrap();
    assert_eq!((stats.min, stats.max, stats.count), (f64::MIN, f64::INFINITY, 6));
    assert!(FloatChunk::encode(&[f64::NAN]).stats().unwrap().min.is_nan());
    assert_eq!(FloatChunk::encode(&[]).decode().unwrap(), Vec::<f64>::new());
    let mut bytes = FloatChunk::encode(&series).to_bytes();
    bytes.truncate(bytes.len() - 3);
    assert!(FloatChunk::from_bytes(&bytes).is_err());
    let mut wrong = FloatChunk::encode(&[1.0, 2.0]).to_bytes();
    let last = wrong.len() - 1;
    wrong[last] ^= 0x80;
    assert!(FloatChunk::from_bytes(&wrong).is_err());
}

#[test]
fn string_chunks_choose_dictionary_snappy_or_plain() {
    let names: Vec<Vec<u8>> = (0..10).map(|number| format!("service-number-{number:05}").into_bytes()).collect();
    let repeated: Vec<&[u8]> = (0..1000u64)
        .map(|at| names[(at.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 60) as usize % 10].as_slice())
        .collect();
    let chunk = StringChunk::encode(&repeated);
    assert_eq!(chunk.encoding(), StringEncoding::Dictionary);
    assert_eq!(chunk.decode().unwrap(), repeated.iter().map(|value| value.to_vec()).collect::<Vec<_>>());
    let stats = chunk.stats().unwrap();
    assert_eq!(stats.count, 1000);
    assert!(stats.min <= stats.max);
    let long = b"the same long sentence repeated over and over again ".repeat(40);
    let distinct: Vec<Vec<u8>> = (0..20).map(|at| [long.as_slice(), &[at as u8]].concat()).collect();
    let rows: Vec<&[u8]> = distinct.iter().map(Vec::as_slice).collect();
    let chunk = StringChunk::encode(&rows);
    assert_eq!(chunk.encoding(), StringEncoding::SnappyPlain);
    assert_eq!(StringChunk::from_bytes(&chunk.to_bytes()).unwrap().decode().unwrap(), distinct);
    let few: Vec<&[u8]> = vec![b"a", b"bc"];
    let chunk = StringChunk::encode(&few);
    assert_eq!(chunk.encoding(), StringEncoding::Plain);
    let mut bytes = chunk.to_bytes();
    bytes.push(0);
    assert!(StringChunk::from_bytes(&bytes).is_err());
    assert!(StringChunk::from_bytes(&[9, 0]).is_err());
}
