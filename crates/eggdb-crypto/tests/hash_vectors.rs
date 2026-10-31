//! The published test vectors of the SHA-2 family's longer hashes and of the keyed functions TLS
//! builds on (D35): FIPS 180-4's examples for SHA-512 and SHA-384, RFC 4231 for HMAC-SHA-384 and
//! HMAC-SHA-512, RFC 5869 for HKDF-SHA-256.

use eggdb_crypto::{
    HashFunction, Hmac, HkdfError, Sha256, Sha384, Sha512, hkdf_expand, hkdf_extract, hmac,
    sha384, sha512,
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

const TWO_BLOCK_MESSAGE: &[u8] = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";

#[test]
fn sha512_matches_the_fips_180_examples() {
    let cases: [(&[u8], &str); 3] = [
        (
            b"",
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
        ),
        (
            b"abc",
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        ),
        (
            TWO_BLOCK_MESSAGE,
            "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909",
        ),
    ];
    for (message, digest) in cases {
        assert_eq!(hex(&sha512(message)), digest);
    }
}

#[test]
fn sha384_matches_the_fips_180_examples() {
    let cases: [(&[u8], &str); 3] = [
        (
            b"",
            "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b",
        ),
        (
            b"abc",
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
        ),
        (
            TWO_BLOCK_MESSAGE,
            "09330c33f71147e83d192fc782cd1b4753111b173b3b05d22fa08086e3b0f712fcc7c71a557e2db966c3e9fa91746039",
        ),
    ];
    for (message, digest) in cases {
        assert_eq!(hex(&sha384(message)), digest);
    }
}

#[test]
fn sha512_and_sha384_of_a_million_a_match_the_nist_long_message() {
    let chunk = [b'a'; 1000];
    let mut long512 = Sha512::new();
    let mut long384 = Sha384::new();
    for _ in 0..1000 {
        long512.update(&chunk);
        long384.update(&chunk);
    }
    assert_eq!(
        hex(&long512.finish()),
        "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"
    );
    assert_eq!(
        hex(&long384.finish()),
        "9d0e1809716474cb086e834e310a4a1ced149e9c00f248527972cec5704c2a5b07b8b3dc38ecc4ebae97ddd87f3d8985"
    );
}

#[test]
fn sha512_gives_the_same_digest_however_the_message_is_split() {
    let message: Vec<u8> = (0..600u32).map(|index| (index * 7 + 3) as u8).collect();
    for length in [0, 1, 111, 112, 127, 128, 129, 239, 240, 256, 600] {
        let whole = sha512(&message[..length]);
        for split in [1, 3, 17, 127, 128, 129] {
            let mut hash = Sha512::new();
            for piece in message[..length].chunks(split) {
                hash.update(piece);
            }
            assert_eq!(hash.finish(), whole, "length {length}, pieces of {split}");
        }
    }
}

#[test]
fn the_hash_trait_digests_like_each_hash() {
    assert_eq!(Sha512::digest(b"abc"), sha512(b"abc"));
    assert_eq!(Sha384::digest(b"abc"), sha384(b"abc"));
    assert_eq!(Sha256::digest(b"abc"), eggdb_crypto::sha256(b"abc"));
    assert_eq!(<Sha384 as HashFunction>::DIGEST_BYTES, 48);
    assert_eq!(<Sha512 as HashFunction>::BLOCK_BYTES, 128);
}

struct HmacCase {
    key: Vec<u8>,
    data: Vec<u8>,
    sha384: &'static str,
    sha512: &'static str,
}

fn rfc_4231_cases() -> Vec<HmacCase> {
    vec![
        HmacCase {
            key: vec![0x0b; 20],
            data: b"Hi There".to_vec(),
            sha384: "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6",
            sha512: "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854",
        },
        HmacCase {
            key: b"Jefe".to_vec(),
            data: b"what do ya want for nothing?".to_vec(),
            sha384: "af45d2e376484031617f78d2b58a6b1b9c7ef464f5a01b47e42ec3736322445e8e2240ca5e69e2c78b3239ecfab21649",
            sha512: "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea2505549758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737",
        },
        HmacCase {
            key: vec![0xaa; 131],
            data: b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
            sha384: "4ece084485813e9088d2c63a041bc5b44f9ef1012a2b588f3cd11f05033ac4c60c2ef6ab4030fe8296248df163f44952",
            sha512: "80b24263c7c1a3ebb71493c1dd7be8b49b46d1f41b4aeec1121b013783f8f3526b56d037e05f2598bd0fd2215d6a1e5295e64f73f63f0aec8b915a985d786598",
        },
        HmacCase {
            key: vec![0xaa; 131],
            data: b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm.".to_vec(),
            sha384: "6617178e941f020d351e2f254e8fd32c602420feb0b8fb9adccebb82461e99c5a678cc31e799176d3860e6110c46523e",
            sha512: "e37b6a775dc87dbaa4dfa9f96e5e3ffddebd71f8867289865df5a32d20cdc944b6022cac3c4982b10d5eeb55c3e4de15134676fb6de0446065c97440fa8c6a58",
        },
    ]
}

#[test]
fn hmac_sha384_and_sha512_match_rfc_4231() {
    for case in rfc_4231_cases() {
        assert_eq!(hex(&hmac::<Sha384>(&case.key, &case.data)), case.sha384);
        assert_eq!(hex(&hmac::<Sha512>(&case.key, &case.data)), case.sha512);
        let mut pieces = Hmac::<Sha512>::new(&case.key);
        for piece in case.data.chunks(5) {
            pieces.update(piece);
        }
        assert_eq!(hex(&pieces.finish()), case.sha512);
    }
}

struct HkdfCase {
    input_key: Vec<u8>,
    salt: Vec<u8>,
    info: Vec<u8>,
    prk: &'static str,
    okm: &'static str,
}

fn rfc_5869_cases() -> Vec<HkdfCase> {
    vec![
        HkdfCase {
            input_key: vec![0x0b; 22],
            salt: (0x00..=0x0c).collect(),
            info: (0xf0..=0xf9).collect(),
            prk: "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5",
            okm: "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
        },
        HkdfCase {
            input_key: (0x00..=0x4f).collect(),
            salt: (0x60..=0xaf).collect(),
            info: (0xb0..=0xff).collect(),
            prk: "06a6b88c5853361a06104c9ceb35b45cef760014904671014a193f40c15fc244",
            okm: "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71cc30c58179ec3e87c14c01d5c1f3434f1d87",
        },
        HkdfCase {
            input_key: vec![0x0b; 22],
            salt: Vec::new(),
            info: Vec::new(),
            prk: "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04",
            okm: "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8",
        },
    ]
}

#[test]
fn hkdf_sha256_matches_rfc_5869() {
    for case in rfc_5869_cases() {
        let prk = hkdf_extract::<Sha256>(&case.salt, &case.input_key);
        assert_eq!(hex(&prk), case.prk);
        let mut okm = vec![0u8; unhex(case.okm).len()];
        hkdf_expand::<Sha256>(&prk, &case.info, &mut okm).unwrap();
        assert_eq!(hex(&okm), case.okm);
    }
}

#[test]
fn hkdf_refuses_more_than_255_blocks() {
    let prk = hkdf_extract::<Sha256>(b"salt", b"key");
    let mut longest = vec![0u8; 255 * 32];
    assert_eq!(hkdf_expand::<Sha256>(&prk, b"", &mut longest), Ok(()));
    let mut too_long = vec![0u8; 255 * 32 + 1];
    assert_eq!(
        hkdf_expand::<Sha256>(&prk, b"", &mut too_long),
        Err(HkdfError::OutputTooLong)
    );
    let mut empty = [0u8; 0];
    assert_eq!(hkdf_expand::<Sha384>(&prk, b"", &mut empty), Ok(()));
}
