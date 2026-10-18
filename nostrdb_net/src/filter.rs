use serde_json::{Value, json};

pub type Filter = nostrdb::Filter;

/// The largest `until` nostrdb's [`Filter::from_json`] accepts: a larger value
/// fails with `BufferOverflow`.
const NDB_MAX_UNTIL: u64 = u32::MAX as u64;

/// Parse a NIP-01 wire filter into the nostrdb [`Filter`] that matches the same
/// events.
///
/// The two disagree on `until`. NIP-01's is inclusive (`created_at <= until`),
/// and that is how a relay like strfry answers. nostrdb's is exclusive: it keeps
/// a note only when `created_at < until`. A wire filter handed straight to
/// [`Filter::from_json`] therefore drops every event stamped exactly `until`.
/// That is harmless for a "recent notes" query, but it breaks anything that
/// partitions a range into `[since, mid]` and `[mid + 1, until]`, because the
/// event on `mid` lands in neither half.
///
/// So this writes `until + 1` before parsing. When that would pass the 32-bit
/// bound nostrdb accepts (`until` is `u32::MAX`, the search bound the windowed
/// reconciles use), it drops `until` instead. That is a superset only for
/// events stamped after the year 2106. `since` is inclusive on both sides and is
/// left alone, as is a filter without an `until` or one that isn't a JSON
/// object.
pub fn ndb_filter_from_wire(wire_json: &str) -> Result<Filter, nostrdb::Error> {
    let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(wire_json) else {
        return Filter::from_json(wire_json);
    };
    let Some(until) = obj.get("until").and_then(Value::as_u64) else {
        return Filter::from_json(wire_json);
    };
    if until >= NDB_MAX_UNTIL {
        obj.remove("until");
    } else {
        obj.insert("until".to_owned(), json!(until + 1));
    }
    Filter::from_json(&Value::Object(obj).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostrdb::{Config, Ndb, NoteBuilder, Transaction};

    /// Ingest one signed kind-1 note per `created_at` and wait until all are
    /// queryable.
    fn ndb_with_notes(created_at: &[u64]) -> (tempfile::TempDir, Ndb) {
        let dir = tempfile::TempDir::new().expect("tmp dir");
        let ndb = Ndb::new(dir.path().to_str().expect("path"), &Config::new()).expect("ndb");
        for (i, ts) in created_at.iter().enumerate() {
            let note = NoteBuilder::new()
                .kind(1)
                .content(&format!("note-{i}"))
                .created_at(*ts)
                .sign(&[3u8; 32])
                .build()
                .expect("note");
            let frame = format!(r#"["EVENT",{}]"#, note.json().expect("json"));
            ndb.process_client_event(&frame).expect("ingest");
        }
        for _ in 0..1000 {
            if matching(&ndb, &Filter::new().kinds([1]).build()).len() == created_at.len() {
                return (dir, ndb);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("notes never became queryable");
    }

    /// The `created_at`s `filter` matches in `ndb`, sorted.
    fn matching(ndb: &Ndb, filter: &Filter) -> Vec<u64> {
        let txn = Transaction::new(ndb).expect("txn");
        let mut out: Vec<u64> = ndb
            .query(&txn, std::slice::from_ref(filter), 1000)
            .expect("query")
            .iter()
            .map(|r| r.note.created_at())
            .collect();
        out.sort_unstable();
        out
    }

    /// A wire `until` is inclusive after translation, where nostrdb's own parse
    /// of the same JSON drops the boundary event. The second assertion pins
    /// nostrdb's exclusive `until`, the reason this function exists.
    #[test]
    fn wire_until_is_inclusive() {
        let (_dir, ndb) = ndb_with_notes(&[99, 100, 101]);
        let wire = r#"{"kinds":[1],"since":99,"until":100}"#;

        let translated = ndb_filter_from_wire(wire).expect("filter");
        assert_eq!(matching(&ndb, &translated), vec![99, 100]);

        let raw = Filter::from_json(wire).expect("filter");
        assert_eq!(
            matching(&ndb, &raw),
            vec![99],
            "nostrdb's until is exclusive"
        );
    }

    /// `until = u32::MAX` would overflow nostrdb's bound at `+ 1`, so it is
    /// dropped, and the top event still matches.
    #[test]
    fn max_until_drops_the_bound() {
        let top = u32::MAX as u64;
        let (_dir, ndb) = ndb_with_notes(&[5, top]);
        let wire = format!(r#"{{"kinds":[1],"since":5,"until":{top}}}"#);
        let filter = ndb_filter_from_wire(&wire).expect("filter");
        assert_eq!(filter.until(), None);
        assert_eq!(matching(&ndb, &filter), vec![5, top]);
    }

    /// A filter without an `until` parses exactly as nostrdb would parse it.
    #[test]
    fn no_until_passes_through() {
        let filter = ndb_filter_from_wire(r#"{"kinds":[1],"since":5}"#).expect("filter");
        assert_eq!(filter.until(), None);
        assert_eq!(filter.since(), Some(5));
    }
}
