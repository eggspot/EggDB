use crate::{
    ContentError, DecompressError, ZipArchive, crc32, extract_text, gunzip, gunzip_within_memory,
    inflate, zlib_inflate, zlib_inflate_within_memory,
};

const DYNAMIC_STREAM: [&str; 7] = [
    "1d515b0ac43008fcef29bc9a900185a01027f7dfc9b6c4b6d6c73c92037c2898031170041446871dfcd0040bcadd2f72",
    "ad6aa7a3ede669c082c9ab8eccba0be8ad737a0aab880b6ffacc05cf34a3a3c6358eeeb8f406e3aba4618d2eec01e187",
    "48ade3c3041f7a9586deb5b7ea91be55ef56fa6ffd7f29ff3e4ba1d26cbbf54038eb548982aad581adec703312b3b485",
    "02b1b84b4f716088917b9ba77f41d4b2307e34c37968a9fd69a6e87c9c0f70f4cd3ef66d73731e06e6245cfa0965d1b9",
    "a4a138326c32582936b90e26aded09fc2f942662f8494c3c31141e66dd0b19125e9e081ad9c2a66decc7e921b6ed0771",
    "bfb1764f52828b8a0b3c5eaab1d474e09529b35c5ea4f46da3ff97cbf341b76bd2e4e5db24c6e2fba6cbed11c5cfefc3",
    "61d219fbe9e3beee1ba24d12553d1b570edcb65e6b898c12ad06d92f7f7f",
];

const DOCX: [&str; 11] = [
    "504b0304140000000800d5ae5f5dee4758661f0000001d000000130000005b436f6e74656e745f54797065735d2e786d",
    "6cb3b1afc8cd51284b2d2acecccfb35532d43350b2b7b309a92c482dd6b70300504b0304140000000800d5ae5f5db517",
    "621dd80000004e01000011000000776f72642f646f63756d656e742e786d6c6d90414e03310c45af1265c18e66ca02a1",
    "61662a2140ec8103a4893b8d94d8911d3af4489c838b9188a282c4e6dbd6b79f2d0f9bf714d5015802e1a8d7ab4e2b40",
    "473ee03cead797c7cb1bada458f43612c2a88f207a330d4befc9bd25c0a22a00a55f46bd2f25f7c688db43b2b2a20c58",
    "bd1d71b2a5963c9b85d867260722959fa2b9eaba6b936c40dd905bf2c71673136e52a6278891d4605ade94cf5edbdc4b",
    "b6ae9e951904f8007a7a98e7fb3b756153be553b0e805efe4c9b13fff792677084fed456c56ecdb7110382fafcf80760",
    "7ece35e7574c5f504b01021403140000000800d5ae5f5dee4758661f0000001d00000013000000000000000000000080",
    "01000000005b436f6e74656e745f54797065735d2e786d6c504b01021403140000000800d5ae5f5db517621dd8000000",
    "4e010000110000000000000000000000800150000000776f72642f646f63756d656e742e786d6c504b05060000000002",
    "00020080000000570100000000",
];

const GZIP_TEXT: &str = "1f8b08000000000002ffcb48cdc9c957c840903a0ae95599050a8979290a29a969398925a90a99790aaee9e92e4e5c001422d2702d000000";
const GZIP_NAMED_TEXT: &str = "1f8b08080000000002ff6e6f7465732e74787400cb48cdc9c957c840903a0ae95599050a8979290a29a969398925a90a99790aaee9e92e4e5c001422d2702d000000";
const ZLIB_TEXT: &str =
    "78dacb48cdc9c957c840903a0ae95599050a8979290a29a969398925a90a99790aaee9e92e4e5c0076bc0f85";
const RAW_TEXT: &str =
    "cb48cdc9c957c840903a0ae95599050a8979290a29a969398925a90a99790aaee9e92e4e5c00";
const PLAIN_TEXT: &[u8] = b"hello hello hello, gzip and deflate in EggDB\n";

fn bytes(parts: &[&str]) -> Vec<u8> {
    let hex: String = parts.concat();
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
        .collect()
}

