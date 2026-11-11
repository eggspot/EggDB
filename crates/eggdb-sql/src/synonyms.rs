use crate::ast::{Name, SynonymCommand};
use crate::encoding::NAMESPACE;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite};

pub(crate) const SYNONYM_SPACE: u8 = 0x17;
const RECORD_VERSION: u8 = 1;
const PUBLIC_MARK: u8 = 0x00;
const MAX_CHAIN: usize = 16;

fn synonym_key(name: &str, public: bool) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(SYNONYM_SPACE);
    if public {
        key.push(PUBLIC_MARK);
    }
    key.extend_from_slice(name.as_bytes());
    key
}

fn encode(target: &str) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    out.extend_from_slice(target.as_bytes());
    out
}

fn decode(bytes: &[u8]) -> SqlResult<String> {
    match bytes.split_first() {
        Some((&RECORD_VERSION, text)) => std::str::from_utf8(text)
            .map(str::to_owned)
            .map_err(|_| crate::encoding::corrupt("synonym")),
        _ => Err(crate::encoding::corrupt("synonym")),
    }
}

fn names_an_object(reader: &dyn KeyValueRead, name: &str) -> SqlResult<bool> {
    Ok(reader.get(&crate::encoding::table_key(name))?.is_some()
        || reader.get(&crate::encoding::sequence_key(name))?.is_some())
}

fn target_of(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Option<String>> {
    let base = crate::schemas::base_name(name);
    for (key, public) in [(name, false), (base, true)] {
        if let Some(bytes) = reader.get(&synonym_key(key, public))? {
            return decode(&bytes).map(Some);
        }
    }
    Ok(None)
}

fn looping(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("looping chain of synonyms: {name}"),
    )
    .with_oracle_code(1775)
}

/// The object an Oracle synonym stands for: `name` itself when it names a table, view or
/// sequence, else the target of its private synonym, then of a public one, followed through
/// chains of synonyms; `None` when `name` is no synonym.
pub(crate) fn resolved(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Option<String>> {
    if names_an_object(reader, name)? {
        return Ok(None);
    }
    let mut current = name.to_owned();
    let mut followed = false;
    for _ in 0..MAX_CHAIN {
        let Some(target) = target_of(reader, &current)? else {
            return Ok(followed.then_some(current));
        };
        followed = true;
        if names_an_object(reader, &target)? {
            return Ok(Some(target));
        }
        current = target;
    }
    Err(looping(name))
}

fn display(name: &Name) -> String {
    crate::schemas::display(&name.text).to_ascii_uppercase()
}

/// Runs Oracle's `CREATE [OR REPLACE] [PUBLIC] SYNONYM` or `DROP [PUBLIC] SYNONYM`.
pub(crate) fn run(writer: &mut dyn KeyValueWrite, command: &SynonymCommand) -> SqlResult<QueryResult> {
    match command {
        SynonymCommand::Create {
            name,
            public,
            replace,
            target,
        } => {
            let key_name = if *public {
                crate::schemas::base_name(&name.text)
            } else {
                name.text.as_str()
            };
            let key = synonym_key(key_name, *public);
            if !*public && names_an_object(&*writer, &name.text)? {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_TABLE,
                    "name is already used by an existing object",
                )
                .with_oracle_code(955)
                .at(name.position));
            }
            if !*replace && writer.get(&key)?.is_some() {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("synonym {} already exists", display(name)),
                )
                .with_oracle_code(955)
                .at(name.position));
            }
            if target.text == name.text {
                return Err(SqlError::syntax(
                    "a synonym can't stand for itself",
                    target.position,
                )
                .with_oracle_code(1471));
            }
            writer.put(&key, &encode(&target.text))?;
            Ok(QueryResult::command("CREATE SYNONYM"))
        }
        SynonymCommand::Drop { name, public, .. } => {
            let key_name = if *public {
                crate::schemas::base_name(&name.text)
            } else {
                name.text.as_str()
            };
            let key = synonym_key(key_name, *public);
            if writer.get(&key)?.is_none() {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("synonym to be dropped does not exist: {}", display(name)),
                )
                .with_oracle_code(1434)
                .at(name.position));
            }
            writer.delete(&key)?;
            Ok(QueryResult::command("DROP SYNONYM"))
        }
    }
}
