use super::{
    ExportReader, Header, ReadError, largest_key_bytes, write_entry, write_header, write_trailer,
};
use eggdb_storage::BTree;
use std::io::Read;
use std::time::{Duration, Instant};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn bytes(&mut self, length: usize) -> Vec<u8> {
        (0..length).map(|_| self.next() as u8).collect()
    }

    fn text(&mut self, length: usize) -> Vec<u8> {
        const PIECES: &[&str] = &[
            "a", "Z", " ", "\"", "\\", "\n", "\t", "\r", "/", "é", "😀", "\u{2028}", "{", "}", ",",
            ":", "0", "\u{7f}", "\u{1}", "\u{9b}", "\u{0}",
        ];
        let mut out = Vec::new();
        while out.len() < length {
            out.extend_from_slice(PIECES[self.below(PIECES.len())].as_bytes());
        }
        out
    }

    fn byte_string(&mut self, longest: usize) -> Vec<u8> {
        let length = match self.below(4) {
            0 => 0,
            1 => self.below(4),
            _ => self.below(longest + 1),
        };
        if self.below(2) == 0 {
            self.text(length)
        } else {
            self.bytes(length)
        }
    }
}

struct Trickle<'b> {
    bytes: &'b [u8],
    rng: Rng,
    interrupted: bool,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.interrupted && self.rng.below(5) == 0 {
            self.interrupted = true;
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        self.interrupted = false;
        let wanted = 1 + self.rng.below(9);
        let count = wanted.min(buffer.len()).min(self.bytes.len());
        let (now, rest) = self.bytes.split_at(count);
        buffer[..count].copy_from_slice(now);
        self.bytes = rest;
        Ok(count)
    }
}

fn export(header: &Header, entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    write_header(&mut out, header).unwrap();
    for (key, value) in entries {
        write_entry(&mut out, key, value).unwrap();
    }
    write_trailer(&mut out, entries.len() as u64).unwrap();
    out
}

type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn read_all(input: &mut dyn Read) -> Result<(Header, Entries), ReadError> {
    let mut reader = ExportReader::new(input);
    let header = reader.read_header()?;
    let mut entries = Vec::new();
    while let Some((key, value)) = reader.next_entry()? {
        entries.push((key.to_vec(), value.to_vec()));
    }
    assert_eq!(reader.entries(), entries.len() as u64);
    Ok((header, entries))
}

fn read_bytes(bytes: &[u8]) -> Result<(Header, Entries), ReadError> {
    read_all(&mut &bytes[..])
}

#[track_caller]
fn refused(bytes: &[u8]) -> (u64, String) {
    match read_bytes(bytes) {
        Err(ReadError::Invalid { line, problem }) => (line, problem),
        other => panic!(
            "{:?} was not refused: {other:?}",
            String::from_utf8_lossy(bytes)
        ),
    }
}

const HEADER: &str = "{\"format\":\"eggdb-jsonl\",\"version\":1}\n";

fn with_header(rest: &str) -> Vec<u8> {
    format!("{HEADER}{rest}").into_bytes()
}

#[test]
fn the_format_reads_as_specified() {
    let header = Header {
        page_size: Some(4096),
        eggdb: Some("0.2.0".to_owned()),
        exported_at: Some("2026-10-27T09:14:07.123456Z".to_owned()),
    };
    let entries = vec![
        (b"user:1".to_vec(), b"Ada".to_vec()),
        (vec![0x00, 0xff], b"line\n\"two\"\t\\".to_vec()),
        (Vec::new(), Vec::new()),
        (b"bell".to_vec(), b"\x07".to_vec()),
    ];
    assert_eq!(
        String::from_utf8(export(&header, &entries)).unwrap(),
        "{\"format\":\"eggdb-jsonl\",\"version\":1,\"eggdb\":\"0.2.0\",\"page_size\":4096,\"exported_at\":\"2026-10-27T09:14:07.123456Z\"}\n\
         {\"key\":\"user:1\",\"value\":\"Ada\"}\n\
         {\"key_hex\":\"00ff\",\"value\":\"line\\n\\\"two\\\"\\t\\\\\"}\n\
         {\"key\":\"\",\"value\":\"\"}\n\
         {\"key\":\"bell\",\"value_hex\":\"07\"}\n\
         {\"entries\":4}\n"
    );
}