fn stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, contents) in files {
        let offset = out.len() as u32;
        let crc = crc32(contents);
        let size = contents.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(contents);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

#[test]
fn inflate_reads_stored_fixed_and_dynamic_blocks() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(
        inflate(&bytes(&["cb48cdc9c957c8409000"]), 100).unwrap(),
        b"hello hello hello"
    );
    assert_eq!(
        inflate(&[1, 3, 0, 0xfc, 0xff, b'a', b'b', b'c'], 100).unwrap(),
        b"abc"
    );
    let dynamic = inflate(&bytes(&DYNAMIC_STREAM), 1_000).unwrap();
    assert_eq!((dynamic.len(), crc32(&dynamic)), (600, 1_157_992_099));
}

#[test]
fn inflate_refuses_corrupt_streams_and_bombs() {
    let dynamic = bytes(&DYNAMIC_STREAM);
    assert!(inflate(&dynamic, 599).is_err());
    assert!(inflate(&dynamic[..dynamic.len() / 2], 1_000).is_err());
    assert!(inflate(&[], 10).is_err());
    assert!(inflate(&[0b111], 10).is_err());
    assert!(inflate(&[1, 3, 0, 0xfc, 0xfe, b'a', b'b', b'c'], 100).is_err());
    for seed in 0u32..2_000 {
        let noise: Vec<u8> = (0..64u32)
            .map(|at| {
                (seed
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add(at.wrapping_mul(40_503))
                    >> 13) as u8
            })
            .collect();
        let _ = inflate(&noise, 1 << 16);
    }
}

#[test]
fn office_documents_give_their_text() {
    assert_eq!(
        extract_text(&bytes(&DOCX), 1 << 20).unwrap(),
        "Hello EggDB & friends\nSecond\tline \u{e9}"
    );
    let odt = stored_zip(&[
        ("mimetype", b"application/vnd.oasis.opendocument.text"),
        ("content.xml", b"<office:document-content><office:body><office:text><text:h>Title</text:h><text:p>One<text:s text:c=\"2\"/>two<![CDATA[ <three> ]]></text:p><text:p/><text:p>&#x45;nd</text:p></office:text></office:body></office:document-content>"),
    ]);
    assert_eq!(
        extract_text(&odt, 1 << 20).unwrap(),
        "Title\nOne  two <three> \n\nEnd"
    );
    let pptx = stored_zip(&[
        (
            "ppt/slides/slide10.xml",
            b"<p:sld><a:p><a:r><a:t>Last</a:t></a:r></a:p></p:sld>",
        ),
        (
            "ppt/slides/slide2.xml",
            b"<p:sld><a:p><a:r><a:t>First</a:t></a:r></a:p></p:sld>",
        ),
    ]);
    assert_eq!(extract_text(&pptx, 1 << 20).unwrap(), "First\n\nLast");
    let xlsx = stored_zip(&[(
        "xl/sharedStrings.xml",
        b"<sst><si><t>Name</t></si><si><r><t>Ada</t></r><r><t> L.</t></r></si></sst>",
    )]);
    assert_eq!(extract_text(&xlsx, 1 << 20).unwrap(), "Name\nAda L.");
}

#[test]
fn archives_that_are_not_documents_or_are_damaged_are_refused() {
    assert!(extract_text(b"plain text", 1 << 20).is_err());
    assert!(extract_text(&stored_zip(&[("notes.txt", b"hi")]), 1 << 20).is_err());
    let mut damaged = stored_zip(&[("content.xml", b"<text:p>abc</text:p>")]);
    let at = damaged
        .windows(3)
        .position(|window| window == b"abc")
        .unwrap();
    damaged[at] = b'x';
    assert!(
        matches!(extract_text(&damaged, 1 << 20), Err(ContentError(message)) if message.contains("CRC"))
    );
    assert!(extract_text(&bytes(&DOCX), 16).is_err());
    let docx = bytes(&DOCX);
    let archive = ZipArchive::new(&docx).unwrap();
    assert_eq!(archive.entries.len(), 2);
    assert_eq!(archive.read("missing.xml", 100).unwrap(), None);
}

#[test]
fn every_part_of_a_document_shares_one_decompression_budget() {
    let slide = b"<p:sld><a:p><a:r><a:t>Slide text that repeats</a:t></a:r></a:p></p:sld>";
    let names: Vec<String> = (1..=200)
        .map(|number| format!("ppt/slides/slide{number}.xml"))
        .collect();
    let files: Vec<(&str, &[u8])> = names
        .iter()
        .map(|name| (name.as_str(), &slide[..]))
        .collect();
    let pptx = stored_zip(&files);
    assert!(extract_text(&pptx, 1 << 20).is_ok());
    assert!(extract_text(&pptx, 4096).is_err());
    let entities = format!("<sst><si><t>{}</t></si></sst>", "&".repeat(200_000));
    let xlsx = stored_zip(&[("xl/sharedStrings.xml", entities.as_bytes())]);
    assert_eq!(extract_text(&xlsx, 1 << 20).unwrap().len(), 200_000);
}

struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn mutated(valid: &[u8], random: &mut Seeded) -> Vec<u8> {
    let mut bytes = valid.to_vec();
    for _ in 0..=random.next() % 8 {
        if bytes.is_empty() {
            break;
        }
        let at = (random.next() % bytes.len() as u64) as usize;
        match random.next() % 4 {
            0 => bytes[at] ^= 1 << (random.next() % 8),
            1 => bytes[at] = random.next() as u8,
            2 => bytes.truncate(at),
            _ => bytes.insert(at, random.next() as u8),
        }
    }
    bytes
}

#[test]
fn mutated_streams_and_archives_are_errors_or_text_never_panics() {
    let cases: usize = std::env::var("EGGDB_FUZZ_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(20_000);
    let seed = std::env::var("EGGDB_FUZZ_SEED")
        .ok()
        .and_then(|seed| seed.parse().ok())
        .unwrap_or(0x5eed_c0de_u64);
    let mut random = Seeded(seed | 1);
    let dynamic = bytes(&DYNAMIC_STREAM);
    let docx = bytes(&DOCX);
    for _ in 0..cases {
        let _ = inflate(&mutated(&dynamic, &mut random), 1 << 16);
        let _ = extract_text(&mutated(&docx, &mut random), 1 << 16);
    }
}

fn gzip_member(deflated: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut member = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    member.extend_from_slice(deflated);
    member.extend_from_slice(&crc32(plain).to_le_bytes());
    member.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    member
}

fn zlib_stream(deflated: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut stream = vec![0x78, 0x9c];
    stream.extend_from_slice(deflated);
    stream.extend_from_slice(&crate::inflate::adler32(plain).to_be_bytes());
    stream
}

#[test]
fn gzip_members_from_the_gzip_tool_decompress_with_their_checks() {
    assert_eq!(gunzip(&bytes(&[GZIP_TEXT]), 100).unwrap(), PLAIN_TEXT);
    assert_eq!(gunzip(&bytes(&[GZIP_NAMED_TEXT]), 100).unwrap(), PLAIN_TEXT);
    let two = [bytes(&[GZIP_TEXT]), bytes(&[GZIP_NAMED_TEXT])].concat();
    assert_eq!(
        gunzip(&two, 200).unwrap(),
        [PLAIN_TEXT, PLAIN_TEXT].concat()
    );
    let mut flagged = bytes(&[GZIP_TEXT]);
    flagged[3] |= 0x02;
    let header_crc = (crc32(&flagged[..10]) as u16).to_le_bytes();
    let with_header_crc = [&flagged[..10], &header_crc[..], &flagged[10..]].concat();
    assert_eq!(gunzip(&with_header_crc, 100).unwrap(), PLAIN_TEXT);
    let mut flagged = bytes(&[GZIP_TEXT]);
    flagged[3] |= 0x04;
    let extra = [
        &flagged[..10],
        &[3u8, 0, b'a', b'b', b'c'][..],
        &flagged[10..],
    ]
    .concat();
    assert_eq!(gunzip(&extra, 100).unwrap(), PLAIN_TEXT);
    let dynamic = inflate(&bytes(&DYNAMIC_STREAM), 1_000).unwrap();
    let wrapped = gzip_member(&bytes(&DYNAMIC_STREAM), &dynamic);
    assert_eq!(gunzip(&wrapped, 600).unwrap(), dynamic);
    assert_eq!(gunzip(&wrapped, 599), Err(DecompressError::PastLimit(599)));
    let empty = gzip_member(&[3, 0], b"");
    assert_eq!(gunzip(&[empty, wrapped].concat(), 600).unwrap(), dynamic);
}

struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn bit(&mut self, set: bool) {
        if self.bits.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if set {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (self.bits % 8);
        }
        self.bits += 1;
    }

    fn value(&mut self, value: u32, count: u32) {
        for at in 0..count {
            self.bit((value >> at) & 1 == 1);
        }
    }

    fn huffman_code(&mut self, code: u32, length: u32) {
        for at in (0..length).rev() {
            self.bit((code >> at) & 1 == 1);
        }
    }
}

