use std::fmt;

/// Error type of the crate. The variants mirror the Python exceptions used by
/// `drf-api-contracts` so behaviour stays easy to compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// Python `ValueError`: an argument has an invalid value.
    Value(String),
    /// Python `TypeError`: the API was used in the wrong way
    /// (e.g. `respond()` on a paginated contract).
    Type(String),
    /// DRF `ValidationError`: payload did not match the declared schema type.
    Validation(String),
}

impl ContractError {
    pub(crate) fn value(msg: impl Into<String>) -> Self {
        Self::Value(msg.into())
    }
    pub(crate) fn type_err(msg: impl Into<String>) -> Self {
        Self::Type(msg.into())
    }
    pub fn message(&self) -> &str {
        match self {
            Self::Value(m) | Self::Type(m) | Self::Validation(m) => m,
        }
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(m) => write!(f, "value error: {m}"),
            Self::Type(m) => write!(f, "type error: {m}"),
            Self::Validation(m) => write!(f, "validation error: {m}"),
        }
    }
}

impl std::error::Error for ContractError {}

pub type Result<T> = std::result::Result<T, ContractError>;
