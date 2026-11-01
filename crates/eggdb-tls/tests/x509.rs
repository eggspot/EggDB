//! X.509 certificates, private keys, self-signed certificates and chain verification, against
//! the publicly known test keys of RFC 9500 and a chain OpenSSL 3.6 issued with them
//! (`tests/data/x509`: an RSA root, a P-256 intermediate it signed with RSA-PSS, and a P-256
//! server certificate the intermediate signed with ECDSA); hostile input is refused with an
//! error, never a panic.

use std::net::IpAddr;

use eggdb_crypto::{P256PrivateKey, P256PublicKey};
use eggdb_tls::oid;
use eggdb_tls::private_key::{PrivateKey, PrivateKeyError};
use eggdb_tls::self_signed::{
    SelfSignedCertificateError, SelfSignedCertificateRequest, generate_self_signed_certificate,
    self_signed_certificate,
};
use eggdb_tls::signature::{SignatureAlgorithm, SignatureHash, SubjectPublicKey, verify_signature};
use eggdb_tls::verify::{
    CertificateVerificationError, KeyPurpose, ServerName, TrustStore, verify_certificate_chain,
    verify_server_name,
};
use eggdb_tls::x509::{
    BasicConstraints, Certificate, CertificateError, KeyUsage, SubjectAlternativeName,
    certificates_from_pem,
};

const ROOT_PEM: &str = include_str!("data/x509/root_ca_rsa.pem");
const INTERMEDIATE_PEM: &str = include_str!("data/x509/intermediate_ca_p256_pss.pem");
const LEAF_PEM: &str = include_str!("data/x509/leaf_p256.pem");
const CN_ONLY_PEM: &str = include_str!("data/x509/cn_only_self_signed_p256.pem");
const P256_SEC1_PEM: &str = include_str!("data/x509/rfc9500_p256.sec1.pem");
const P256_PKCS8_PEM: &str = include_str!("data/x509/rfc9500_p256.pkcs8.pem");
const RSA_PKCS1_PEM: &str = include_str!("data/x509/rfc9500_rsa2048.pkcs1.pem");
const RSA_PKCS8_PEM: &str = include_str!("data/x509/rfc9500_rsa2048.pkcs8.pem");
const INTERMEDIATE_KEY_PEM: &str = include_str!("data/x509/intermediate_ca_p256.pkcs8.pem");

const RFC9500_P256_D: &str = "e6cb5bdd80aa45ae9c95e8c15476679ffec953c16851e711e743939589c64fc1";
const RFC9500_P256_QX: &str = "422548f88fb782ffb5eca3744452c72a1e558fbd6f73be5e48e93232cc45c5b1";
const RFC9500_P256_QY: &str = "6c4cd10c4cb8d5b8a17139e94882c8992572993425f41419ab7e90a42a494272";
const RFC9500_RSA2048_MODULUS_PREFIX: &str = "b0f9e81943a7ae9892aade17ca7c40f8";

const JANUARY_2026: i64 = 1_767_225_600;
const JANUARY_2027: i64 = 1_798_761_600;
const JANUARY_2036: i64 = 2_082_758_400;
const JANUARY_2046: i64 = 2_398_377_600;
const JANUARY_2051: i64 = 2_556_144_000;
const MID_2026: i64 = 1_780_000_000;

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn one_certificate(pem: &str) -> Certificate {
    let mut certificates = certificates_from_pem(pem).unwrap();
    assert_eq!(certificates.len(), 1);
    certificates.remove(0)
}

fn root() -> Certificate {
    one_certificate(ROOT_PEM)
}

fn intermediate() -> Certificate {
    one_certificate(INTERMEDIATE_PEM)
}

fn leaf() -> Certificate {
    one_certificate(LEAF_PEM)
}

fn store_of(certificates: &[Certificate]) -> TrustStore {
    let mut store = TrustStore::new();
    for certificate in certificates {
        store.add(certificate.clone());
    }
    store
}