fn zeros_deflate(copies: usize) -> Vec<u8> {
    let mut out = BitWriter {
        bytes: Vec::new(),
        bits: 0,
    };
    out.value(1, 1);
    out.value(1, 2);
    out.huffman_code(0x30, 8);
    for _ in 0..copies {
        out.huffman_code(0b1100_0101, 8);
        out.huffman_code(0, 5);
    }
    out.huffman_code(0, 7);
    out.bytes
}

fn zeros(copies: usize) -> Vec<u8> {
    vec![0; 1 + 258 * copies]
}

#[test]
fn decompressed_output_never_reserves_past_its_limit() {
    let dynamic = inflate(&bytes(&DYNAMIC_STREAM), 1_000).unwrap();
    let length = dynamic.len();
    assert!(inflate(&bytes(&DYNAMIC_STREAM), length).unwrap().capacity() <= length);
    let wrapped = gzip_member(&bytes(&DYNAMIC_STREAM), &dynamic);
    assert!(gunzip(&wrapped, length).unwrap().capacity() <= length);
    let twice = gunzip(&[wrapped.clone(), wrapped].concat(), 2 * length).unwrap();
    assert_eq!(twice, [dynamic.clone(), dynamic.clone()].concat());
    assert!(twice.capacity() <= 2 * length);
    let zlib = zlib_stream(&bytes(&DYNAMIC_STREAM), &dynamic);
    assert!(zlib_inflate(&zlib, length).unwrap().capacity() <= length);
    let plain = zeros(4_000);
    let decoded = zlib_inflate(&zeros_deflate(4_000), plain.len()).unwrap();
    assert_eq!(decoded, plain);
    assert!(decoded.capacity() <= plain.len());
}

#[test]
fn output_past_its_memory_is_only_counted_to_tell_the_memory_from_the_limit() {
    let plain = zeros(4_000);
    let bomb = gzip_member(&zeros_deflate(4_000), &plain);
    assert_eq!(
        gunzip_within_memory(&bomb, 2_000_000, 10_000),
        Err(DecompressError::PastMemory(10_000))
    );
    assert_eq!(
        gunzip_within_memory(&bomb, 1_000_000, 10_000),
        Err(DecompressError::PastLimit(1_000_000))
    );
    assert_eq!(
        gunzip_within_memory(&bomb, plain.len(), plain.len() - 1),
        Err(DecompressError::PastMemory(plain.len() - 1))
    );
    assert_eq!(
        gunzip_within_memory(&bomb, plain.len() - 1, 10),
        Err(DecompressError::PastLimit(plain.len() - 1))
    );
    assert_eq!(
        gunzip_within_memory(&bomb, plain.len(), plain.len()).unwrap(),
        plain
    );
    let small = gzip_member(&zeros_deflate(10), &zeros(10));
    let both = [small.clone(), bomb.clone(), small].concat();
    assert_eq!(
        gunzip_within_memory(&both, 2_000_000, 10_000),
        Err(DecompressError::PastMemory(10_000))
    );
    assert_eq!(
        gunzip_within_memory(&both, plain.len() + 2_000, 10_000),
        Err(DecompressError::PastLimit(plain.len() + 2_000))
    );
    let mut wrong_size = bomb.clone();
    let size_at = wrong_size.len() - 4;
    wrong_size[size_at] ^= 1;
    assert!(matches!(
        gunzip_within_memory(&wrong_size, 2_000_000, 10_000),
        Err(DecompressError::Corrupt(_))
    ));
    let zlib = zlib_stream(&zeros_deflate(4_000), &plain);
    assert_eq!(
        zlib_inflate_within_memory(&zlib, 2_000_000, 10_000),
        Err(DecompressError::PastMemory(10_000))
    );
    assert_eq!(
        zlib_inflate_within_memory(&zlib, 1_000_000, 10_000),
        Err(DecompressError::PastLimit(1_000_000))
    );
    assert_eq!(
        zlib_inflate_within_memory(&zeros_deflate(4_000), 2_000_000, 10_000),
        Err(DecompressError::PastMemory(10_000))
    );
    assert_eq!(
        zlib_inflate_within_memory(&zlib, 2_000_000, 2_000_000).unwrap(),
        plain
    );
}

