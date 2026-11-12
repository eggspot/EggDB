//! The rule file that picks how each user authenticates, read top to bottom; the first line
//! whose user matches wins, and users no line matches use SCRAM-SHA-256 with their stored
//! password. Each line is `<user|all> <method> [name=value ...]`; `#` starts a remark.
//!
//! - `scram-sha-256`: the password stored in the database (the default).
//! - `ldap server=[ldap://|ldaps://]host:port prefix=uid= suffix=,ou=people,dc=example,dc=org
//!   [timeout=5] [starttls=1] [ca=ca.pem]`: a simple bind against a directory, over TLS with
//!   `ldaps://` or `starttls=1` (both need `ca=`); with `basedn=dc=example,dc=org
//!   [search_attribute=uid] [bind_dn=...] [bind_password=...]` it searches for the user's entry
//!   first and binds as it.
//! - `jwt jwks=keys.json issuer=https://login.example audience=eggdb [user_claim=sub]
//!   [leeway=60]`: an OpenID Connect token sent as the password, its signature checked with the
//!   key set file (relative to the rule file).
//! - `oauth jwks=keys.json issuer=... audience=... discovery=https://login.example/.well-known/openid-configuration
//!   [scope=openid] [user_claim=sub] [leeway=60]`: the same token sent through SASL's `OAUTHBEARER`, as
//!   PostgreSQL 18's clients send it; a client without a token is pointed at `discovery` to get one.
//! - `cert`: a TLS client certificate whose common name is the user name; the server must ask
//!   clients for certificates (`EGGDB_TLS_CLIENT_CA`).
//! - `apikey`: an API key of the role (`CREATE EXTENSION eggdb_api_keys`) sent as the password.
//! - `reject`: the user may not log in.
//!
//! Any line may also name `database=<name>` and `address=<ip>[/<prefix>]`; it then applies only
//! to logins to that database, or from that address range (never on the local socket).

use crate::jwt::{JwtKeys, JwtPolicy};
use crate::ldap::{LdapConfig, LdapSearch, LdapSecurity};
use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

const MAX_RULES: usize = 1024;

/// How one user authenticates.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    /// SCRAM-SHA-256 against the stored password.
    ScramSha256,
    /// A password checked by an LDAP simple bind.
    Ldap(LdapConfig),
    /// An OpenID Connect token checked against a key set.
    Jwt {
        /// The keys tokens may be signed with.
        keys: JwtKeys,
        /// What tokens must say.
        policy: JwtPolicy,
    },
    /// An OpenID Connect token sent through SASL's `OAUTHBEARER` mechanism (RFC 7628).
    OAuthBearer {
        /// The keys tokens may be signed with.
        keys: JwtKeys,
        /// What tokens must say.
        policy: JwtPolicy,
        /// The issuer's OpenID discovery document, which a client without a token is sent to.
        discovery: String,
        /// The scope a client asks the issuer for.
        scope: String,
    },
    /// A TLS client certificate, checked by the TLS layer, whose common name is the user.
    Certificate,
    /// An API key of the role, sent as the password and checked against the hashes
    /// `CREATE EXTENSION eggdb_api_keys` keeps.
    ApiKey,
    /// No login.
    Reject,
}

impl AuthMethod {
    /// The method's name as a rule file writes it: `scram-sha-256`, `ldap`, `jwt`, `oauth`,
    /// `cert`, `apikey` or `reject`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ScramSha256 => "scram-sha-256",
            Self::Ldap(_) => "ldap",
            Self::Jwt { .. } => "jwt",
            Self::OAuthBearer { .. } => "oauth",
            Self::Certificate => "cert",
            Self::ApiKey => "apikey",
            Self::Reject => "reject",
        }
    }

    /// Checks a password, token or key that `user` sent in clear text against this method:
    /// `ldap` binds to the directory, `jwt` verifies the token (which must name `user`), and
    /// `apikey` asks `api_key_ok`, which looks the key up in the database. Other methods take no
    /// such secret and fail.
    pub fn check_secret(&self, user: &str, secret: &str, api_key_ok: &dyn Fn(&str) -> bool) -> Result<(), String> {
        match self {
            Self::Ldap(config) => match config.authenticate(user, secret) {
                Ok(true) => Ok(()),
                Ok(false) => Err("the LDAP server refused the password".to_owned()),
                Err(error) => Err(error.to_string()),
            },
            Self::Jwt { keys, policy } => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
                match crate::jwt::verify(secret, keys, policy, now) {
                    Ok(named) if named == user => Ok(()),
                    Ok(_) => Err("the token names another user".to_owned()),
                    Err(error) => Err(error.to_string()),
                }
            }
            Self::ApiKey if api_key_ok(secret) => Ok(()),
            Self::ApiKey => Err("the API key is not a live key of this role".to_owned()),
            _ => Err(format!("{} takes no password", self.name())),
        }
    }

    /// Whether the method has the client send its password or token in clear text, so it is
    /// allowed only over TLS or a local socket.
    pub fn sends_secret_in_clear(&self) -> bool {
        matches!(self, Self::Ldap(_) | Self::Jwt { .. } | Self::OAuthBearer { .. } | Self::ApiKey)
    }
}

