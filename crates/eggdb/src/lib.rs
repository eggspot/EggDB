//! EggDB: one database for every kind of data, written from scratch in Rust.

/// The version of this EggDB build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_matches_the_package() {
        assert_eq!(VERSION, "0.1.0");
    }
}