#[test]
fn damaged_gzip_is_refused_as_corrupt() {
    let valid = bytes(&[GZIP_TEXT]);
    let corrupt = |data: &[u8]| matches!(gunzip(data, 1_000), Err(DecompressError::Corrupt(_)));
    let mut wrong_crc = valid.clone();
    let crc_at = wrong_crc.len() - 8;
    wrong_crc[crc_at] ^= 1;
    let mut wrong_size = valid.clone();
    let size_at = wrong_size.len() - 4;
    wrong_size[size_at] ^= 1;
    let mut reserved_flag = valid.clone();
    reserved_flag[3] |= 0x20;
    let mut other_method = valid.clone();
    other_method[2] = 7;
    let mut flagged = valid.clone();
    flagged[3] |= 0x02;
    let wrong_header_crc = [&flagged[..10], &[0u8, 0][..], &flagged[10..]].concat();
    for damaged in [
        wrong_crc,
        wrong_size,
        reserved_flag,
        other_method,
        wrong_header_crc,
        valid[..valid.len() - 1].to_vec(),
        valid[..9].to_vec(),
        [&valid[..], &b"x"[..]].concat(),
        [&valid[..], &[0x1f_u8, 0x8b][..]].concat(),
        Vec::new(),
        PLAIN_TEXT.to_vec(),
    ] {
        assert!(corrupt(&damaged), "{damaged:?}");
    }
    let two = [valid.clone(), valid].concat();
    assert_eq!(
        gunzip(&two, PLAIN_TEXT.len() * 2 - 1),
        Err(DecompressError::PastLimit(PLAIN_TEXT.len() * 2 - 1))
    );
}

#[test]
fn deflate_bodies_decompress_as_zlib_or_raw_deflate() {
    assert_eq!(crate::inflate::adler32(b"Wikipedia"), 0x11E6_0398);
    assert_eq!(zlib_inflate(&bytes(&[ZLIB_TEXT]), 100).unwrap(), PLAIN_TEXT);
    assert_eq!(zlib_inflate(&bytes(&[RAW_TEXT]), 100).unwrap(), PLAIN_TEXT);
    let dynamic = inflate(&bytes(&DYNAMIC_STREAM), 1_000).unwrap();
    let wrapped = zlib_stream(&bytes(&DYNAMIC_STREAM), &dynamic);
    assert_eq!(zlib_inflate(&wrapped, 600).unwrap(), dynamic);
    assert_eq!(
        zlib_inflate(&wrapped, 599),
        Err(DecompressError::PastLimit(599))
    );
    assert_eq!(
        zlib_inflate(&bytes(&DYNAMIC_STREAM), 599),
        Err(DecompressError::PastLimit(599))
    );
    let valid = bytes(&[ZLIB_TEXT]);
    let mut wrong_adler = valid.clone();
    let adler_at = wrong_adler.len() - 1;
    wrong_adler[adler_at] ^= 1;
    let mut preset_dictionary = valid.clone();
    preset_dictionary[1] = 0xbb;
    for damaged in [
        wrong_adler,
        preset_dictionary,
        valid[..valid.len() - 2].to_vec(),
        [&valid[..], &b"x"[..]].concat(),
        [&bytes(&[RAW_TEXT])[..], &b"x"[..]].concat(),
        Vec::new(),
        vec![0x78],
    ] {
        assert!(
            matches!(
                zlib_inflate(&damaged, 1_000),
                Err(DecompressError::Corrupt(_))
            ),
            "{damaged:?}"
        );
    }
}

#[test]
fn mutated_gzip_and_zlib_bodies_are_errors_or_bytes_never_panics() {
    let cases: usize = std::env::var("EGGDB_FUZZ_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(20_000);
    let mut random = Seeded(0x9e37_79b9_7f4a_7c15);
    let dynamic = inflate(&bytes(&DYNAMIC_STREAM), 1_000).unwrap();
    let gzip = [
        bytes(&[GZIP_NAMED_TEXT]),
        gzip_member(&bytes(&DYNAMIC_STREAM), &dynamic),
    ]
    .concat();
    let zlib = zlib_stream(&bytes(&DYNAMIC_STREAM), &dynamic);
    for _ in 0..cases {
        let _ = gunzip(&mutated(&gzip, &mut random), 1 << 16);
        let _ = zlib_inflate(&mutated(&zlib, &mut random), 1 << 16);
        let _ = zlib_inflate(&mutated(&bytes(&[RAW_TEXT]), &mut random), 1 << 16);
    }
}