#[test]
fn any_bytes_round_trip_whatever_the_reads_return() {
    let mut rng = Rng(0x5eed_1234_abcd_0001);
    for round in 0..300 {
        let entries: Entries = (0..rng.below(12))
            .map(|_| (rng.byte_string(40), rng.byte_string(300)))
            .collect();
        let header = Header {
            page_size: [None, Some(4096), Some(65536)][rng.below(3)],
            eggdb: (rng.below(2) == 0).then(|| "0.2.0".to_owned()),
            exported_at: None,
        };
        let bytes = export(&header, &entries);
        let mut input = Trickle {
            bytes: &bytes,
            rng: Rng(rng.next() | 1),
            interrupted: false,
        };
        let (read_header, read_entries) =
            read_all(&mut input).unwrap_or_else(|error| panic!("round {round}: {error:?}"));
        assert_eq!(read_header, header, "round {round}");
        assert_eq!(read_entries, entries, "round {round}");
    }
}

#[test]
fn large_values_cross_every_buffer_boundary() {
    let mut rng = Rng(77);
    let binary = rng.bytes(3 << 20);
    let text = rng.text(3 << 20);
    let entries = vec![
        (b"binary".to_vec(), binary),
        (b"text".to_vec(), text),
        (vec![b'k'; largest_key_bytes()], b"longest key".to_vec()),
    ];
    let bytes = export(&Header::default(), &entries);
    assert_eq!(read_bytes(&bytes).unwrap().1, entries);
}

#[test]
fn other_spellings_of_the_same_json_are_read_too() {
    let bytes = with_header(
        " { \"value\" : \"\\u00e9\\ud83d\\ude00\\/\\b\\f\" , \"key_hex\" : \"00FFaB\" } \r\n{\"entries\":1}",
    );
    let (_, entries) = read_bytes(&bytes).unwrap();
    assert_eq!(
        entries,
        vec![(vec![0x00, 0xff, 0xab], "é😀/\u{8}\u{c}".as_bytes().to_vec())]
    );
}

