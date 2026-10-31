//! The rule file that picks how each user authenticates, read top to bottom; the first line
//! whose user matches wins, and users no line matches use SCRAM-SHA-256 with their stored
//! password. Each line is `<user|all> <method> [name=value ...]`; `#` starts a remark.
//!
//! - `scram-sha-256`: the password stored in the database (the default).
//! - `ldap server=host:port prefix=uid= suffix=,ou=people,dc=example,dc=org [timeout=5]`: a
//!   simple bind against a directory.
//! - `jwt jwks=keys.json issuer=https://login.example audience=eggdb [user_claim=sub]
//!   [leeway=60]`: an OpenID Connect token sent as the password, its signature checked with the
//!   key set file (relative to the rule file).
//! - `reject`: the user may not log in.

use crate::jwt::{JwtKeys, JwtPolicy};
use crate::ldap::LdapConfig;
use std::collections::BTreeMap;
use std::fmt;
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
    /// No login.
    Reject,
}

impl AuthMethod {
    /// Whether the method has the client send its password or token in clear text, so it is
    /// allowed only over TLS or a local socket.
    pub fn sends_secret_in_clear(&self) -> bool {
        matches!(self, Self::Ldap(_) | Self::Jwt { .. })
    }
}

#[derive(Debug, Clone)]
struct Rule {
    user: Option<String>,
    method: AuthMethod,
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

fn seconds(found: &BTreeMap<&str, &str>, name: &str, default: u64, line: usize) -> Result<u64, RulesError> {
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
                "ldap" => AuthMethod::Ldap(LdapConfig {
                    server: required(&found, "server", line)?.to_owned(),
                    prefix: found.get("prefix").copied().unwrap_or_default().to_owned(),
                    suffix: found.get("suffix").copied().unwrap_or_default().to_owned(),
                    timeout: Duration::from_secs(seconds(&found, "timeout", 5, line)?),
                }),
                "jwt" => {
                    let path = required(&found, "jwks", line)?;
                    let document = read_key_set(Path::new(path)).map_err(|error| RulesError {
                        line,
                        message: format!("cannot read {path}: {error}"),
                    })?;
                    let keys = JwtKeys::from_jwks(&document).map_err(|error| RulesError {
                        line,
                        message: error.to_string(),
                    })?;
                    AuthMethod::Jwt {
                        keys,
                        policy: JwtPolicy {
                            issuer: required(&found, "issuer", line)?.to_owned(),
                            audience: required(&found, "audience", line)?.to_owned(),
                            user_claim: found.get("user_claim").copied().unwrap_or("sub").to_owned(),
                            leeway_seconds: seconds(&found, "leeway", 60, line)?.min(3600) as i64,
                        },
                    }
                }
                other => {
                    return Err(RulesError {
                        line,
                        message: format!(
                            "unknown method {other}; use scram-sha-256, ldap, jwt or reject"
                        ),
                    });
                }
            };
            rules.push(Rule {
                user: (user != "all").then(|| user.to_owned()),
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
        Self::parse(&text, &|key_set| std::fs::read_to_string(folder.join(key_set)))
    }

    /// The method for `user`.
    pub fn method_for(&self, user: &str) -> &AuthMethod {
        self.rules
            .iter()
            .find(|rule| rule.user.as_deref().is_none_or(|named| named == user))
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
        assert!(matches!(rules.method_for("ada"), AuthMethod::Ldap(config) if config.prefix == "uid="));
        assert!(matches!(rules.method_for("mallory"), AuthMethod::Reject));
        assert!(matches!(rules.method_for("grace"), AuthMethod::ScramSha256));
        let everyone = AuthRules::parse("all reject\nada scram-sha-256", &no_files).unwrap();
        assert!(matches!(everyone.method_for("ada"), AuthMethod::Reject));
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
            assert_eq!(AuthRules::parse(text, &no_files).unwrap_err().line, line, "{text}");
        }
    }
}
