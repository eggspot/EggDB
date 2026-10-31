//! The published test vectors of ChaCha20, Poly1305 and AEAD_CHACHA20_POLY1305 (D35): every
//! example of RFC 8439 section 2 and its appendix A.3 (including the reduction edge cases) and
//! A.5.

use eggdb_crypto::{
    AEAD_TAG_BYTES, AeadError, ChaCha20, ChaCha20Poly1305, Poly1305, poly1305,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn array<const N: usize>(text: &str) -> [u8; N] {
    unhex(text).try_into().unwrap()
}

fn sequence_key(first: u8) -> [u8; 32] {
    std::array::from_fn(|index| first + index as u8)
}

const SUNSCREEN: &str = concat!(
    "4c616469657320616e642047656e746c656d656e206f662074686520636c617373206f6620273939",
    "3a204966204920636f756c64206f6666657220796f75206f6e6c79206f6e652074697020666f7220",
    "746865206675747572652c2073756e73637265656e20776f756c642062652069742e",
);

#[test]
fn the_chacha20_block_matches_rfc_8439_section_2_3_2() {
    let cipher = ChaCha20::new(&sequence_key(0), &array("000000090000004a00000000"));
    assert_eq!(
        hex(&cipher.block(1)),
        concat!(
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4ed2826446079faa09",
            "14c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e",
        )
    );
}

#[test]
fn chacha20_encryption_matches_rfc_8439_section_2_4_2() {
    let cipher = ChaCha20::new(&sequence_key(0), &array("000000000000004a00000000"));
    let mut data = unhex(SUNSCREEN);
    cipher.apply_keystream(1, &mut data);
    assert_eq!(
        hex(&data),
        concat!(
            "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0bf91b65c5524733ab",
            "8f593dabcd62b3571639d624e65152ab8f530c359f0861d807ca0dbf500d6a6156a38e088a22b65e",
            "52bc514d16ccf806818ce91ab77937365af90bbf74a35be6b40b8eedf2785e42874d",
        )
    );
    cipher.apply_keystream(1, &mut data);
    assert_eq!(hex(&data), SUNSCREEN);
}

#[test]
fn poly1305_matches_rfc_8439_section_2_5_2() {
    let key = array("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
    assert_eq!(
        hex(&poly1305(&key, b"Cryptographic Forum Research Group")),
        "a8061dc1305136c6c22b8baf0c0127a9"
    );
}

#[test]
fn poly1305_matches_rfc_8439_appendix_a_3_including_its_edge_cases() {
    let cases: [(&str, &str, &str); 11] = [
        (
            "0000000000000000000000000000000000000000000000000000000000000000",
            concat!(
                "00000000000000000000000000000000000000000000000000000000000000000000000000000000",
                "000000000000000000000000000000000000000000000000",
            ),
            "00000000000000000000000000000000",
        ),
        (
            "0000000000000000000000000000000036e5f6b5c5e06070f0efca96227a863e",
            concat!(
                "416e79207375626d697373696f6e20746f20746865204945544620696e74656e6465642062792074",
                "686520436f6e7472696275746f7220666f72207075626c69636174696f6e20617320616c6c206f72",
                "2070617274206f6620616e204945544620496e7465726e65742d4472616674206f72205246432061",
                "6e6420616e792073746174656d656e74206d6164652077697468696e2074686520636f6e74657874",
                "206f6620616e204945544620616374697669747920697320636f6e7369646572656420616e202249",
                "45544620436f6e747269627574696f6e222e20537563682073746174656d656e747320696e636c75",
                "6465206f72616c2073746174656d656e747320696e20494554462073657373696f6e732c20617320",
                "77656c6c206173207772697474656e20616e6420656c656374726f6e696320636f6d6d756e696361",
                "74696f6e73206d61646520617420616e792074696d65206f7220706c6163652c2077686963682061",
                "72652061646472657373656420746f",
            ),
            "36e5f6b5c5e06070f0efca96227a863e",
        ),
        (
            "36e5f6b5c5e06070f0efca96227a863e00000000000000000000000000000000",
            concat!(
                "416e79207375626d697373696f6e20746f20746865204945544620696e74656e6465642062792074",
                "686520436f6e7472696275746f7220666f72207075626c69636174696f6e20617320616c6c206f72",
                "2070617274206f6620616e204945544620496e7465726e65742d4472616674206f72205246432061",
                "6e6420616e792073746174656d656e74206d6164652077697468696e2074686520636f6e74657874",
                "206f6620616e204945544620616374697669747920697320636f6e7369646572656420616e202249",
                "45544620436f6e747269627574696f6e222e20537563682073746174656d656e747320696e636c75",
                "6465206f72616c2073746174656d656e747320696e20494554462073657373696f6e732c20617320",
                "77656c6c206173207772697474656e20616e6420656c656374726f6e696320636f6d6d756e696361",
                "74696f6e73206d61646520617420616e792074696d65206f7220706c6163652c2077686963682061",
                "72652061646472657373656420746f",
            ),
            "f3477e7cd95417af89a6b8794c310cf0",
        ),
        (
            "1c9240a5eb55d38af333888604f6b5f0473917c1402b80099dca5cbc207075c0",
            concat!(
                "2754776173206272696c6c69672c20616e642074686520736c6974687920746f7665730a44696420",
                "6779726520616e642067696d626c6520696e2074686520776162653a0a416c6c206d696d73792077",
                "6572652074686520626f726f676f7665732c0a416e6420746865206d6f6d65207261746873206f75",
                "7467726162652e",
            ),
            "4541669a7eaaee61e708dc7cbcc5eb62",
        ),
        (
            "0200000000000000000000000000000000000000000000000000000000000000",
            "ffffffffffffffffffffffffffffffff",
            "03000000000000000000000000000000",
        ),
        (
            "02000000000000000000000000000000ffffffffffffffffffffffffffffffff",
            "02000000000000000000000000000000",
            "03000000000000000000000000000000",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            concat!(
                "fffffffffffffffffffffffffffffffff0ffffffffffffffffffffffffffffff1100000000000000",
                "0000000000000000",
            ),
            "05000000000000000000000000000000",
        ),
        (
            "0100000000000000000000000000000000000000000000000000000000000000",
            concat!(
                "fffffffffffffffffffffffffffffffffbfefefefefefefefefefefefefefefe0101010101010101",
                "0101010101010101",
            ),
            "00000000000000000000000000000000",
        ),
        (
            "0200000000000000000000000000000000000000000000000000000000000000",
            "fdffffffffffffffffffffffffffffff",
            "faffffffffffffffffffffffffffffff",
        ),
        (
            "0100000000000000040000000000000000000000000000000000000000000000",
            concat!(
                "e33594d7505e43b900000000000000003394d7505e4379cd01000000000000000000000000000000",
                "000000000000000001000000000000000000000000000000",
            ),
            "14000000000000005500000000000000",
        ),
        (
            "0100000000000000040000000000000000000000000000000000000000000000",
            concat!(
                "e33594d7505e43b900000000000000003394d7505e4379cd01000000000000000000000000000000",
                "0000000000000000",
            ),
            "13000000000000000000000000000000",
        ),
    ];
    for (index, (key, message, tag)) in cases.into_iter().enumerate() {
        let key = array(key);
        let message = unhex(message);
        assert_eq!(hex(&poly1305(&key, &message)), tag, "case {}", index + 1);
        for split in [1, 7, 16, 17] {
            let mut pieces = Poly1305::new(&key);
            for piece in message.chunks(split) {
                pieces.update(piece);
            }
            assert_eq!(hex(&pieces.finish()), tag, "case {} in pieces of {split}", index + 1);
        }
    }
}

#[test]
fn the_poly1305_key_generation_matches_rfc_8439_section_2_6_2() {
    let cipher = ChaCha20::new(&sequence_key(0x80), &array("000000000001020304050607"));
    assert_eq!(
        hex(&cipher.block(0)[..32]),
        "8ad5a08b905f81cc815040274ab29471a833b637e3fd0da508dbb8e2fdd1a646"
    );
}

#[test]
fn chacha20_poly1305_matches_rfc_8439_section_2_8_2() {
    let aead = ChaCha20Poly1305::new(&sequence_key(0x80));
    let nonce = array("070000004041424344454647");
    let associated_data = unhex("50515253c0c1c2c3c4c5c6c7");
    let mut buffer = unhex(SUNSCREEN);
    let tag = aead.seal_in_place(&nonce, &associated_data, &mut buffer);
    assert_eq!(
        hex(&buffer),
        concat!(
            "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca96712",
            "82fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58",
            "fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b6116",
        )
    );
    assert_eq!(hex(&tag), "1ae10b594f09e26a7e902ecbd0600691");
    aead.open_in_place(&nonce, &associated_data, &mut buffer, &tag)
        .unwrap();
    assert_eq!(hex(&buffer), SUNSCREEN);
}

#[test]
fn chacha20_poly1305_opens_rfc_8439_appendix_a_5() {
    let aead = ChaCha20Poly1305::new(&array("1c9240a5eb55d38af333888604f6b5f0473917c1402b80099dca5cbc207075c0"));
    let nonce = array("000000000102030405060708");
    let associated_data = unhex("f33388860000000000004e91");
    let tag: [u8; AEAD_TAG_BYTES] = array("eead9d67890cbb22392336fea1851f38");
    let mut buffer = unhex(
        concat!(
            "64a0861575861af460f062c79be643bd5e805cfd345cf389f108670ac76c8cb24c6cfc18755d43ee",
            "a09ee94e382d26b0bdb7b73c321b0100d4f03b7f355894cf332f830e710b97ce98c8a84abd0b9481",
            "14ad176e008d33bd60f982b1ff37c8559797a06ef4f0ef61c186324e2b3506383606907b6a7c02b0",
            "f9f6157b53c867e4b9166c767b804d46a59b5216cde7a4e99040c5a40433225ee282a1b0a06c523e",
            "af4534d7f83fa1155b0047718cbc546a0d072b04b3564eea1b422273f548271a0bb2316053fa7699",
            "1955ebd63159434ecebb4e466dae5a1073a6727627097a1049e617d91d361094fa68f0ff77987130",
            "305beaba2eda04df997b714d6c6f2c29a6ad5cb4022b02709b",
        ),
    );
    aead.open_in_place(&nonce, &associated_data, &mut buffer, &tag)
        .unwrap();
    assert_eq!(
        hex(&buffer),
        concat!(
            "496e7465726e65742d4472616674732061726520647261667420646f63756d656e74732076616c69",
            "6420666f722061206d6178696d756d206f6620736978206d6f6e74687320616e64206d6179206265",
            "20757064617465642c207265706c616365642c206f72206f62736f6c65746564206279206f746865",
            "7220646f63756d656e747320617420616e792074696d652e20497420697320696e617070726f7072",
            "6961746520746f2075736520496e7465726e65742d447261667473206173207265666572656e6365",
            "206d6174657269616c206f7220746f2063697465207468656d206f74686572207468616e20617320",
            "2fe2809c776f726b20696e2070726f67726573732e2fe2809d",
        )
    );
}

#[test]
fn chacha20_poly1305_refuses_any_altered_byte_and_leaves_the_ciphertext() {
    let aead = ChaCha20Poly1305::new(&sequence_key(0x80));
    let nonce = array("070000004041424344454647");
    let mut sealed = b"a record of the wire protocol".to_vec();
    let tag = aead.seal_in_place(&nonce, b"header", &mut sealed);
    for index in 0..sealed.len() {
        let mut altered = sealed.clone();
        altered[index] ^= 0x01;
        let before = altered.clone();
        assert_eq!(
            aead.open_in_place(&nonce, b"header", &mut altered, &tag),
            Err(AeadError)
        );
        assert_eq!(altered, before);
    }
    for index in 0..AEAD_TAG_BYTES {
        let mut altered_tag = tag;
        altered_tag[index] ^= 0x80;
        let mut copy = sealed.clone();
        assert_eq!(
            aead.open_in_place(&nonce, b"header", &mut copy, &altered_tag),
            Err(AeadError)
        );
    }
    let mut copy = sealed.clone();
    assert_eq!(
        aead.open_in_place(&nonce, b"headex", &mut copy, &tag),
        Err(AeadError)
    );
    let mut copy = sealed.clone();
    assert_eq!(
        aead.open_in_place(&array("070000004041424344454648"), b"header", &mut copy, &tag),
        Err(AeadError)
    );
}