fn rfc9500_p256_public_key() -> P256PublicKey {
    let mut point = vec![0x04];
    point.extend(hex(RFC9500_P256_QX));
    point.extend(hex(RFC9500_P256_QY));
    P256PublicKey::from_sec1(&point).unwrap()
}

fn rfc9500_p256_private_key() -> P256PrivateKey {
    match PrivateKey::from_pem(P256_SEC1_PEM).unwrap() {
        PrivateKey::P256(key) => key,
        PrivateKey::Rsa(_) => panic!("the RFC 9500 P-256 key read as RSA"),
    }
}

fn counter_random() -> impl FnMut(&mut [u8]) -> std::io::Result<()> {
    let mut counter = 0u8;
    move |bytes: &mut [u8]| {
        for byte in bytes.iter_mut() {
            counter = counter.wrapping_add(37);
            *byte = counter;
        }
        Ok(())
    }
}

#[test]
fn rfc9500_p256_key_reads_the_same_from_sec1_and_pkcs8() {
    let sec1 = rfc9500_p256_private_key();
    let PrivateKey::P256(pkcs8) = PrivateKey::from_pem(P256_PKCS8_PEM).unwrap() else {
        panic!("the PKCS#8 P-256 key read as RSA");
    };
    assert_eq!(sec1.to_bytes().to_vec(), hex(RFC9500_P256_D));
    assert_eq!(pkcs8.to_bytes(), sec1.to_bytes());
    assert_eq!(sec1.public_key(), rfc9500_p256_public_key());
}

#[test]
fn rfc9500_rsa_key_reads_the_same_from_pkcs1_and_pkcs8() {
    let pkcs1 = PrivateKey::from_pem(RSA_PKCS1_PEM).unwrap();
    let pkcs8 = PrivateKey::from_pem(RSA_PKCS8_PEM).unwrap();
    let (SubjectPublicKey::Rsa(pkcs1_public), SubjectPublicKey::Rsa(pkcs8_public)) =
        (pkcs1.public_key(), pkcs8.public_key())
    else {
        panic!("the RFC 9500 RSA key read as another type");
    };
    assert_eq!(pkcs1_public, pkcs8_public);
    assert_eq!(pkcs1_public.modulus_bits(), 2048);
    assert!(
        pkcs1_public
            .modulus()
            .starts_with(&hex(RFC9500_RSA2048_MODULUS_PREFIX))
    );
    assert_eq!(pkcs1_public.public_exponent(), vec![0x01, 0x00, 0x01]);
    assert!(pkcs1.to_pkcs8_der().is_none());
}

#[test]
fn the_root_certificate_reads_with_its_fields() {
    let root = root();
    assert_eq!(root.serial_number(), &[0x01]);
    assert_eq!(
        root.signature_algorithm(),
        SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha256)
    );
    assert_eq!(root.subject(), root.issuer());
    assert_eq!(root.subject().common_names(), ["EggDB Test Root CA"]);
    assert_eq!(root.not_before_unix_seconds(), JANUARY_2026);
    assert_eq!(root.not_after_unix_seconds(), JANUARY_2046);
    assert_eq!(
        root.basic_constraints(),
        Some(BasicConstraints {
            certificate_authority: true,
            path_length: None
        })
    );
    let usage = root.key_usage().unwrap();
    assert!(usage.contains(KeyUsage::KEY_CERT_SIGN));
    assert!(usage.contains(KeyUsage::CRL_SIGN));
    assert!(!usage.contains(KeyUsage::DIGITAL_SIGNATURE));
    assert!(!root.has_unhandled_critical_extension());
    let PrivateKey::Rsa(rsa) = PrivateKey::from_pem(RSA_PKCS1_PEM).unwrap() else {
        panic!("the RFC 9500 RSA key read as another type");
    };
    assert_eq!(
        root.public_key(),
        &SubjectPublicKey::Rsa(rsa.public_key().clone())
    );
    assert_eq!(root.verify_signed_by(root.public_key()), Ok(()));
    assert_eq!(
        certificates_from_pem(&root.to_pem()).unwrap(),
        vec![root.clone()]
    );
}

