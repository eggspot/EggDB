use std::fmt;

/// Why a document operation failed, with the numeric code and name MongoDB's documentation
/// gives the same failure, so drivers report it the way they would there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentError {
    /// The code, such as `2` for a bad value or `11000` for a duplicate key.
    pub code: i32,
    /// The code's name, such as `BadValue`.
    pub code_name: &'static str,
    /// What went wrong.
    pub message: String,
}

impl DocumentError {
    fn new(code: i32, code_name: &'static str, message: impl Into<String>) -> Self {
        Self { code, code_name, message: message.into() }
    }

    /// A value the operation cannot use (`BadValue`, 2).
    pub fn bad_value(message: impl Into<String>) -> Self {
        Self::new(2, "BadValue", message)
    }

    /// A request that does not parse (`FailedToParse`, 9).
    pub fn failed_to_parse(message: impl Into<String>) -> Self {
        Self::new(9, "FailedToParse", message)
    }

    /// A value of the wrong type (`TypeMismatch`, 14).
    pub fn type_mismatch(message: impl Into<String>) -> Self {
        Self::new(14, "TypeMismatch", message)
    }

    /// A collection or database that is not there (`NamespaceNotFound`, 26).
    pub fn namespace_not_found(message: impl Into<String>) -> Self {
        Self::new(26, "NamespaceNotFound", message)
    }

    /// Two update operators on the same path (`ConflictingUpdateOperators`, 40).
    pub fn conflicting_update(message: impl Into<String>) -> Self {
        Self::new(40, "ConflictingUpdateOperators", message)
    }

    /// A collection that already exists (`NamespaceExists`, 48).
    pub fn namespace_exists(message: impl Into<String>) -> Self {
        Self::new(48, "NamespaceExists", message)
    }

    /// A change to a field that may not change, such as `_id` (`ImmutableField`, 66).
    pub fn immutable_field(message: impl Into<String>) -> Self {
        Self::new(66, "ImmutableField", message)
    }

    /// A database or collection name that is not allowed (`InvalidNamespace`, 73).
    pub fn invalid_namespace(message: impl Into<String>) -> Self {
        Self::new(73, "InvalidNamespace", message)
    }

    /// A feature not implemented yet (`NotImplemented`, 238).
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(238, "NotImplemented", message)
    }

    /// A second document with the same unique key (`DuplicateKey`, 11000).
    pub fn duplicate_key(message: impl Into<String>) -> Self {
        Self::new(11000, "DuplicateKey", message)
    }

    /// A failure of the storage under the documents (`InternalError`, 1).
    pub fn storage(message: impl Into<String>) -> Self {
        Self::new(1, "InternalError", message)
    }
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.code_name, self.code, self.message)
    }
}

impl std::error::Error for DocumentError {}

/// The result of a document operation.
pub type DocumentResult<T> = Result<T, DocumentError>;
