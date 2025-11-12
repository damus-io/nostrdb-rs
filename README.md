# nostrdb-rs

[![ci](https://github.com/damus-io/nostrdb-rs/actions/workflows/rust.yml/badge.svg)](https://github.com/damus-io/nostrdb-rs/actions)
[![docs](https://img.shields.io/docsrs/nostrdb)](https://docs.rs/nostrdb)

Rust bindings for [nostrdb], the unfairly fast LMDB-backed nostr datastore.
This crate exposes safe wrappers around the C engine—open a database, ingest
events, run zero-copy filters, and subscribe to updates from async Rust.

[nostrdb]: https://github.com/damus-io/nostrdb

## Documentation

- **Engine reference**: the upstream [nostrdb mdBook](https://github.com/damus-io/nostrdb/tree/master/docs/book)
  covers architecture, metadata formats, and CLI workflows. Build it locally with
  `mdbook serve docs/book --open`.
- **Crate docs**: `cargo doc --open` or [docs.rs/nostrdb](https://docs.rs/nostrdb) for the
  Rust-specific API surface (re-exported types, builders, async helpers).
- **Rust guide**: see [`docs/rust.md`](docs/rust.md) for environment notes,
  binding regeneration, and idiomatic usage patterns.

## Requirements

- Rust 1.75+ (edition 2021)
- `clang`/`libclang` for `bindgen` (`LIBCLANG_PATH` may be required on macOS/Nix)
- `cmake`, `pkg-config`, `make`, and a C11 compiler (used to build the vendored nostrdb)
- Optional: `zstd`, `curl`, and the nostrdb fixtures if you run the examples end-to-end

The `nostrdb` C sources live in the `nostrdb/` submodule; run
`git submodule update --init --recursive` after cloning.

## Quick start

```bash
git clone https://github.com/damus-io/nostrdb-rs.git
cd nostrdb-rs
git submodule update --init --recursive
cargo test               # builds the C core and runs the Rust test suite

# Try the examples (see docs/rust.md for details)
cargo run --example ingest -- testdata/many-events.json
cargo run --example query  -- --kind 1 --search nostrdb --limit 5
```

Examples reuse the same fixtures described in the nostrdb mdBook Getting Started
chapter. To grab them quickly:

```bash
make -C nostrdb testdata/many-events.json
```

## Usage snapshot

```rust
use nostrdb::{Config, Filter, Ndb, NoteBuilder};

let mut config = Config::default();
config.skip_verification(true);
config.writer_scratch_size(4 * 1024 * 1024);
let ndb = Ndb::open("./data", &config)?;

// ingest a JSON event (skipping signatures here)
let raw = include_str!("../testdata/sample-event.json");
ndb.process_event(raw)?;

// build a filter (kind 1 + text search) and iterate results
let filter = Filter::new()
    .kinds([1])
    .search("nostrdb")
    .build()?;
let mut txn = ndb.txn()?;
for note in txn.query(&filter)? {
    println!("{}: {}", note.id_hex(), note.content());
}
```

See [`examples/`](examples) and the mdBook *CLI Guide* for richer workflows
(thread queries, relay metadata, async subscriptions).

## Regenerating bindings

When the `schemas/*.fbs` or C headers change upstream:

```bash
cargo clean
cargo build --features bindgen   # or run build.rs manually with BINDGEN=1
```

This reruns `bindgen` against the vendored nostrdb headers. CI uses the
pre-generated bindings by default to avoid requiring libclang everywhere.

## License

GPL-3.0-or-later (same as upstream nostrdb).