#[test]
fn the_intermediate_is_signed_with_rsa_pss() {
    let intermediate = intermediate();
    assert_eq!(intermediate.serial_number(), &[0x10, 0x02]);
    assert_eq!(
        intermediate.signature_algorithm(),
        SignatureAlgorithm::RsaPss {
            hash: SignatureHash::Sha256,
            salt_length: 32
        }
    );
    assert_eq!(intermediate.issuer(), root().subject());
    assert_eq!(
        intermediate.subject().common_names(),
        ["EggDB Test Intermediate CA"]
    );
    assert_eq!(intermediate.not_after_unix_seconds(), JANUARY_2036);
    assert_eq!(
        intermediate.basic_constraints(),
        Some(BasicConstraints {
            certificate_authority: true,
            path_length: Some(0)
        })
    );
    assert_eq!(intermediate.verify_signed_by(root().public_key()), Ok(()));
    let intermediate_key = PrivateKey::from_pem(INTERMEDIATE_KEY_PEM).unwrap();
    assert_eq!(&intermediate_key.public_key(), intermediate.public_key());
}

#[test]
fn the_server_certificate_reads_with_its_names_and_purposes() {
    let leaf = leaf();
    assert_eq!(
        leaf.serial_number(),
        &[0x00, 0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18]
    );
    assert_eq!(
        leaf.signature_algorithm(),
        SignatureAlgorithm::Ecdsa(SignatureHash::Sha256)
    );
    assert_eq!(
        leaf.public_key(),
        &SubjectPublicKey::P256(rfc9500_p256_public_key())
    );
    assert_eq!(leaf.not_before_unix_seconds(), JANUARY_2026);
    assert_eq!(leaf.not_after_unix_seconds(), JANUARY_2027);
    assert_eq!(
        leaf.subject_alternative_names(),
        [
            SubjectAlternativeName::DnsName("db.example.com".to_string()),
            SubjectAlternativeName::DnsName("*.cluster.example.com".to_string()),
            SubjectAlternativeName::IpAddress("192.0.2.10".parse::<IpAddr>().unwrap()),
            SubjectAlternativeName::IpAddress("2001:db8::1".parse::<IpAddr>().unwrap()),
        ]
    );
    assert_eq!(
        leaf.extended_key_usage(),
        Some(&[oid::SERVER_AUTH.to_vec()][..])
    );
    assert_eq!(leaf.key_usage(), Some(KeyUsage::DIGITAL_SIGNATURE));
    assert_eq!(
        leaf.basic_constraints(),
        Some(BasicConstraints {
            certificate_authority: false,
            path_length: None
        })
    );
    assert_eq!(leaf.verify_signed_by(intermediate().public_key()), Ok(()));
    assert!(leaf.verify_signed_by(root().public_key()).is_err());
}

#[test]
fn the_chain_verifies_to_the_root() {
    let chain = [leaf(), intermediate()];
    let roots = store_of(&[root()]);
    assert_eq!(
        verify_certificate_chain(&chain, &roots, KeyPurpose::ServerAuthentication, MID_2026),
        Ok(())
    );
    let reversed_extra = [leaf(), root(), intermediate()];
    assert_eq!(
        verify_certificate_chain(
            &reversed_extra,
            &roots,
            KeyPurpose::ServerAuthentication,
            MID_2026
        ),
        Ok(())
    );
    let intermediate_as_anchor = store_of(&[intermediate()]);
    assert_eq!(
        verify_certificate_chain(
            &[leaf()],
            &intermediate_as_anchor,
            KeyPurpose::ServerAuthentication,
            MID_2026
        ),
        Ok(())
    );
    let leaf_as_anchor = store_of(&[leaf()]);
    assert_eq!(
        verify_certificate_chain(
            &[leaf()],
            &leaf_as_anchor,
            KeyPurpose::ServerAuthentication,
            MID_2026
        ),
        Ok(())
    );
    let from_pem = TrustStore::from_pem(&format!("{ROOT_PEM}{INTERMEDIATE_PEM}")).unwrap();
    assert_eq!(from_pem.certificates().len(), 2);
}

