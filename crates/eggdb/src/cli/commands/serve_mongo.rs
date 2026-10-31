use eggdb_auth::scram::ScramVerifier;

const MONGO_SCRAM_ITERATIONS: u32 = 15_000;

pub(super) fn config(user: &str, password: &str, max_connections: usize) -> std::io::Result<eggdb_mongowire::ServerConfig> {
    let mut salt = [0u8; 16];
    eggdb_platform::secure_random::fill(&mut salt)?;
    let mut unknown_user_secret = [0u8; 32];
    eggdb_platform::secure_random::fill(&mut unknown_user_secret)?;
    Ok(eggdb_mongowire::ServerConfig {
        user: user.to_owned(),
        verifier: ScramVerifier::derive(password.as_bytes(), &salt, MONGO_SCRAM_ITERATIONS),
        unknown_user_secret,
        max_connections,
        random: eggdb_platform::secure_random::fill,
    })
}

pub(super) fn store_seed() -> [u8; 8] {
    let mut seed = [0u8; 8];
    if eggdb_platform::secure_random::fill(&mut seed).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos() as u64);
        seed = (nanos ^ u64::from(std::process::id()).rotate_left(32)).to_le_bytes();
    }
    seed
}