#[derive(Debug, Clone)]
struct Rule {
    user: Option<String>,
    database: Option<String>,
    address: Option<AddressRange>,
    method: AuthMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AddressRange {
    network: IpAddr,
    prefix: u8,
}

impl AddressRange {
    fn parse(text: &str) -> Option<Self> {
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix.parse::<u8>().ok()?)),
            None => (text, None),
        };
        let network: IpAddr = address.parse().ok()?;
        let most = if network.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(most);
        (prefix <= most).then_some(Self { network, prefix })
    }

    fn contains(self, address: IpAddr) -> bool {
        let address = match (self.network, address) {
            (IpAddr::V4(_), IpAddr::V6(v6)) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
            _ => address,
        };
        let (network, candidate, bits) = match (self.network, address) {
            (IpAddr::V4(network), IpAddr::V4(candidate)) => {
                (u128::from(u32::from(network)), u128::from(u32::from(candidate)), 32u32)
            }
            (IpAddr::V6(network), IpAddr::V6(candidate)) => (u128::from(network), u128::from(candidate), 128u32),
            _ => return false,
        };
        let host_bits = bits - u32::from(self.prefix);
        if host_bits >= 128 {
            return true;
        }
        (network >> host_bits) == (candidate >> host_bits)
    }
}

impl Rule {
    fn applies(&self, user: &str, database: Option<&str>, address: Option<IpAddr>) -> bool {
        self.user.as_deref().is_none_or(|named| named == user)
            && self.database.as_deref().is_none_or(|named| database == Some(named))
            && self.address.is_none_or(|range| address.is_some_and(|address| range.contains(address)))
    }
}

/// The rules of a rule file, in order.
#[derive(Debug, Clone, Default)]
pub struct AuthRules {
    rules: Vec<Rule>,
}

/// A line of the rule file that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesError {
    /// The line's number, from 1.
    pub line: usize,
    /// What is wrong with it.
    pub message: String,
}

impl fmt::Display for RulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for RulesError {}

fn settings<'a>(
    words: impl Iterator<Item = &'a str>,
    line: usize,
) -> Result<BTreeMap<&'a str, &'a str>, RulesError> {
    let mut found = BTreeMap::new();
    for word in words {
        let Some((name, value)) = word.split_once('=') else {
            return Err(RulesError {
                line,
                message: format!("{word} is not name=value"),
            });
        };
        found.insert(name, value);
    }
    Ok(found)
}

fn required<'a>(
    found: &BTreeMap<&str, &'a str>,
    name: &str,
    line: usize,
) -> Result<&'a str, RulesError> {
    found.get(name).copied().ok_or_else(|| RulesError {
        line,
        message: format!("the method needs {name}="),
    })
}

fn seconds(
    found: &BTreeMap<&str, &str>,
    name: &str,
    default: u64,
    line: usize,
) -> Result<u64, RulesError> {
    found.get(name).map_or(Ok(default), |value| {
        value.parse().map_err(|_| RulesError {
            line,
            message: format!("{name}= takes a number of seconds"),
        })
    })
}