#[test]
fn broken_chains_are_refused_with_the_reason() {
    let roots = store_of(&[root()]);
    let chain = [leaf(), intermediate()];
    let server = KeyPurpose::ServerAuthentication;
    assert_eq!(
        verify_certificate_chain(&[], &roots, server, MID_2026),
        Err(CertificateVerificationError::EmptyChain)
    );
    assert_eq!(
        verify_certificate_chain(&[leaf()], &roots, server, MID_2026),
        Err(CertificateVerificationError::UnknownIssuer)
    );
    assert_eq!(
        verify_certificate_chain(&chain, &TrustStore::new(), server, MID_2026),
        Err(CertificateVerificationError::UnknownIssuer)
    );
    assert_eq!(
        verify_certificate_chain(&chain, &roots, server, JANUARY_2027 + 1),
        Err(CertificateVerificationError::Expired)
    );
    assert_eq!(
        verify_certificate_chain(&chain, &roots, server, JANUARY_2026 - 1),
        Err(CertificateVerificationError::NotYetValid)
    );
    assert_eq!(
        verify_certificate_chain(&chain, &roots, KeyPurpose::ClientAuthentication, MID_2026),
        Err(CertificateVerificationError::ExtendedKeyUsage)
    );
    let mut too_long = vec![leaf()];
    too_long.extend(std::iter::repeat_n(intermediate(), 8));
    assert_eq!(
        verify_certificate_chain(&too_long, &roots, server, MID_2026),
        Err(CertificateVerificationError::ChainTooLong)
    );
}

#[test]
fn a_tampered_certificate_does_not_verify() {
    let mut der = leaf().der().to_vec();
    let last = der.len() - 1;
    der[last] ^= 0x01;
    let tampered = Certificate::from_der(&der).unwrap();
    let roots = store_of(&[root()]);
    assert_eq!(
        verify_certificate_chain(
            &[tampered, intermediate()],
            &roots,
            KeyPurpose::ServerAuthentication,
            MID_2026
        ),
        Err(CertificateVerificationError::UnknownIssuer)
    );
}

#[test]
fn server_names_match_the_subject_alternative_names() {
    let leaf = leaf();
    for host in [
        "db.example.com",
        "DB.Example.COM.",
        "node1.cluster.example.com",
        "192.0.2.10",
        "2001:db8::1",
        "[2001:db8:0::1]",
    ] {
        let name = ServerName::parse(host).unwrap();
        assert_eq!(verify_server_name(&leaf, &name), Ok(()), "{host}");
    }
    for host in [
        "example.com",
        "cluster.example.com",
        "a.node1.cluster.example.com",
        "other.example.com",
        "192.0.2.11",
        "2001:db8::2",
    ] {
        let name = ServerName::parse(host).unwrap();
        assert_eq!(
            verify_server_name(&leaf, &name),
            Err(CertificateVerificationError::NameMismatch),
            "{host}"
        );
    }
}

#[test]
fn a_certificate_without_alternative_names_matches_its_common_name() {
    let certificate = one_certificate(CN_ONLY_PEM);
    assert!(certificate.subject_alternative_names().is_empty());
    assert_eq!(
        certificate.signature_algorithm(),
        SignatureAlgorithm::Ecdsa(SignatureHash::Sha384)
    );
    assert_eq!(certificate.not_after_unix_seconds(), JANUARY_2051);
    assert_eq!(
        certificate.verify_signed_by(certificate.public_key()),
        Ok(())
    );
    let pg = ServerName::parse("PG.example.com").unwrap();
    assert_eq!(verify_server_name(&certificate, &pg), Ok(()));
    let db = ServerName::parse("db.example.com").unwrap();
    assert_eq!(
        verify_server_name(&certificate, &db),
        Err(CertificateVerificationError::NameMismatch)
    );
    let itself = store_of(std::slice::from_ref(&certificate));
    assert_eq!(
        verify_certificate_chain(
            std::slice::from_ref(&certificate),
            &itself,
            KeyPurpose::ServerAuthentication,
            MID_2026
        ),
        Ok(())
    );
}

