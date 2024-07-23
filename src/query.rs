use crate::{bindings, Note, NoteKey, Transaction};

#[derive(Debug)]
pub struct QueryResult<'a> {
    pub note: Note<'a>,
    pub note_size: u64,
    pub note_key: NoteKey,
}

impl<'a> QueryResult<'a> {
    pub fn new(result: &bindings::ndb_query_result, txn: &'a Transaction) -> Self {
        QueryResult {
            note: Note::new(
                result.note,
                result.note_size as usize,
                NoteKey::new(result.note_id),
                txn,
            ),
            note_size: result.note_size,
            note_key: NoteKey::new(result.note_id),
        }
    }
}
