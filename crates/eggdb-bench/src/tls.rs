use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::measure;
use eggdb_tls::cipher_suite::AeadKind;
use eggdb_tls::key_schedule::TrafficKeys;
use eggdb_tls::record::{MAX_PLAINTEXT_BYTES, Tls13RecordProtection, content_type};
use eggdb_tls::self_signed::{SelfSignedCertificateRequest, generate_self_signed_certificate};
use eggdb_tls::x509::SubjectAlternativeName;
use eggdb_tls::{ClientConfig, ServerConfig, ServerVerification, TlsStream};
use std::hint::black_box;
use std::net::{TcpListener, TcpStream};

pub(crate) const RECORD_SEAL: &str = "tls.record_seal";
pub(crate) const HANDSHAKE: &str = "tls.handshake";

fn sealer(kind: AeadKind) -> Result<Tls13RecordProtection, String> {
    let keys = TrafficKeys { key: vec![7u8; kind.key_bytes()], iv: [9u8; 12] };
    Tls13RecordProtection::new(kind, &keys).ok_or_else(|| "a record key of the wrong length".to_owned())
}

fn seal_records(
    protection: &mut Tls13RecordProtection,
    fragment: &[u8],
    iterations: u64,
) -> Result<(), String> {
    for _ in 0..iterations {
        let record = protection
            .seal(content_type::APPLICATION_DATA, fragment)
            .map_err(|error| error.to_string())?;
        black_box(record);
    }
    Ok(())
}

pub(crate) fn record_seal(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let fragment = vec![0x42u8; MAX_PLAINTEXT_BYTES];
    let mut aes = sealer(AeadKind::Aes128Gcm)?;
    let mut chacha = sealer(AeadKind::ChaCha20Poly1305)?;
    let measured = measure::paired(
        &mut |iterations| seal_records(&mut aes, &fragment, iterations),
        &mut |iterations| seal_records(&mut chacha, &fragment, iterations),
    )?;
    Ok(vec![
        Metric::ratio(RECORD_SEAL, "aes_128_gcm_to_chacha20_poly1305", measured.ratio),
        Metric::reported(RECORD_SEAL, "aes_128_gcm_ns_per_16k_record", measured.subject_ns),
        Metric::reported(RECORD_SEAL, "chacha20_poly1305_ns_per_16k_record", measured.reference_ns),
    ])
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn handshake(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let request = SelfSignedCertificateRequest {
        common_name: "localhost".to_owned(),
        subject_alternative_names: vec![SubjectAlternativeName::DnsName("localhost".to_owned())],
        not_before_unix_seconds: now() - 86_400,
        not_after_unix_seconds: now() + 86_400,
    };
    let (certificate, key) = generate_self_signed_certificate(&request, &mut eggdb_platform::secure_random::fill)
        .map_err(|error| format!("a self-signed certificate: {error:?}"))?;
    let server_config = ServerConfig::new(vec![certificate], key).map_err(|error| error.to_string())?;
    let client_config = ClientConfig::new(ServerVerification::EncryptionOnly, "localhost");
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let ns = std::thread::scope(|scope| -> Result<f64, String> {
        let server = scope.spawn(|| {
            for connection in listener.incoming() {
                let Ok(socket) = connection else { break };
                if TlsStream::accept(socket, &server_config).is_err() {
                    break;
                }
            }
        });
        let measured = measure::single(&mut |iterations| {
            for _ in 0..iterations {
                let socket = TcpStream::connect(address).map_err(|error| error.to_string())?;
                let stream = TlsStream::connect(socket, &client_config).map_err(|error| error.to_string())?;
                black_box(stream);
            }
            Ok(())
        });
        drop(TcpStream::connect(address));
        let _ = server.join();
        measured
    })?;
    Ok(vec![Metric::reported(HANDSHAKE, "ns_per_tls13_handshake", ns)])
}
