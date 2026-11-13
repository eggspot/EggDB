use super::{OutputColumn, QueryResult, Session};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};
use std::sync::OnceLock;
use std::time::Duration;

const FRESHNESS_SETTING: &str = "eggdb.read_freshness";
const AFTER_SETTING: &str = "eggdb.read_after";
const STALENESS_SETTING: &str = "eggdb.max_staleness_ms";
const TIMEOUT_SETTING: &str = "eggdb.freshness_timeout_ms";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_STALENESS: Duration = Duration::from_secs(1);

/// How fresh a replica's data must be before a statement reads it (D1028).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Whatever the replica holds.
    Eventual,
    /// Caught up with everything the primary had, as last heard, no more than this long ago.
    BoundedStaleness(Duration),
    /// At least the position of a token the client took after its own write.
    ReadYourWrites(u64, u64),
    /// At least the position of a token the client carries from anything it read or wrote, in
    /// any session.
    Causal(u64, u64),
    /// Everything the primary had committed when the statement began.
    Linearizable,
}

type FreshnessHook = fn(Freshness, Duration) -> Result<(), String>;
type TokenHook = fn() -> Option<(u64, u64)>;

static FRESHNESS: OnceLock<FreshnessHook> = OnceLock::new();
static TOKEN: OnceLock<TokenHook> = OnceLock::new();

/// Lets sessions wait for a replica's freshness and read the replication token (D1028); the
/// replicating server installs the hooks once. The freshness hook waits up to the timeout and
/// says why when the replica cannot become fresh enough.
pub fn set_replication_hooks(freshness: FreshnessHook, token: TokenHook) {
    let _ = FRESHNESS.set(freshness);
    let _ = TOKEN.set(token);
}

fn token_of(text: &str) -> SqlResult<(u64, u64)> {
    let invalid = || SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("{AFTER_SETTING} takes a replication token, term:sequence, not {text}"));
    let (term, sequence) = text.trim().split_once(':').ok_or_else(invalid)?;
    Ok((term.parse().map_err(|_| invalid())?, sequence.parse().map_err(|_| invalid())?))
}

impl Session<'_> {
    fn custom_setting(&self, name: &str) -> Option<String> {
        self.settings.get(name).ok().filter(|value| !value.is_empty())
    }

    fn requested_freshness(&self) -> SqlResult<(Freshness, Duration)> {
        let millis = |name: &str, default: Duration| -> SqlResult<Duration> {
            match self.custom_setting(name) {
                None => Ok(default),
                Some(text) => text
                    .trim()
                    .parse::<u64>()
                    .map(Duration::from_millis)
                    .map_err(|_| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("{name} takes milliseconds, not {text}"))),
            }
        };
        let timeout = millis(TIMEOUT_SETTING, DEFAULT_TIMEOUT)?;
        let after = self.custom_setting(AFTER_SETTING).map(|text| token_of(&text)).transpose()?;
        let freshness = match self.custom_setting(FRESHNESS_SETTING).as_deref().map(str::to_ascii_lowercase).as_deref() {
            None | Some("eventual") => match after {
                Some((term, sequence)) => Freshness::ReadYourWrites(term, sequence),
                None => Freshness::Eventual,
            },
            Some("bounded_staleness") => Freshness::BoundedStaleness(millis(STALENESS_SETTING, DEFAULT_STALENESS)?),
            Some("read_your_writes") => {
                let (term, sequence) = after.ok_or_else(|| missing_token("read_your_writes"))?;
                Freshness::ReadYourWrites(term, sequence)
            }
            Some("causal") => {
                let (term, sequence) = after.ok_or_else(|| missing_token("causal"))?;
                Freshness::Causal(term, sequence)
            }
            Some("linearizable") => Freshness::Linearizable,
            Some(other) => {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("{FRESHNESS_SETTING} is eventual, bounded_staleness, read_your_writes, causal or linearizable, not {other}"),
                ));
            }
        };
        Ok((freshness, timeout))
    }

    /// Answers `SHOW eggdb.replication_token`, and before any other statement waits until the
    /// replica holds what the session's freshness asks for (D1028). On a primary or a server
    /// that does not replicate every read is fresh, so nothing waits.
    pub(super) fn prepare_freshness(&mut self, text: &str, on_result: &mut dyn FnMut(QueryResult) -> bool) -> SqlResult<bool> {
        let compact = text.trim().trim_end_matches(';').trim_end();
        if compact.eq_ignore_ascii_case("show eggdb.replication_token") {
            let token = TOKEN.get().and_then(|hook| hook());
            let value = token.map_or(Value::Null, |(term, sequence)| Value::Text(format!("{term}:{sequence}")));
            let column = OutputColumn { name: "replication_token".to_owned(), sql_type: Some(SqlType::Text) };
            on_result(QueryResult::rows("SHOW".to_owned(), vec![column], vec![vec![value]]));
            return Ok(true);
        }
        let Some(hook) = FRESHNESS.get() else {
            return Ok(false);
        };
        let (freshness, timeout) = self.requested_freshness()?;
        if freshness == Freshness::Eventual {
            return Ok(false);
        }
        hook(freshness, timeout).map_err(|message| SqlError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("the replica is not fresh enough: {message}")))?;
        Ok(false)
    }
}

fn missing_token(freshness: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("{FRESHNESS_SETTING} = {freshness} needs {AFTER_SETTING}, the token SHOW eggdb.replication_token gave after the write"),
    )
}

#[cfg(test)]
mod tests {
    use super::token_of;

    #[test]
    fn tokens_read_as_term_and_sequence() {
        assert_eq!(token_of("3:1042").unwrap(), (3, 1042));
        assert!(token_of("1042").is_err());
    }
}
