# Rust Guide

This document bridges the nostrdb mdBook (architecture, metadata, CLI workflows)
and the Rust crate API. Use it as a checklist when wiring nostrdb-rs into an
application.

## Environment

1. Install a recent Rust toolchain (1.75+ recommended).
2. Install `clang`/`libclang`. `bindgen` will look for `libclang.so` / `libclang.dylib`;
   set `LIBCLANG_PATH` if it lives outside your system default.
3. Ensure a C build toolchain is available: `cmake`, `pkg-config`, `make`, and a
   C11 compiler (Clang or GCC). The build script compiles the vendored `nostrdb`
   sources located in `./nostrdb`.
4. Clone submodules: `git submodule update --init --recursive`.
5. Optional: install [`mdbook`](https://rust-lang.github.io/mdBook/) if you want to
   read the upstream documentation locally: `mdbook serve docs/book --open` from the
   nostrdb repo.

## Building & testing

```bash
cargo build              # builds Rust + C artifacts
cargo test               # runs the integration tests
```

To regenerate bindings after changing headers or schemas:

```bash
cargo clean
cargo build --features bindgen
```

This sets the `BINDGEN` cfg path in `build.rs` so the C headers are parsed again.
Otherwise the pre-generated Rust bindings in `src/bindings*.rs` are used.

## Examples

The `examples/` directory mirrors the workflows described in the nostrdb mdBook
*Getting Started* and *CLI Guide* chapters:

- `ingest.rs` – import LDJSON using `Ndb::process_events_with`.
- `query.rs` – build nostr filters, execute queries, and print JSON.
- `subscription.rs` – subscribe to filters asynchronously with `SubscriptionStream`.

Run them with `cargo run --example <name> -- [args...]`. See the example files for
CLI flags; they intentionally match the upstream `ndb` tool.

## Mapping chapters to Rust types

| mdBook chapter | Rust focus |
| --- | --- |
| Getting Started | `Config`, `Ndb`, `Ndb::process_event(s)` |
| Architecture | `Ndb`, `Transaction`, `Note`, metadata structs |
| API Tour | `FilterBuilder`, `NoteBuilder`, `Subscription`, `NoteMetadataBuilder` |
| CLI Guide | Examples + `query.rs`, `subscription.rs` |
| Metadata | `NoteMetadata`, `NoteMetadataBuilder`, `ReactionEntry` |
| Language Bindings | Build scripts (`build.rs`, `src/bindings_*`) |

Whenever the mdBook changes, update references in this guide plus the Rust inline
docs so both stay synchronized.
