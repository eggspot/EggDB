use crate::error::{DocumentError, DocumentResult};
use crate::store::MAX_DOCUMENT_BYTES;
use eggdb_bson::{Bson, Document};

fn decimal_digits(mut number: usize) -> usize {
    let mut digits = 1;
    while number >= 10 {
        number /= 10;
        digits += 1;
    }
    digits
}

fn string_size(text: &str) -> usize {
    4usize.saturating_add(text.len()).saturating_add(1)
}

fn entry_size(name_length: usize, value: &Bson) -> usize {
    name_length
        .saturating_add(2)
        .saturating_add(value_size(value))
}

/// The length of `document` in BSON, counted without encoding it.
pub(crate) fn document_size(document: &Document) -> usize {
    document.iter().fold(5usize, |total, (name, value)| {
        total.saturating_add(entry_size(name.len(), value))
    })
}

/// The length of `value`'s BSON bytes, without its type byte and field name.
pub(crate) fn value_size(value: &Bson) -> usize {
    match value {
        Bson::Double(_) | Bson::DateTime(_) | Bson::Timestamp { .. } | Bson::Int64(_) => 8,
        Bson::String(text) | Bson::JavaScript(text) | Bson::Symbol(text) => string_size(text),
        Bson::Document(document) => document_size(document),
        Bson::Array(items) => items.iter().enumerate().fold(5usize, |total, (at, item)| {
            total.saturating_add(entry_size(decimal_digits(at), item))
        }),
        Bson::Binary { bytes, .. } => 5usize.saturating_add(bytes.len()),
        Bson::Undefined | Bson::Null | Bson::MinKey | Bson::MaxKey => 0,
        Bson::ObjectId(_) => 12,
        Bson::Boolean(_) => 1,
        Bson::Regex { pattern, options } => pattern
            .len()
            .saturating_add(options.len())
            .saturating_add(2),
        Bson::DbPointer { namespace, .. } => string_size(namespace).saturating_add(12),
        Bson::JavaScriptWithScope { code, scope } => 4usize
            .saturating_add(string_size(code))
            .saturating_add(document_size(scope)),
        Bson::Int32(_) => 4,
        Bson::Decimal128(_) => 16,
    }
}

/// A running count of the bytes a result will take, refused with the too-large error as
/// soon as it passes [`MAX_DOCUMENT_BYTES`], so a result that cannot be stored is never
/// built in full.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ByteBudget {
    used: usize,
}

impl ByteBudget {
    pub(crate) fn starting_at(used: usize) -> DocumentResult<Self> {
        let mut budget = Self::default();
        budget.add(used)?;
        Ok(budget)
    }

    pub(crate) fn add(&mut self, bytes: usize) -> DocumentResult<()> {
        self.used = self.used.saturating_add(bytes);
        if self.used > MAX_DOCUMENT_BYTES {
            return Err(DocumentError::document_too_large(self.used));
        }
        Ok(())
    }

    pub(crate) fn replace(&mut self, removed: usize, added: usize) -> DocumentResult<()> {
        self.used = self.used.saturating_sub(removed);
        self.add(added)
    }
}
