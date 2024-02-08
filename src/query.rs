use crate::{bindings, Note, Transaction};

pub struct QueryResult<'a> {
    pub note: Note<'a>,
    pub note_size: u64,
    pub note_key: u64,
}

impl<'a> QueryResult<'a> {
    pub fn new(result: &bindings::ndb_query_result, txn: &'a Transaction) -> Self {
        QueryResult {
            note: Note::new_transactional(
                result.note,
                result.note_size as usize,
                result.note_id,
                txn,
            ),
            note_size: result.note_size,
            note_key: result.note_id,
        }
    }
}