#[test]
fn every_mistake_is_refused_with_its_line() {
    let cases: &[(&[u8], u64, &str)] = &[
        (b"", 1, "the file is empty"),
        (b"\n", 1, "the line is empty"),
        (b"[1]\n", 1, "expected '{' to start the line's object, found '['"),
        (b"{\"version\":1}\n", 1, "its first line has no \"format\" field"),
        (b"{\"format\":\"csv\",\"version\":1}\n", 1, "its format is \"csv\""),
        (b"{\"format\":\"eggdb-jsonl\"}\n", 1, "the header has no \"version\""),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":2}\n", 1, "version 2 of eggdb-jsonl, from a newer EggDB"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1,\"page_size\":5000}\n", 1, "\"page_size\" must be a power of two"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1,\"key\":\"k\"}\n", 1, "\"key\" does not belong in the header"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":01}\n", 1, "leading zero"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1.0}\n", 1, "must be a whole number"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":\"1\"}\n", 1, "must be a whole number"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1,\"version\":1}\n", 1, "\"version\" appears twice"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1,\"colour\":1}\n", 1, "unknown field \"colour\""),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1} x\n", 1, "expected the end of the line after '}', found 'x'"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n", 1, "without the trailer line"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\",\"value\":\"b\"}\n", 2, "without the trailer line"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\",\"value\":\"b\"}", 2, "without the trailer line"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\",\"value\":\"b", 2, "the file ends inside a string"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"entries\":1}\n", 2, "the trailer says the export holds 1 entry, but the file has no entries"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"entries\":0}\n{\"entries\":0}\n", 3, "nothing may follow the trailer line"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"entries\":0}\n\n", 3, "nothing may follow the trailer line"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"entries\":0,\"key\":\"a\"}\n", 2, "\"key\" does not belong in the trailer line"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"format\":\"eggdb-jsonl\",\"version\":1}\n", 2, "a second header"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\"}\n", 2, "an entry needs \"value\" or \"value_hex\""),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"value\":\"a\"}\n", 2, "an entry needs \"key\" or \"key_hex\""),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{}\n", 2, "an entry needs \"key\" or \"key_hex\""),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\",\"key_hex\":\"61\",\"value\":\"\"}\n", 2, "\"key\" and \"key_hex\" cannot both appear"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\",\"value\":\"\",\"version\":1}\n", 2, "\"version\" does not belong in an entry"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\\x\",\"value\":\"\"}\n", 2, "unknown escape \\x"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\\u12\",\"value\":\"\"}\n", 2, "\\u needs four hex digits"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"\\ud800\",\"value\":\"\"}\n", 2, "half of a UTF-16 surrogate pair"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"\\udc00\",\"value\":\"\"}\n", 2, "half of a UTF-16 surrogate pair"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"\\ud800\\u0041\",\"value\":\"\"}\n", 2, "half of a UTF-16 surrogate pair"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"\xff\",\"value\":\"\"}\n", 2, "\"key\" is not valid UTF-8; bytes that are not text go in \"key_hex\""),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\",\"value\":\"\xc3\"}\n", 2, "\"value\" is not valid UTF-8"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\tb\",\"value\":\"\"}\n", 2, "a control character (byte 0x09) inside a string"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\nb\",\"value\":\"\"}\n", 2, "the line ends inside a string"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key_hex\":\"abc\",\"value\":\"\"}\n", 2, "\"key_hex\" needs an even number of hex digits"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key_hex\":\"zz\",\"value\":\"\"}\n", 2, "\"key_hex\" holds 'z', which is not a hex digit"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key_hex\":\"\\u0030\",\"value\":\"\"}\n", 2, "holds '\\', which is not a hex digit"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\" \"value\":\"\"}\n", 2, "expected ',' or '}', found '\"'"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":\"a\",}\n", 2, "expected a field name in double quotes, found '}'"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\" \"a\"}\n", 2, "expected ':' after a field name"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"key\":a}\n", 2, "\"key\" must be a string in double quotes, found 'a'"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"entries\":-1}\n", 2, "must be a whole number"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"entries\":18446744073709551616}\n", 2, "\"entries\" is too large"),
        (b"{\"format\":\"eggdb-jsonl\",\"version\":1}\n{\"a_very_long_field_name_indeed\":1}\n", 2, "unknown field \"a_very_long_fiel"),
        (b"\xef\xbb\xbf{\"format\":\"eggdb-jsonl\",\"version\":1}\n", 1, "found byte 0xef"),
    ];
    for (input, line, problem) in cases {
        let (found_line, found_problem) = refused(input);
        assert!(
            found_problem.contains(problem) && found_line == *line,
            "{:?}: line {found_line}: {found_problem}",
            String::from_utf8_lossy(input)
        );
    }
}

#[test]
fn keys_and_values_stop_at_their_limits() {
    let key = "k".repeat(largest_key_bytes() + 1);
    let (line, problem) = refused(&with_header(&format!(
        "{{\"key\":\"{key}\",\"value\":\"\"}}\n"
    )));
    assert_eq!(line, 2);
    assert!(
        problem.starts_with("the key is longer than 8,192 bytes"),
        "{problem}"
    );
    let hex = "61".repeat(largest_key_bytes() + 1);
    let (_, problem) = refused(&with_header(&format!(
        "{{\"key_hex\":\"{hex}\",\"value\":\"\"}}\n"
    )));
    assert!(problem.starts_with("the key is longer than"), "{problem}");
    let escaped = "\\n".repeat(largest_key_bytes() + 1);
    let (_, problem) = refused(&with_header(&format!(
        "{{\"key\":\"{escaped}\",\"value\":\"\"}}\n"
    )));
    assert!(problem.starts_with("the key is longer than"), "{problem}");
    assert_eq!(BTree::MAX_VALUE_BYTES, 1 << 30);
}

