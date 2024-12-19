use thiserror::Error;

/// Main error type
#[derive(Debug, Error)]
pub enum Error {
    #[error("Database open failed")]
    DbOpenFailed,

    #[error("Not found")]
    NotFound,

    #[error("Decode error")]
    DecodeError,

    #[error("Query failed")]
    QueryError,

    #[error("Note process failed")]
    NoteProcessFailed,

    #[error("Transaction failed")]
    TransactionFailed,

    #[error("Subscription failed")]
    SubscriptionError,

    #[error("Buffer overflow")]
    BufferOverflow,

    #[error("Filter error: {0}")]
    Filter(#[from] FilterError),

    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
}

/// Filter-specific error type
#[derive(Debug, Error, Eq, PartialEq)]
pub enum FilterError {
    #[error("Field already exists")]
    FieldAlreadyExists,

    #[error("Field already started")]
    FieldAlreadyStarted,
}

impl FilterError {
    pub fn already_exists() -> Error {
        Error::Filter(FilterError::FieldAlreadyExists)
    }

    pub fn already_started() -> Error {
        Error::Filter(FilterError::FieldAlreadyStarted)
    }
}