fn request() -> SelfSignedCertificateRequest {
    SelfSignedCertificateRequest {
        common_name: "eggdb.local".to_string(),
        subject_alternative_names: vec![
            SubjectAlternativeName::DnsName("eggdb.local".to_string()),
            SubjectAlternativeName::DnsName("localhost".to_string()),
            SubjectAlternativeName::IpAddress("127.0.0.1".parse().unwrap()),
            SubjectAlternativeName::IpAddress("::1".parse().unwrap()),
        ],
        not_before_unix_seconds: JANUARY_2026,
        not_after_unix_seconds: JANUARY_2051,
    }
}

#[test]
fn a_self_signed_certificate_reads_back_and_trusts_itself() {
    let key = rfc9500_p256_private_key();
    let mut serial = [0u8; 16];
    serial[0] = 0x40;
    serial[15] = 0x2a;
    let certificate = self_signed_certificate(&request(), &key, &serial).unwrap();
    assert_eq!(
        certificate,
        self_signed_certificate(&request(), &key, &serial).unwrap()
    );
    assert_eq!(certificate.serial_number(), &serial);
    assert_eq!(certificate.subject(), certificate.issuer());
    assert_eq!(certificate.subject().common_names(), ["eggdb.local"]);
    assert_eq!(
        certificate.subject_alternative_names(),
        request().subject_alternative_names.as_slice()
    );
    assert_eq!(certificate.not_before_unix_seconds(), JANUARY_2026);
    assert_eq!(certificate.not_after_unix_seconds(), JANUARY_2051);
    assert_eq!(
        certificate.basic_constraints(),
        Some(BasicConstraints {
            certificate_authority: false,
            path_length: None
        })
    );
    assert_eq!(
        certificate.extended_key_usage(),
        Some(&[oid::SERVER_AUTH.to_vec(), oid::CLIENT_AUTH.to_vec()][..])
    );
    assert_eq!(certificate.key_usage(), None);
    assert_eq!(
        certificate.public_key(),
        &SubjectPublicKey::P256(key.public_key())
    );
    assert_eq!(
        certificate.verify_signed_by(certificate.public_key()),
        Ok(())
    );
    let store = TrustStore::from_pem(&certificate.to_pem()).unwrap();
    for purpose in [
        KeyPurpose::ServerAuthentication,
        KeyPurpose::ClientAuthentication,
    ] {
        assert_eq!(
            verify_certificate_chain(
                std::slice::from_ref(&certificate),
                &store,
                purpose,
                MID_2026
            ),
            Ok(())
        );
    }
    for host in ["localhost", "eggdb.local", "127.0.0.1", "::1"] {
        let name = ServerName::parse(host).unwrap();
        assert_eq!(verify_server_name(&certificate, &name), Ok(()), "{host}");
    }
}

#[test]
fn a_generated_certificate_comes_with_its_key() {
    let mut random = counter_random();
    let (certificate, private_key) =
        generate_self_signed_certificate(&request(), &mut random).unwrap();
    assert_eq!(certificate.public_key(), &private_key.public_key());
    assert_eq!(certificate.serial_number().len(), 16);
    assert_eq!(certificate.serial_number()[0] & 0xc0, 0x40);
    let pem = private_key.to_pkcs8_pem().unwrap();
    let read_back = PrivateKey::from_pem(&pem).unwrap();
    assert_eq!(read_back.public_key(), private_key.public_key());
}

