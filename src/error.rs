#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    DbOpenFailed,
    NotFound,
    DecodeError,
    NoteProcessFailed,
    TransactionFailed,
}
