//! The published test vectors of X25519 (D35): RFC 7748 sections 5.2 and 6.1, and every
//! Wycheproof case, whose low-order public keys must be refused by the shared-secret check.

use eggdb_crypto::{X25519LowOrderPoint, x25519, x25519_public_key, x25519_shared_secret};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn array(text: &str) -> [u8; 32] {
    let bytes: Vec<u8> = text
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    bytes.try_into().unwrap()
}

#[test]
fn x25519_matches_rfc_7748_section_5_2() {
    let cases = [
        (
            "a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4",
            "e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c",
            "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552",
        ),
        (
            "4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d",
            "e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493",
            "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957",
        ),
    ];
    for (scalar, u, output) in cases {
        assert_eq!(hex(&x25519(&array(scalar), &array(u))), output);
    }
}

#[test]
fn x25519_iterated_matches_rfc_7748_after_one_and_a_thousand_rounds() {
    let mut k = array("0900000000000000000000000000000000000000000000000000000000000000");
    let mut u = k;
    for round in 1..=1000 {
        let next = x25519(&k, &u);
        u = k;
        k = next;
        if round == 1 {
            assert_eq!(
                hex(&k),
                "422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079"
            );
        }
    }
    assert_eq!(
        hex(&k),
        "684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51"
    );
}

#[test]
fn the_diffie_hellman_example_of_rfc_7748_section_6_1_agrees() {
    let alice = array("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    let bob = array("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
    let alice_public = x25519_public_key(&alice);
    let bob_public = x25519_public_key(&bob);
    assert_eq!(
        hex(&alice_public),
        "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
    );
    assert_eq!(
        hex(&bob_public),
        "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"
    );
    let shared = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";
    assert_eq!(hex(&x25519_shared_secret(&alice, &bob_public).unwrap()), shared);
    assert_eq!(hex(&x25519_shared_secret(&bob, &alice_public).unwrap()), shared);
}

#[test]
fn x25519_passes_every_wycheproof_case_and_refuses_low_order_points() {
    let mut checked = 0;
    for line in include_str!("data/wycheproof_x25519.txt")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let fields: Vec<&str> = line.split(' ').collect();
        let [id, zero, private, public, shared] = fields[..] else {
            panic!("malformed vector line {line}");
        };
        let private = array(private);
        let public = array(public);
        assert_eq!(hex(&x25519(&private, &public)), shared, "case {id}");
        let outcome = x25519_shared_secret(&private, &public);
        if zero == "zero" {
            assert_eq!(outcome, Err(X25519LowOrderPoint), "case {id}");
        } else {
            assert_eq!(outcome.map(|secret| hex(&secret)), Ok(shared.to_string()), "case {id}");
        }
        checked += 1;
    }
    assert_eq!(checked, 518);
}