impl AuthRules {
    /// Reads rules from `text`; `read_key_set` reads a `jwks=` file named on a line.
    pub fn parse(
        text: &str,
        read_key_set: &dyn Fn(&Path) -> std::io::Result<String>,
    ) -> Result<Self, RulesError> {
        let mut rules = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let content = raw.split('#').next().unwrap_or_default().trim();
            if content.is_empty() {
                continue;
            }
            if rules.len() >= MAX_RULES {
                return Err(RulesError {
                    line,
                    message: format!("more than {MAX_RULES} rules"),
                });
            }
            let mut words = content.split_whitespace();
            let user = words.next().unwrap_or_default();
            let Some(method) = words.next() else {
                return Err(RulesError {
                    line,
                    message: "a rule needs a user and a method".to_owned(),
                });
            };
            let found = settings(words, line)?;
            let method = match method {
                "scram-sha-256" => AuthMethod::ScramSha256,
                "reject" => AuthMethod::Reject,
                "apikey" => AuthMethod::ApiKey,
                "ldap" => {
                    let written = required(&found, "server", line)?;
                    let (server, mut security) = if let Some(rest) = written.strip_prefix("ldaps://") {
                        (rest.to_owned(), LdapSecurity::Ldaps)
                    } else {
                        (
                            written.strip_prefix("ldap://").unwrap_or(written).to_owned(),
                            LdapSecurity::Plain,
                        )
                    };
                    if found.get("starttls").copied() == Some("1") {
                        if security == LdapSecurity::Ldaps {
                            return Err(RulesError {
                                line,
                                message: "starttls=1 does not go with an ldaps:// server".to_owned(),
                            });
                        }
                        security = LdapSecurity::StartTls;
                    }
                    let trusted_pem = match found.get("ca") {
                        Some(path) => Some(read_key_set(Path::new(path)).map_err(|error| {
                            RulesError {
                                line,
                                message: format!("cannot read {path}: {error}"),
                            }
                        })?),
                        None if security != LdapSecurity::Plain => {
                            return Err(RulesError {
                                line,
                                message: "TLS to the directory needs ca=<certificate authority file>".to_owned(),
                            });
                        }
                        None => None,
                    };
                    let search = found.get("basedn").map(|base_dn| LdapSearch {
                        base_dn: (*base_dn).to_owned(),
                        attribute: found
                            .get("search_attribute")
                            .copied()
                            .unwrap_or("uid")
                            .to_owned(),
                        bind_dn: found.get("bind_dn").map(|dn| (*dn).to_owned()),
                        bind_password: found.get("bind_password").map(|secret| (*secret).to_owned()),
                    });
                    AuthMethod::Ldap(LdapConfig {
                        server,
                        prefix: found.get("prefix").copied().unwrap_or_default().to_owned(),
                        suffix: found.get("suffix").copied().unwrap_or_default().to_owned(),
                        timeout: Duration::from_secs(seconds(&found, "timeout", 5, line)?),
                        security,
                        trusted_pem,
                        search,
                    })
                }
                "cert" => AuthMethod::Certificate,
                "jwt" | "oauth" => {
                    let path = required(&found, "jwks", line)?;
                    let document = read_key_set(Path::new(path)).map_err(|error| RulesError {
                        line,
                        message: format!("cannot read {path}: {error}"),
                    })?;
                    let keys = JwtKeys::from_jwks(&document).map_err(|error| RulesError {
                        line,
                        message: error.to_string(),
                    })?;
                    let policy = JwtPolicy {
                        issuer: required(&found, "issuer", line)?.to_owned(),
                        audience: required(&found, "audience", line)?.to_owned(),
                        user_claim: found
                            .get("user_claim")
                            .copied()
                            .unwrap_or("sub")
                            .to_owned(),
                        leeway_seconds: seconds(&found, "leeway", 60, line)?.min(3600) as i64,
                    };
                    if method == "oauth" {
                        AuthMethod::OAuthBearer {
                            keys,
                            policy,
                            discovery: required(&found, "discovery", line)?.to_owned(),
                            scope: found.get("scope").copied().unwrap_or("openid").to_owned(),
                        }
                    } else {
                        AuthMethod::Jwt { keys, policy }
                    }
                }
                other => {
                    return Err(RulesError {
                        line,
                        message: format!(
                            "unknown method {other}; use scram-sha-256, ldap, jwt, oauth, cert, apikey or reject"
                        ),
                    });
                }
            };
            let database = found.get("database").map(|name| (*name).to_owned()).filter(|name| name != "all");
            let address = match found.get("address") {
                None => None,
                Some(written) => Some(AddressRange::parse(written).ok_or_else(|| RulesError {
                    line,
                    message: format!("address={written} is not an IP address or address/prefix"),
                })?),
            };
            rules.push(Rule {
                user: (user != "all").then(|| user.to_owned()),
                database,
                address,
                method,
            });
        }
        Ok(Self { rules })
    }

    /// Reads the rule file at `path`, with `jwks=` files relative to its folder.
    pub fn load(path: &Path) -> Result<Self, RulesError> {
        let text = std::fs::read_to_string(path).map_err(|error| RulesError {
            line: 0,
            message: format!("cannot read {}: {error}", path.display()),
        })?;
        let folder = path.parent().unwrap_or(Path::new("."));
        Self::parse(&text, &|key_set| {
            std::fs::read_to_string(folder.join(key_set))
        })
    }

    /// The key sets and policies of the `jwt` rules, in order, for clients that send a bearer
    /// token without naming a user first.
    pub fn bearer_methods(&self) -> impl Iterator<Item = (&JwtKeys, &JwtPolicy)> {
        self.rules.iter().filter_map(|rule| match &rule.method {
            AuthMethod::Jwt { keys, policy } | AuthMethod::OAuthBearer { keys, policy, .. } => {
                Some((keys, policy))
            }
            _ => None,
        })
    }

    /// The method for `user` when neither the database nor the client's address is known: the
    /// first rule for the user that names neither `database=` nor `address=`.
    pub fn method_for(&self, user: &str) -> &AuthMethod {
        self.method_for_connection(user, None, None)
    }

    /// The method for `user` logging in to `database` from `address` (`None` on the local
    /// socket): the first rule whose user, `database=` and `address=` (an IP address or
    /// `address/prefix`) all match; scram-sha-256 when none does.
    pub fn method_for_connection(&self, user: &str, database: Option<&str>, address: Option<IpAddr>) -> &AuthMethod {
        self.rules
            .iter()
            .find(|rule| rule.applies(user, database, address))
            .map_or(&AuthMethod::ScramSha256, |rule| &rule.method)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_files(_: &Path) -> std::io::Result<String> {
        Err(std::io::Error::other("no files in this test"))
    }

    #[test]
    fn the_first_matching_rule_wins_and_others_use_scram() {
        let rules = AuthRules::parse(
            "# directory users\nada ldap server=ldap:389 prefix=uid= suffix=,dc=x\nmallory reject\n",
            &no_files,
        )
        .unwrap();
        assert!(
            matches!(rules.method_for("ada"), AuthMethod::Ldap(config) if config.prefix == "uid=")
        );
        assert!(matches!(rules.method_for("mallory"), AuthMethod::Reject));
        assert!(matches!(rules.method_for("grace"), AuthMethod::ScramSha256));
        let everyone = AuthRules::parse("all reject\nada scram-sha-256", &no_files).unwrap();
        assert!(matches!(everyone.method_for("ada"), AuthMethod::Reject));
    }

    #[test]
    fn rules_can_name_a_database_and_an_address_range() {
        let rules = AuthRules::parse(
            "all reject address=203.0.113.0/24\nada apikey database=reports\nada ldap server=ldap:389 prefix=uid= suffix=,dc=x address=10.0.0.0/8\n",
            &no_files,
        )
        .unwrap();
        let office: IpAddr = "10.1.2.3".parse().unwrap();
        let outside: IpAddr = "203.0.113.7".parse().unwrap();
        let mapped: IpAddr = "::ffff:203.0.113.9".parse().unwrap();
        assert!(matches!(rules.method_for_connection("ada", Some("reports"), Some(office)), AuthMethod::ApiKey));
        assert!(matches!(rules.method_for_connection("ada", Some("shop"), Some(office)), AuthMethod::Ldap(_)));
        assert!(matches!(rules.method_for_connection("ada", Some("shop"), None), AuthMethod::ScramSha256));
        assert!(matches!(rules.method_for_connection("ada", Some("reports"), Some(outside)), AuthMethod::Reject));
        assert!(matches!(rules.method_for_connection("grace", None, Some(mapped)), AuthMethod::Reject));
        assert!(matches!(rules.method_for("ada"), AuthMethod::ScramSha256));
        assert!(AuthRules::parse("ada reject address=10.0.0.0/33", &no_files).is_err());
        assert!(AuthRules::parse("ada reject address=nowhere", &no_files).is_err());
    }

    #[test]
    fn bad_lines_name_their_number() {
        for (text, line) in [
            ("ada", 1),
            ("\nada kerberos", 2),
            ("ada ldap", 1),
            ("ada ldap server", 1),
            ("ada jwt jwks=k.json issuer=i audience=a", 1),
            ("ada ldap server=x timeout=soon", 1),
        ] {
            assert_eq!(
                AuthRules::parse(text, &no_files).unwrap_err().line,
                line,
                "{text}"
            );
        }
    }
}