#[test]
fn self_signed_requests_are_checked() {
    let key = rfc9500_p256_private_key();
    let serial = [0x40; 16];
    let mut no_name = request();
    no_name.common_name.clear();
    assert_eq!(
        self_signed_certificate(&no_name, &key, &serial),
        Err(SelfSignedCertificateError::EmptyCommonName)
    );
    let mut backwards = request();
    backwards.not_after_unix_seconds = backwards.not_before_unix_seconds - 1;
    assert_eq!(
        self_signed_certificate(&backwards, &key, &serial),
        Err(SelfSignedCertificateError::Validity)
    );
    let mut unicode = request();
    unicode
        .subject_alternative_names
        .push(SubjectAlternativeName::DnsName(
            "caf\u{e9}.test".to_string(),
        ));
    assert_eq!(
        self_signed_certificate(&unicode, &key, &serial),
        Err(SelfSignedCertificateError::DnsName(
            "caf\u{e9}.test".to_string()
        ))
    );
}

#[test]
fn private_keys_sign_and_the_signatures_verify() {
    let message = b"the TLS 1.3 CertificateVerify content";
    let mut random = counter_random();
    let p256 = PrivateKey::from_pem(P256_PKCS8_PEM).unwrap();
    let rsa = PrivateKey::from_pem(RSA_PKCS1_PEM).unwrap();
    let cases = [
        (&p256, SignatureAlgorithm::Ecdsa(SignatureHash::Sha256)),
        (&p256, SignatureAlgorithm::Ecdsa(SignatureHash::Sha384)),
        (&rsa, SignatureAlgorithm::RsaPkcs1(SignatureHash::Sha256)),
        (
            &rsa,
            SignatureAlgorithm::RsaPss {
                hash: SignatureHash::Sha256,
                salt_length: 32,
            },
        ),
    ];
    for (key, algorithm) in cases {
        let signature = key.sign(algorithm, message, &mut random).unwrap();
        assert_eq!(
            verify_signature(algorithm, &key.public_key(), message, &signature),
            Ok(()),
            "{algorithm:?}"
        );
        assert!(
            verify_signature(algorithm, &key.public_key(), b"another message", &signature).is_err(),
            "{algorithm:?}"
        );
    }
}

#[test]
fn malformed_files_are_reported() {
    assert_eq!(
        certificates_from_pem(P256_SEC1_PEM),
        Err(CertificateError::NoCertificates)
    );
    assert_eq!(
        PrivateKey::from_pem(LEAF_PEM).err(),
        Some(PrivateKeyError::NoPrivateKey)
    );
    assert!(Certificate::from_der(&[]).is_err());
    assert!(PrivateKey::from_pkcs8_der(&[0x30, 0x00]).is_err());
}

fn mutations(bytes: &[u8], count: usize, mut check: impl FnMut(&[u8])) {
    for length in 0..bytes.len() {
        check(&bytes[..length]);
    }
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..count {
        let mut mutated = bytes.to_vec();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let position = (state as usize) % mutated.len();
        mutated[position] = (state >> 32) as u8;
        check(&mutated);
    }
}

#[test]
fn hostile_certificates_are_errors_not_panics() {
    let leaf_der = leaf().der().to_vec();
    mutations(&leaf_der, 10_000, |bytes| {
        if bytes.len() < leaf_der.len() {
            assert!(Certificate::from_der(bytes).is_err());
        } else {
            let _ = Certificate::from_der(bytes);
        }
    });
    let root_der = root().der().to_vec();
    mutations(&root_der, 3_000, |bytes| {
        let _ = Certificate::from_der(bytes);
    });
}

#[test]
fn hostile_private_keys_are_errors_not_panics() {
    for pem in [P256_PKCS8_PEM, P256_SEC1_PEM] {
        let der = eggdb_tls::pem::pem_decode_all(pem)
            .unwrap()
            .remove(0)
            .contents;
        mutations(&der, 400, |bytes| {
            let _ = PrivateKey::from_pkcs8_der(bytes);
            let _ = PrivateKey::from_sec1_der(bytes);
        });
    }
}
