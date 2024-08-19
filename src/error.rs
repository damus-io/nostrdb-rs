use std::fmt;

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    DbOpenFailed,
    NotFound,
    DecodeError,
    QueryError,
    NoteProcessFailed,
    TransactionFailed,
    SubscriptionError,
    BufferOverflow,
    Filter(FilterError),
}

impl Error {
    pub fn filter(ferr: FilterError) -> Self {
        Error::Filter(ferr)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum FilterError {
    FieldAlreadyExists,
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

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterError::FieldAlreadyExists => write!(f, "field already exists"),
            FilterError::FieldAlreadyStarted => write!(f, "field already started"),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::DbOpenFailed => write!(f, "Open failed"),
            Error::NotFound => write!(f, "Not found"),
            Error::QueryError => write!(f, "Query failed"),
            Error::DecodeError => write!(f, "Decode error"),
            Error::NoteProcessFailed => write!(f, "Note process failed"),
            Error::TransactionFailed => write!(f, "Transaction failed"),
            Error::SubscriptionError => write!(f, "Subscription failed"),
            Error::BufferOverflow => write!(f, "Buffer overflow"),
            Error::Filter(filter_err) => write!(f, "Filter: {filter_err}"),
        }
    }
}

impl std::error::Error for Error {}
