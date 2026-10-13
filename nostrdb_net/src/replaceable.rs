//! Resolving addressable / replaceable nostr events out of nostrdb.
//!
//! nostrdb keeps *every* revision of a replaceable event (kinds 10000–19999 and
//! the parameterized 30000–39999 range) — it does not collapse them internally.
//! So a reader that wants the *effective* state has to fold the matching notes
//! and keep, per address, the revision with the highest `created_at`, a
//! same-second tie going to the larger id (NIP-01's rule, reversed: the
//! direction doesn't matter so long as every reader agrees, and nostrdb hands
//! same-second revisions back in no particular order).
//!
//! This is that fold, shared so every consumer (calendar sync, Dave sessions,
//! Horizon, …) resolves replaceable events the same way. It will likely move
//! into nostrdb itself eventually; living here keeps it in one place until then.

use nostrdb::{Filter, Ndb, Note, NoteKey, Transaction};
use std::collections::HashMap;

/// Resolve replaceable events matching `filters`, deduplicating by `d` tag.
///
/// Returns one [`NoteKey`] per unique `d`-tag value — the revision with the
/// largest `(created_at, id)`, so a same-second tie resolves the same way
/// whatever order nostrdb returns the revisions in. Notes without a `d` tag
/// are ignored. See the module
/// docs for why this is necessary.
pub fn query_replaceable(ndb: &Ndb, txn: &Transaction, filters: &[Filter]) -> Vec<NoteKey> {
    query_replaceable_filtered(ndb, txn, filters, |_| true)
}

/// Like [`query_replaceable`], but `predicate` decides whether the *winning*
/// (latest) revision of each `d` tag is kept.
///
/// The predicate is evaluated only against the latest revision of each group; an
/// older revision passing it does not resurrect the group. This matters when
/// callers want "the current state, but only if it still satisfies X".
pub fn query_replaceable_filtered(
    ndb: &Ndb,
    txn: &Transaction,
    filters: &[Filter],
    predicate: impl Fn(&Note) -> bool,
) -> Vec<NoteKey> {
    // For each d-tag value track the largest (created_at, id) seen and, when
    // that revision passes the predicate, its key. Notes can arrive in any
    // order, so we always advance on a larger stamp and only retain a key when
    // valid.
    let best = ndb.fold(
        txn,
        filters,
        HashMap::<String, Winner>::new(),
        |mut acc, note| {
            let Some(d_tag) = tag_value(&note, "d") else {
                return acc;
            };
            let stamp = (note.created_at(), *note.id());
            if acc.get(d_tag).is_some_and(|best| stamp <= best.stamp) {
                return acc;
            }
            let key = predicate(&note).then(|| note.key().expect("queried note has a key"));
            acc.insert(d_tag.to_string(), Winner { stamp, key });
            acc
        },
    );

    match best {
        Ok(map) => map.into_values().filter_map(|winner| winner.key).collect(),
        Err(_) => Vec::new(),
    }
}

/// The newest revision of one `d` tag seen so far in
/// [`query_replaceable_filtered`]'s fold.
struct Winner {
    /// Its `(created_at, id)`: the order revisions supersede in.
    stamp: (u64, [u8; 32]),
    /// Its key, or `None` when it failed the predicate.
    key: Option<NoteKey>,
}

/// First value of the single-letter-or-named tag `name` on `note`, if any.
fn tag_value<'a>(note: &'a Note<'a>, name: &str) -> Option<&'a str> {
    note.tags().iter().find_map(|tag| {
        (tag.count() >= 2 && tag.get_str(0) == Some(name)).then(|| tag.get_str(1))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FullKeypair;
    use nostrdb::{Config, NoteBuilder};
    use std::time::Duration;

    /// A parameterized-replaceable kind for the tests.
    const KIND: u32 = 30078;

    /// A signed revision of `d` by `kp`, stamped `created_at`.
    fn revision(kp: &FullKeypair, d: &str, content: &str, created_at: u64) -> Note<'static> {
        NoteBuilder::new()
            .kind(KIND)
            .content(content)
            .created_at(created_at)
            .start_tag()
            .tag_str("d")
            .tag_str(d)
            .sign(&kp.secret_key.secret_bytes())
            .build()
            .expect("note")
    }

    /// Ingest `notes` in order into a fresh ndb and return, in `d` order, the
    /// content of the revision [`query_replaceable`] picks for each `d`.
    async fn winners(kp: &FullKeypair, notes: &[&Note<'_>]) -> Vec<String> {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let ndb = Ndb::new(dir.path().to_str().unwrap(), &Config::new()).expect("ndb");
        let filter = Filter::new()
            .authors([kp.pubkey.bytes()])
            .kinds([KIND as u64])
            .build();
        let sub = ndb.subscribe(std::slice::from_ref(&filter)).expect("sub");
        let waiter =
            ndb.wait_for_all_notes_within(sub, notes.len() as u32, Duration::from_secs(10));
        for note in notes {
            let json = note.json().expect("json");
            ndb.process_event(&format!("[\"EVENT\",\"t\",{json}]"))
                .expect("ingest");
        }
        waiter.await.expect("ingested");

        let txn = Transaction::new(&ndb).expect("txn");
        let mut winners: Vec<Note<'_>> = query_replaceable(&ndb, &txn, &[filter])
            .into_iter()
            .map(|key| ndb.get_note_by_key(&txn, key).expect("winner"))
            .collect();
        winners.sort_by_key(|note| tag_value(note, "d").map(str::to_owned));
        winners
            .iter()
            .map(|note| note.content().to_owned())
            .collect()
    }

    /// Two revisions of each of several `d`s in the same second: the larger
    /// id wins, whichever was ingested first. Several, because the id order
    /// of one random pair matches nostrdb's return order half the time.
    #[tokio::test]
    async fn a_same_second_tie_goes_to_the_larger_id() {
        let kp = FullKeypair::generate();
        let pairs: Vec<[Note<'static>; 2]> = (0..8)
            .map(|i| {
                let d = i.to_string();
                [revision(&kp, &d, "a", 1000), revision(&kp, &d, "b", 1000)]
            })
            .collect();
        let want: Vec<String> = pairs
            .iter()
            .map(|[a, b]| if a.id() > b.id() { "a" } else { "b" }.to_owned())
            .collect();

        let a_first: Vec<&Note<'_>> = pairs.iter().flat_map(|[a, b]| [a, b]).collect();
        let b_first: Vec<&Note<'_>> = pairs.iter().flat_map(|[a, b]| [b, a]).collect();
        assert_eq!(winners(&kp, &a_first).await, want);
        assert_eq!(winners(&kp, &b_first).await, want);
    }

    /// A later revision beats an earlier one, whatever their ids.
    #[tokio::test]
    async fn created_at_outranks_the_id() {
        let kp = FullKeypair::generate();
        let old = revision(&kp, "x", "old", 1000);
        let new = revision(&kp, "x", "new", 1001);

        assert_eq!(winners(&kp, &[&old, &new]).await, ["new"]);
        assert_eq!(winners(&kp, &[&new, &old]).await, ["new"]);
    }
}