#[test]
fn a_line_longer_than_any_entry_needs_is_refused() {
    let line = format!(
        "{HEADER}{{\"key\":\"a\",{}\"value\":\"b\"}}\n",
        " ".repeat(200_000)
    );
    let mut input = line.as_bytes();
    let mut reader = ExportReader::with_longest_line(&mut input, 100_000);
    reader.read_header().unwrap();
    match reader.next_entry() {
        Err(ReadError::Invalid { line, problem }) => {
            assert_eq!(line, 2);
            assert_eq!(
                problem,
                "the line is longer than 100,000 bytes, more than any entry needs"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_failing_input_is_an_input_error() {
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk on fire"))
        }
    }
    assert!(matches!(read_all(&mut Broken), Err(ReadError::Input(_))));
}

fn fuzz_cases() -> u64 {
    std::env::var("EGGDB_FUZZ_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(4000)
}

fn fuzz_seed() -> u64 {
    std::env::var("EGGDB_FUZZ_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x1e5f_ee0d_c0ff_ee01)
}

fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>) {
    const TOKENS: &[&[u8]] = &[
        b"\"",
        b"\\",
        b"\\u",
        b"\\ud800",
        b"{",
        b"}",
        b",",
        b":",
        b"\n",
        b"\r",
        b" ",
        b"0",
        b"-",
        b"1e9",
        b"\xff",
        b"\xc3",
        b"\x00",
        b"key",
        b"key_hex",
        b"value_hex",
        b"entries",
        b"\"format\":\"eggdb-jsonl\"",
        b"99999999999999999999",
    ];
    match rng.below(9) {
        0 if !bytes.is_empty() => {
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << rng.below(8);
        }
        1 if !bytes.is_empty() => {
            let at = rng.below(bytes.len());
            bytes[at] = rng.next() as u8;
        }
        2 => {
            let at = rng.below(bytes.len() + 1);
            bytes.truncate(at);
        }
        3 if !bytes.is_empty() => {
            let at = rng.below(bytes.len());
            let length = rng.below(16).min(bytes.len() - at);
            bytes.drain(at..at + length);
        }
        4 | 5 => {
            let at = rng.below(bytes.len() + 1);
            let token = TOKENS[rng.below(TOKENS.len())];
            bytes.splice(at..at, token.iter().copied());
        }
        6 => {
            let lines: Vec<Vec<u8>> = bytes
                .split(|byte| *byte == b'\n')
                .map(<[u8]>::to_vec)
                .collect();
            if lines.len() > 1 {
                let from = rng.below(lines.len());
                let at = rng.below(lines.len());
                let mut reordered = lines.clone();
                let line = reordered.remove(from);
                reordered.insert(at, line);
                *bytes = reordered.join(&b'\n');
            }
        }
        7 => {
            let at = rng.below(bytes.len() + 1);
            let length = rng.below(32);
            let noise = rng.bytes(length);
            bytes.splice(at..at, noise);
        }
        _ => {
            let at = rng.below(bytes.len() + 1);
            let copy: Vec<u8> = bytes[at..].iter().take(rng.below(64)).copied().collect();
            bytes.splice(at..at, copy);
        }
    }
}

#[test]
fn hostile_exports_end_in_an_error_or_entries_never_a_panic_or_a_hang() {
    let seed = fuzz_seed();
    let mut rng = Rng(seed | 1);
    let started = Instant::now();
    let mut accepted = 0u64;
    let mut refused = 0u64;
    for case in 0..fuzz_cases() {
        let entries: Entries = (0..rng.below(6))
            .map(|_| (rng.byte_string(24), rng.byte_string(80)))
            .collect();
        let mut bytes = if rng.below(10) == 0 {
            let length = rng.below(400);
            rng.bytes(length)
        } else {
            export(
                &Header {
                    page_size: Some(4096),
                    eggdb: Some("0.2.0".to_owned()),
                    exported_at: None,
                },
                &entries,
            )
        };
        for _ in 0..1 + rng.below(3) {
            mutate(&mut rng, &mut bytes);
        }
        let case_started = Instant::now();
        let mut input = Trickle {
            bytes: &bytes,
            rng: Rng(rng.next() | 1),
            interrupted: false,
        };
        match read_all(&mut input) {
            Ok((_, read)) => {
                accepted += 1;
                assert!(
                    read.iter()
                        .all(|(key, value)| key.len() <= largest_key_bytes()
                            && value.len() <= BTree::MAX_VALUE_BYTES),
                    "seed {seed} case {case}"
                );
            }
            Err(ReadError::Invalid { line, problem }) => {
                refused += 1;
                assert!(
                    line >= 1 && !problem.is_empty(),
                    "seed {seed} case {case}: line {line}: {problem:?}"
                );
            }
            Err(ReadError::Input(error)) => panic!("seed {seed} case {case}: {error}"),
        }
        assert!(
            case_started.elapsed() < Duration::from_secs(5),
            "seed {seed} case {case} took {:?}",
            case_started.elapsed()
        );
    }
    assert!(
        accepted > 0 && refused > 0,
        "{accepted} accepted, {refused} refused"
    );
    assert!(started.elapsed() < Duration::from_secs(120));
}
