use super::{COLUMN_GRANT_SPACE, DENY_SPACE, GRANT_SPACE, ROLE_SPACE, decode_role, encode_role};
use crate::encoding::{NAMESPACE, prefix_end};
use crate::error::SqlResult;
use crate::row_security::{POLICY_SPACE, ROW_SECURITY_SPACE};
use crate::store::{EntryVisitor, KeyValueRead, ScanDirection};
use std::collections::BTreeMap;

const ACCESS_SPACES: [u8; 6] = [
    ROLE_SPACE,
    GRANT_SPACE,
    POLICY_SPACE,
    ROW_SECURITY_SPACE,
    COLUMN_GRANT_SPACE,
    DENY_SPACE,
];

/// A copy of the records that decide who may do what (roles without their password verifiers,
/// table and column grants, row security policies and switches) as one statement's catalog snapshot saw them. It reads like
/// the store, so the catalog answers privilege questions with the very functions that check
/// statements, never with a second copy of their rules.
#[derive(Debug, Default)]
pub(crate) struct AccessRecords(BTreeMap<Vec<u8>, Vec<u8>>);

impl AccessRecords {
    pub(crate) fn load(reader: &dyn KeyValueRead) -> SqlResult<Self> {
        let mut records = BTreeMap::new();
        for space in ACCESS_SPACES {
            let mut prefix = NAMESPACE.to_vec();
            prefix.push(space);
            reader.scan(
                &prefix,
                &prefix_end(&prefix),
                ScanDirection::Forward,
                &mut |key, value| {
                    let kept = if space == ROLE_SPACE {
                        without_verifier(key, value)?
                    } else {
                        value.to_vec()
                    };
                    records.insert(key.to_vec(), kept);
                    Ok(true)
                },
            )?;
        }
        Ok(Self(records))
    }
}

fn without_verifier(key: &[u8], value: &[u8]) -> SqlResult<Vec<u8>> {
    let name = key
        .get(NAMESPACE.len() + 1..)
        .and_then(|name| std::str::from_utf8(name).ok())
        .ok_or_else(|| crate::encoding::corrupt("role"))?;
    let mut role = decode_role(name, value)?;
    role.scram_verifier = None;
    Ok(encode_role(&role))
}

impl KeyValueRead for AccessRecords {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.0.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        if start >= end {
            return Ok(());
        }
        let range = self.0.range(start.to_vec()..end.to_vec());
        let entries: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match direction {
            ScanDirection::Forward => Box::new(range),
            ScanDirection::Backward => Box::new(range.rev()),
        };
        for (key, value) in entries {
            if !visit(key, value)? {
                break;
            }
        }
        Ok(())
    }
}
