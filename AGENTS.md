# AGENTS.md

Notes for working in this repo. The part that bites people is bumping the
vendored C, so that gets most of the space.

## Layout

- `nostrdb_rs/` — the `nostrdb` crate, safe bindings over the vendored C.
- `nostrdb_rs/nostrdb/` — the C, as a git submodule. **This is the thing that
  gets bumped.**
- `nostrdb_rs/src/bindings_posix.rs`, `bindings_win.rs` — checked-in bindgen
  output. Not generated at build time; see below.
- `nostrdb_net/` — networking (relays, NIP-77 sync) on top of `nostrdb`.

## Verification, matching CI exactly

CI runs these on Linux, and separately builds and tests on Linux, macOS **and
Windows**. Run all three before committing:

```sh
cargo fmt --all -- --check
cargo clippy -- -D warnings     # note: no --all-targets, no --all-features
cargo test --workspace
```

`cargo clippy --all-targets` surfaces pre-existing warnings in test code that
CI does not gate on. Don't chase them; match the CI command.

## Bumping the vendored C

The C and the Rust wrappers move together, and the checked-in bindings are the
awkward middle. In order:

### 1. Move the submodule

```sh
cd nostrdb_rs/nostrdb
git fetch origin && git checkout <rev>
cd ../.. && git add nostrdb_rs/nostrdb
```

A dirty `nostrdb_rs/nostrdb` in `git status` means the checkout disagrees with
the recorded pointer. `git submodule update` restores the recorded rev — it
*obeys* the pin, it does not advance it. Advancing the pin is the `git add`
above, and it is a commit on the superproject.

Watch the direction when you diff two submodule revs. `git diff <newer>
<older>` prints history in reverse and reads exactly like a pile of deletions.
Confirm with `git merge-base --is-ancestor A B` before concluding anything was
dropped.

### 2. Read the API delta before touching bindings

```sh
cd nostrdb_rs/nostrdb
git diff <old> <new> -- src/nostrdb.h src/metadata.h
```

This decides your next step, so actually read it:

- **Only function declarations, `#define`s and doc comments changed** → the
  bindings can be patched by hand, and for `bindings_win.rs` they must be.
- **Any struct or enum body changed** → hand-patching is not safe. Every
  binding file has to be regenerated on its own platform, or you are handing
  Rust a layout the C does not agree with.

### 3. Regenerate `bindings_posix.rs`

```sh
cargo build --features bindgen
```

`build.rs` only runs bindgen behind that feature — a normal build never
regenerates. It writes `src/bindings_win.rs` on Windows and
`src/bindings_posix.rs` everywhere else.

**`bindings_posix.rs` is shared by the Linux and macOS CI jobs**, and whichever
platform you generate on stamps its libc into the file. Regenerating on a Mac
flips ~2000 lines of glibc typedefs to `__darwin_*` ones and vice versa. That
churn is mostly harmless — the nostrdb structs are built from fixed-width and
plain C scalar types, and bindgen's `bindgen_test_layout_*` tests compare
generated Rust types against constants from the same run, so they stay
self-consistent wherever they execute. But confirm it rather than assume it,
because it stops being harmless the moment a nostrdb struct picks up a
platform-divergent member (`pthread_mutex_t` is 64 bytes on glibc and 8 on
Darwin):

```sh
python3 - <<'PY'
import re
s = open('nostrdb_rs/src/bindings_posix.rs').read()
bad = [m.group(1) for m in
       re.finditer(r'pub struct ((?:ndb|nostr|bech32|cursor|nip44)\w*)\s*\{(.*?)\n\}', s, re.S)
       if re.search(r'__darwin|pthread|FILE', m.group(2))]
print("structs referencing platform types:", bad or "NONE")
PY
```

`NONE` means the regenerated file is safe to ship to the other platform.
Anything else means stop and regenerate on the platform that will run it.

`bindgen` is declared as `"0.69.1"`, a caret range, so a regeneration also
picks up whatever 0.69.x resolves today and rewrites the header line. Expect
that in the diff; it is not a mistake.

### 4. Patch `bindings_win.rs` by hand

There is no cross-generating bindgen here, so unless you are on Windows this
one is edited manually: copy the new `extern "C"` blocks and constants across
from `bindings_posix.rs`, and delete what the C removed. Then check every
symbol appears in both:

```sh
for sym in ndb_new_thing NDB_NEW_CONST; do
  printf '%-28s posix=%s win=%s\n' "$sym" \
    "$(grep -c "\b$sym\b" nostrdb_rs/src/bindings_posix.rs)" \
    "$(grep -c "\b$sym\b" nostrdb_rs/src/bindings_win.rs)"
done
```

It can at least be typechecked standalone without a Windows box:

```sh
{ echo '#![allow(warnings)]'; cat nostrdb_rs/src/bindings_win.rs; } > /tmp/wrap.rs
rustc --edition 2021 --crate-type lib --emit=metadata -o /tmp/wrap.rmeta /tmp/wrap.rs
```

That catches syntax and type errors, not link errors. Say so in the commit
message and let the Windows CI job be the real check.

### 5. Fix the Rust wrappers

A removed or renamed C symbol shows up as a **link** error, not a compile
error — `Undefined symbols: _ndb_whatever, referenced from: ...`. The bindings
still declare it; the library no longer defines it. Grep the message for the
symbol name and follow it back to the wrapper in `nostrdb_rs/src/`.

Port the wrapper to whatever the C now expresses rather than preserving the old
Rust signature over it. When the C renames a concept, rename it here too — a
wrapper that keeps the old name is a wrapper that lies about what the database
does.

### 6. Check downstream before calling it done

A wrapper rename is a breaking change for consumers, and the compiler here will
not tell you. notedeck is the main one. Point it at your working tree without
editing its `Cargo.toml`:

```sh
cd ~/dev/notedeck
cargo check --workspace \
  --config "patch.'https://github.com/damus-io/nostrdb-rs'.nostrdb.path='<path>/nostrdb_rs'" \
  --config "patch.'https://github.com/damus-io/nostrdb-rs'.nostrdb_net.path='<path>/nostrdb_net'"
```

Grep is not a substitute for this — a call site is easy to miss by eye. If it
breaks, the fix belongs in notedeck as its own change; note it in the commit
message and on the card rather than leaving it silently broken.

## Callbacks across FFI

`Config` holds `Arc`s and hands nostrdb `Arc::as_ptr`, so the raw pointer C
holds is a *borrow* and ownership never leaves Rust. `Ndb` clones the `Arc`, so
the closure outlives the threads that call it and drops with the database.
Follow that shape for any new callback: no `Box::into_raw`, nothing leaked,
nothing freed in a trampoline.

Bounds follow the threads that actually call the closure, so check the C:

- `sub_cb` runs on the writer thread.
- `ingest_filter` runs on all `ingester_threads` at once.

Both are `Fn + Send + Sync` because `Config` is `Clone` and one closure can back
several `Ndb`s. `Config` is deliberately **not** `Copy` — a `Copy` type holding
an owning pointer is how you get a double free.

A panic in a trampoline crosses `extern "C"` and aborts the process. Prefer
returning a value that fails closed.
