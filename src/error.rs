use std::fmt;

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    DbOpenFailed,
    NotFound,
    DecodeError,
    NoteProcessFailed,
    TransactionFailed,
    SubscriptionError,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Error::DbOpenFailed => "Open failed",
            Error::NotFound => "Not found",
            Error::DecodeError => "Decode error",
            Error::NoteProcessFailed => "Note process failed",
            Error::TransactionFailed => "Transaction failed",
            Error::SubscriptionError => "Subscription failed",
        };
        write!(f, "{}", s)
    }
}

impl std::error::Error for Error {}
