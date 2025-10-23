# nostrdb-rs

[![ci](https://github.com/damus-io/nostrdb-rs/actions/workflows/rust.yml/badge.svg)](https://github.com/damus-io/nostrdb-rs/actions)
[![docs](https://img.shields.io/docsrs/nostrdb)](https://docs.rs/nostrdb)

`nostrdb-rs` is a Rust binding to the [nostrdb](https://github.com/damus-io/nostrdb) C library: an unfairly fast embedded database for Nostr events, profiles, and relay metadata built on top of LMDB. It is designed for servers, agents, and clients that need deterministic, low-latency access to Nostr data while retaining the ability to ingest and query at scale.

---

## Highlights

- **Embedded & deterministic**: stores all data in a local LMDB environment, so your application owns its state and can ship with no external service dependencies.
- **Fast ingestion**: drop raw relay/client messages in (`["EVENT", ...]`), nostrdb handles validation, indexing, and profile synchronization in native code.
- **Rich query API**: filter by ids, authors, kinds, tags, time windows, full-text search, or custom predicates.
- **Async-first subscriptions**: integrate with `tokio` to await new events, automatically wake tasks, and unsubscribe cleanly.
- **Profile helpers**: keep profile metadata up to date and fetch records via either primary key or pubkey.
- **Event builders**: construct, sign, and serialize notes entirely in memory before ingesting or publishing.

---

## Quick Start

```bash
# Clone with the required C submodule
git clone --recurse-submodules https://github.com/damus-io/nostrdb-rs.git
cd nostrdb-rs

# Build and run tests
cargo build
cargo test
```

```rust
use nostrdb::{
    Config, Filter, IngestMetadata, Ndb, Result, SubscriptionStream, Transaction,
};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::new()
        .set_mapsize(1 << 30)          // 1 GiB LMDB map
        .set_ingester_threads(4);      // C ingestion workers

    let db = Ndb::new("data/nostr", &config)?;

    // Process an incoming relay EVENT
    db.process_event(r#"["EVENT","subid",{"id":"...","pubkey":"...","kind":1,"created_at":1700000000,"tags":[],"content":"hello"}]"#)?;

    // Query with a transaction
    let txn = Transaction::new(&db)?;
    let filter = Filter::new()
        .authors([&[0u8; 32]])     // binary pubkey
        .kinds([1])
        .limit(32)
        .build();
    let results = db.query(&txn, &[filter], 64)?;
    for result in results {
        println!("{}: {}", hex::encode(result.note_key.as_u64().to_be_bytes()), result.note.content());
    }

    // Subscribe for new notes asynchronously
    let sub = db.subscribe(&[Filter::new().kinds([1]).build()])?;
    let mut stream = SubscriptionStream::new(db.clone(), sub).notes_per_await(16);
    while let Some(keys) = stream.next().await {
        for key in keys {
            println!("New note key {}", key.as_u64());
        }
    }

    Ok(())
}
```

---

## Installation & Tooling

- **Rust**: Stable Rust (1.74+) with `cargo` is recommended.
- **C toolchain**: `nostrdb` pulls C sources that require `clang`/`gcc`, `make`, and an LMDB-compatible environment. Windows builds link against `bcrypt`/`advapi32`.
- **Submodules**: The repository embeds the upstream `nostrdb` C code as a git submodule. Always clone with `--recurse-submodules` or run `git submodule update --init --recursive` after cloning.
- **Optional bindgen**: The default build uses pre-generated bindings. Enable the `bindgen` feature if you need to regenerate bindings (requires `clang`).

For Nix users, `shell.nix` sets up an appropriate environment.

---

## Project Layout

- `src/ndb.rs`: safe wrapper around the core database handle.
- `src/ingest.rs`: ingestion metadata helpers.
- `src/filter.rs`: filter builder, iterators, and custom filter support.
- `src/transaction.rs`: RAII transaction helper.
- `src/note.rs`: note representation, note builder, and note metadata accessors.
- `src/profile.rs`, `src/ndb_profile.rs`: profile access helpers.
- `src/future.rs`: subscription stream implementation for async consumers.
- `build.rs`: compiles the embedded C library (`nostrdb`, LMDB, secp256k1, etc.).

---

## Getting Started in Detail

### 1. Configure the Database

```rust
let config = Config::new()
    .set_mapsize(1 << 33)        // grow the LMDB map (power of two is recommended)
    .set_ingester_threads(2)     // number of nostrdb ingestion workers
    .skip_validation(false);     // enable signature checks (default)
```

- `set_mapsize` controls the LMDB memory map size. Choose a value large enough for your dataset; LMDB requires resizing when you outgrow the map.
- `set_ingester_threads` determines how many worker threads the C runtime starts for ingestion/write-side work.
- `skip_validation(true)` can disable note verification (dangerous unless you trust input sources).
- `set_sub_callback` allows you to hook into subscription wake-ups for custom scheduling.

### 2. Open the Database

`Ndb::new(path, &config)` opens or creates the LMDB environment at `path`. The directory will be created if it does not exist.

Only one `Ndb` is needed per process. Cloning an `Ndb` is cheap because it wraps an `Arc`.

### 3. Ingest Events

You can ingest either relay-originating messages or client-originating events.

```rust
// Relay -> server message
db.process_event(r#"["EVENT","subscription-id",{...}]"#)?;

// Client -> server message
db.process_client_event(r#"["EVENT",{...}]"#)?;

// Attach metadata (e.g., relay URL)
db.process_event_with(json, IngestMetadata::new().relay("wss://relay.example.com"));
```

- Input strings must be valid UTF-8 JSON formatted according to the Nostr protocol.
- Ingestion runs asynchronously in native code. Errors surface only for catastrophic failures (e.g., JSON parse problems or LMDB write failure).

### 4. Use Transactions for Queries

Queries must run inside a `Transaction`. Only one transaction can be active per thread.

```rust
let txn = Transaction::new(&db)?;
let filter = Filter::new()
    .search("satoshi")
    .since(1690000000)
    .until(1700000000)
    .limit(128)
    .build();

let notes = db.query(&txn, &[filter], 256)?;
for entry in notes {
    let note = entry.note;
    println!("{} | {}", note.created_at(), note.content());
}
```

Transactions are automatically closed when dropped. Always drop (or let go out of scope) before creating another transaction on the same thread.

### 5. Build Complex Filters

`Filter::new()` returns a `FilterBuilder` that exposes ergonomic methods:

- `ids`, `authors`, `pubkeys`, `pubkey` (aliases), `kinds`, `tags`
- `search` for substring searches.
- `since` / `until` for time windows.
- `limit` to cap results.
- `events`, `event` helpers to match `e` tags.
- `custom` to provide a closure `FnMut(Note<'_>) -> bool` executed inside nostrdb (be mindful of performance).

Filters can be cloned and reused across subscriptions and queries.

### 6. Consume Subscriptions Asynchronously

```rust
let filter = Filter::new().kinds([1]).limit(0).build();
let sub = db.subscribe(&[filter])?;
tokio::spawn({
    let db = db.clone();
    async move {
        let mut stream = SubscriptionStream::new(db, sub).notes_per_await(32);
        while let Some(keys) = stream.next().await {
            println!("Fetched {} notes", keys.len());
        }
    }
});
```

- `SubscriptionStream` stores a `Waker` internally so your async tasks resume as soon as new data arrives or the subscription completes.
- Streams automatically unsubscribe on drop unless you call `.unsubscribe_on_drop(false)`.
- For blocking code, use `Ndb::wait_for_notes(sub, max_notes).await`.

### 7. Retrieve Notes, Profiles, and Relays

```rust
let txn = Transaction::new(&db)?;
let note = db.get_note_by_id(&txn, &note_id_bytes)?;
let json = note.json()?;                 // raw event JSON
let tags = note.tags();                  // iterate structured tags
let relays = note.relays(&txn);          // all relays this note was seen on

let profile = db.get_profile_by_pubkey(&txn, &pubkey_bytes)?;
println!("Profile display name: {}", profile.content().name());
```

Transactional notes borrow the transaction lifetime. Clone or call `note.json()` to materialize data if you need to hold it after the transaction ends.

### 8. Create and Sign Notes

```rust
use nostrdb::{NoteBuilder, NoteBuildOptions};

let seckey = [0u8; 32];
let mut builder = NoteBuilder::new()
    .kind(1)
    .content("hello, nostrdb!")
    .start_tag().tag_str("p").tag_id(&recipient).start_tag()
    .tag_str("client").tag_str("nostrdb-rs");

let note = builder
    .sign(&seckey)                 // automatically sets pubkey + signature
    .build()
    .expect("note build");
db.process_client_event(note.json()?.as_str())?;
```

- `NoteBuilder::with_bufsize` lets you control the scratch buffer when generating large notes.
- `NoteBuildOptions::created_at(false)` or `NoteBuilder::created_at(ts)` to override timestamps.
- You can pass a precomputed signature via `.sig(signature)` if you handle signing externally.

### 9. Error Handling

All public APIs return `nostrdb::Result<T>` with errors defined in `Error`:

- `DbOpenFailed`, `TransactionFailed`, `QueryError`, `SubscriptionError`, etc.
- Filter helpers produce `FilterError` variants when fields are misused (e.g., appending twice).
- `process_*` methods return `NoteProcessFailed` if ingestion was rejected.

Use `?` and match on specific variants for precise recovery.

### 10. Observability & Debugging

- Set `NDB_LOG=1` before building to enable debug logging inside the C library.
- The Rust wrapper uses `tracing`, so add a subscriber (e.g., `tracing_subscriber::fmt::init()`) to view logs.
- `nostrdb::Ndb::subscription_count()` can be used to monitor active subscriptions.
- For test environments, the `src/test_util.rs` helpers handle fixture directories.

---

## Integrating With Larger Systems

### Embedding in Agents or Services

- Spawn a background task to receive from relays and call `process_event_with` along with metadata about the relay connection.
- Use dedicated worker threads for ingestion when `set_ingester_threads` > 0; the runtime handles coordination.
- Use a channel or queue to forward internal messages to the subscription stream for the components that need near-real-time updates.

### Cross-language & FFI

- If you need to expose nostrdb to other runtimes, `nostrdb-rs` keeps the raw C handles accessible via `Ndb::as_ptr()` and `Transaction::as_mut_ptr()`. Use with caution; correctness relies on nostrdb’s threading guarantees.

### Storage Considerations

- LMDB map size must grow ahead of usage. Monitor free pages and increase via `Config::set_mapsize` (requires reopening).
- The database directory stores LMDB data (`data.mdb`, `lock.mdb`) plus nostrdb indexes.

---

## Testing & Maintenance

- Run `cargo test` to execute Rust unit tests (requires writable `target/testdbs`).
- CI uses GitHub Actions (`rust.yml`).
- When updating the C submodule, rerun `cargo build` to rebuild static libraries.
- Enable `--features bindgen` only when regenerating bindings; check updated bindings into source control.

---

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

---

## Maintainer Notes / Open Questions

- Recommended production `mapsize` guidance (beyond generic LMDB advice) needs confirmation.
- Preferred defaults for `set_ingester_threads` under heavy load are not documented.
- The repo does not currently include `nostrd-rs`; clarify whether there is an official companion project and how it integrates.
- Best practices for pruning old events or compacting the LMDB environment are not yet described.
- Confirm whether the `Filter::custom` closure is safe to block or should remain non-blocking; suggest guidelines if necessary.
