//! Shared sync plumbing for CLIs that drive a running notedeck over its embedded
//! relay (see `headway_cli` and `notebook_cli`).
//!
//! A CLI keeps its **own** nostrdb as a cache. Each run it reconciles that cache
//! against the relay with NIP-77 negentropy — pulling the events the relay has
//! that it lacks and pushing the ones it holds that the relay lacks — then folds
//! its document locally with a domain reducer. Edits forward the events they
//! produce back to the relay so the running app sees the change.
//!
//! This module is domain-agnostic: it deals in event kinds, nostrdb filters and
//! `["EVENT", {...}]` frames, not boards or canvases. Each CLI supplies its kinds,
//! a filter, and a predicate naming which kinds are addressable (latest-wins,
//! keyed per `(kind, d-tag)`) so stale revisions aren't re-pushed forever.

mod relay;
mod session;

use std::collections::{HashMap, HashSet};

use crate::Pubkey;
use negentropy::{Id, NegentropyStorageVector};
use nostrdb::{Config, Filter, Ndb, Note, Transaction};
use serde_json::json;

pub use relay::{Diff, Relay, TooManyResults, Transient};
pub use session::Session;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Default URL of notedeck's embedded relay (see `--relay-bind`, default
/// `127.0.0.1:6677`).
pub const DEFAULT_RELAY: &str = "ws://127.0.0.1:6677";

/// How many event ids to request per `REQ` when pulling reconciled events down.
/// The relay caps a single `REQ`'s stored replay, so we fetch in chunks under
/// that cap.
const ID_FETCH_CHUNK: usize = 300;

/// Open (creating if needed) a CLI's own nostrdb cache under `<data-dir>/<app>`
/// (e.g. `~/.local/share/headway-cli` on Linux), or at `db` if given.
pub fn open_ndb(db: Option<&str>, app: &str) -> Result<Ndb> {
    let path = match db {
        Some(p) => std::path::PathBuf::from(p),
        None => dirs::data_dir()
            .ok_or("no data dir; pass --db <path>")?
            .join(app),
    };
    std::fs::create_dir_all(&path)?;
    let path = path.to_str().ok_or("db path is not valid utf-8")?;
    Ok(Ndb::new(path, &Config::new())?)
}

/// Reconcile the local cache against the relay both ways, so the cache and the
/// app converge regardless of which side an edit happened on.
///
/// `kinds` is the event kinds to sync (used for the wire filter); `filter` is the
/// matching nostrdb filter (used for the local fold). `is_addressable(kind)` names
/// the kinds the caller treats as latest-wins per `(kind, d-tag)`, so [`frames_where`]
/// pushes only the winning revision of each.
///
/// Best-effort: a relay that doesn't speak NIP-77 falls back to a full NIP-01
/// sync, and a failed flush is warned rather than fatal. A failed *pull* does
/// propagate, matching the original behaviour.
pub async fn reconcile_sync(
    relay: &mut Relay,
    ndb: &Ndb,
    author: &Pubkey,
    kinds: &[u32],
    filter: &Filter,
    is_addressable: &dyn Fn(u32) -> bool,
) -> Result<()> {
    let wire = json!({ "kinds": kinds, "authors": [author.hex()] });
    match relay
        .reconcile(&wire.to_string(), local_set(ndb, filter)?)
        .await
    {
        Ok(diff) => {
            // Pull missing events by id. The relay caps a single `REQ`'s stored
            // replay, so fetch in chunks rather than one oversized filter.
            for chunk in diff.need.chunks(ID_FETCH_CHUNK) {
                let ids: Vec<String> = chunk.iter().map(hex::encode).collect();
                // sync_into returns only once the received events are queryable.
                relay
                    .sync_into(ndb, &json!({ "ids": ids }).to_string())
                    .await?;
            }

            // Push events the relay is missing (e.g. edits made offline).
            // Best-effort: a rejected flush (or a dropped connection mid-push)
            // mustn't abort the command.
            let have: HashSet<[u8; 32]> = diff.have.iter().copied().collect();
            let pending = frames_where(ndb, filter, is_addressable, |id| have.contains(id));
            if !pending.is_empty() {
                match relay.publish(&pending).await {
                    Ok(()) => eprintln!("flushed {} local event(s) to the relay", pending.len()),
                    Err(e) => eprintln!("warning: couldn't flush local events: {e}"),
                }
            }
        }
        // Sync is best-effort. A relay that doesn't speak NIP-77 (an older
        // notedeck, or a plain NIP-01 relay) can't reconcile — fall back to a
        // full NIP-01 sync rather than failing or, worse, hanging. If even that
        // fails, warn and carry on against the cache.
        Err(e) => {
            eprintln!("warning: negentropy reconcile unavailable: {e}");
            if let Err(e) = fallback_sync(relay, ndb, kinds, author, filter, is_addressable).await {
                eprintln!("warning: fallback sync failed: {e}");
            }
        }
    }
    Ok(())
}

/// Pull-only reconcile of `ndb` against `relay` for a single wire filter.
///
/// Unlike [`reconcile_sync`] this is one-directional — it never pushes local
/// events — and fully filter-driven: `filter_json` is the exact wire filter, so
/// whatever `kinds`, `authors`, and `since` it carries are honoured on both the
/// negentropy reconcile and the NIP-01 fallback. `local` is the sealed
/// negentropy set of the matching cached events (see [`local_set`]). It fetches
/// the ids the relay holds that the local db lacks in [`ID_FETCH_CHUNK`]-sized
/// `REQ`s; a relay that can't reconcile falls back to a plain `REQ` pull of the
/// same filter.
///
/// The returned future is `Send`, so it can be `tokio::spawn`ed onto a
/// multi-thread runtime — the reason this primitive exists separately from
/// [`reconcile_sync`], whose bidirectional path holds a `nostrdb::Filter`
/// (`!Send`/`!Sync`) across its awaits. Two things keep it `Send`: it takes no
/// `Filter` (the caller reduces the filter to `filter_json` + `local`
/// synchronously first), and it collapses the reconcile's `!Send` `Box<dyn
/// Error>` into a `Send` value before the next await. Because `sync_into`
/// returns only once the received events are queryable, an `Ok(())` means the
/// pulled history is actually readable — a deterministic settle point, not just
/// "requested".
pub async fn pull_reconcile(
    relay: &mut Relay,
    ndb: &Ndb,
    filter_json: &str,
    local: NegentropyStorageVector,
) -> Result<()> {
    // Collapse the reconcile `Result` (its `Box<dyn Error>` is `!Send`) into a
    // `Send` `Option` before any further await. `None` means the relay can't
    // reconcile, so fall back to a plain NIP-01 `REQ` of the same filter.
    let need = match relay.reconcile(filter_json, local).await {
        Ok(diff) => Some(diff.need),
        Err(_) => None,
    };
    match need {
        Some(need) => {
            // Pull the ids the relay holds that we lack, chunked under the
            // relay's single-`REQ` replay cap.
            for chunk in need.chunks(ID_FETCH_CHUNK) {
                let ids: Vec<String> = chunk.iter().map(hex::encode).collect();
                relay
                    .sync_into(ndb, &json!({ "ids": ids }).to_string())
                    .await?;
            }
        }
        None => {
            relay.sync_into(ndb, filter_json).await?;
        }
    }
    Ok(())
}

/// Pull-only reconcile of a filter that may match more events than the relay's
/// per-sync cap (strfry `maxSyncEvents`) allows, by bisecting its `created_at`
/// range until every window reconciles under the cap.
///
/// A relay refuses a `NEG-OPEN` outright when its filter matches more than the cap
/// — it counts the *whole* match, not the set difference, and streams no partial
/// result (verified against strfry, which logs `QUERY size exceeded` and replies
/// `NEG-ERR ... "too many query results"`). So a single [`pull_reconcile`] can't
/// sync such a filter. This reconciles the full range first — one pass for any
/// set already under the cap — and on a [`TooManyResults`] refusal splits the
/// window in half and retries each half, recursing until each window is under the
/// cap; every in-cap window then pulls its diff exactly as [`pull_reconcile`] does.
///
/// `base_filter_json` is the wire filter (its own `since`/`until`, if any, clamp
/// the initial window; `until_now` supplies the upper bound when it carries no
/// `until`). Windows are disjoint (`[since, mid]` and `[mid+1, until]`), so events
/// are neither double-fetched nor skipped at a boundary. A window that stays over
/// the cap even at one-second width (a pathological >cap events in a single
/// second) falls back to a plain bounded `REQ` pull. A non-cap reconcile error
/// (e.g. a relay that doesn't speak NIP-77) propagates so the caller can fall back.
///
/// `Send`, like [`pull_reconcile`]: it holds no `nostrdb::Filter` across an await —
/// each window is reduced to wire JSON (`serde_json`, `Send`) plus a sealed local
/// set synchronously (the transient `Filter` from [`Filter::from_json`] drops
/// before the reconcile).
pub async fn pull_reconcile_windowed(
    relay: &mut Relay,
    ndb: &Ndb,
    base_filter_json: &str,
    until_now: u64,
) -> Result<()> {
    let base: serde_json::Value = serde_json::from_str(base_filter_json)?;
    // The caller's own bounds clamp the search; absent an explicit `until`, cover
    // up to `until_now`.
    let base_since = base
        .get("since")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let base_until = base
        .get("until")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(until_now);

    // A LIFO stack of `created_at` windows still to reconcile.
    let mut windows = vec![(base_since, base_until)];
    while let Some((since, until)) = windows.pop() {
        // Reduce this window to wire JSON + a sealed local set *synchronously*, so
        // the transient `Filter` never crosses the reconcile await below.
        let window_json = {
            let mut obj = base.clone();
            // Object-key assignment overrides any inherited since/until — no
            // duplicate fields, unlike copying them onto a FilterBuilder.
            obj["since"] = serde_json::json!(since);
            obj["until"] = serde_json::json!(until);
            obj.to_string()
        };
        let local = local_set(ndb, &Filter::from_json(&window_json)?)?;

        // Collapse the reconcile `Result` (its `Box<dyn Error>` is `!Send`) into a
        // `Send` value in this one statement, before any further await — holding
        // the error across the awaits below would make this future `!Send`. `Ok`
        // carries the ids to pull; `Err(())` flags the recoverable cap refusal; a
        // fatal (non-cap) error propagates here and now, with no await after it.
        let need = match relay.reconcile(&window_json, local).await {
            Ok(diff) => Ok(diff.need),
            Err(e) if e.downcast_ref::<TooManyResults>().is_some() => Err(()),
            Err(e) => return Err(e),
        };
        match need {
            Ok(need) => {
                for chunk in need.chunks(ID_FETCH_CHUNK) {
                    let ids: Vec<String> = chunk.iter().map(hex::encode).collect();
                    relay
                        .sync_into(ndb, &json!({ "ids": ids }).to_string())
                        .await?;
                }
            }
            Err(()) => {
                if until <= since + 1 {
                    // Can't bisect a one-second (or empty) window further; pull it
                    // with a plain REQ. Bounded by the relay's REQ limit, so lossy
                    // above it — but >cap events sharing a single second is
                    // pathological for the envelope kinds this syncs.
                    tracing::warn!(
                        "windowed reconcile: window [{since},{until}] over cap at min width; REQ fallback"
                    );
                    relay.sync_into(ndb, &window_json).await?;
                } else {
                    let mid = since + (until - since) / 2;
                    windows.push((since, mid));
                    windows.push((mid + 1, until));
                }
            }
        }
    }
    Ok(())
}

/// Connect to `relay_url` and reconcile the local cache against it, returning the
/// live relay — or `None` if nothing was reachable, in which case the CLI works
/// offline against the cache. The relay is best-effort: it's how fresh events sync
/// in and edits fan back out to the running app, but the cache is the source of
/// truth the CLI folds from, so an unreachable relay falls back to the cache.
pub async fn connect_and_sync(
    relay_url: &str,
    ndb: &Ndb,
    author: &Pubkey,
    kinds: &[u32],
    filter: &Filter,
    is_addressable: &dyn Fn(u32) -> bool,
) -> Result<Option<Relay>> {
    let relay = match Relay::connect(relay_url).await {
        // The app being closed is the common case, not an error worth warning
        // about — fall back to the local cache quietly.
        Ok(mut relay) => {
            reconcile_sync(&mut relay, ndb, author, kinds, filter, is_addressable).await?;
            Some(relay)
        }
        Err(e) => {
            eprintln!("warning: {e}");
            eprintln!("working offline against the local cache (--relay to point elsewhere)");
            None
        }
    };
    Ok(relay)
}

/// The sealed negentropy set of the cached events matching `filter`, keyed by
/// `(created_at, id)`. This is the local side handed to [`Relay::reconcile`].
pub fn local_set(ndb: &Ndb, filter: &Filter) -> Result<NegentropyStorageVector> {
    let txn = Transaction::new(ndb)?;
    let mut storage = NegentropyStorageVector::new();
    ndb.fold(
        &txn,
        std::slice::from_ref(filter),
        &mut storage,
        |acc, note| {
            // insert only fails on a bad id length, which can't happen for a stored
            // note; ignore the Result to keep the fold infallible.
            let _ = acc.insert(note.created_at(), Id::from_byte_array(*note.id()));
            acc
        },
    )?;
    storage.seal()?;
    Ok(storage)
}

/// The `["EVENT", {...}]` frames for the cached events matching `filter` whose id
/// satisfies `keep` — the events to push so the relay (and app) catch up. `keep`
/// selects which side of a reconcile to forward (the ids we hold that the relay
/// lacks).
///
/// Addressable events (those `is_addressable` accepts) are deduplicated to their
/// latest revision per `(kind, d-tag)` right in the query; immutable events pass
/// through untouched. The local cache is append-only and keeps every old
/// revision, but a relay holds only the latest and rejects the rest as
/// "replaced" — pushing stale revisions is pointless and would keep the reconcile
/// from ever converging (the dropped id can never land, so it re-flushes every
/// run). The winner follows NIP-33 resolution: newest `created_at`, ties broken
/// by the lexically lowest id, matching what the relay and app keep.
pub fn frames_where(
    ndb: &Ndb,
    filter: &Filter,
    is_addressable: &dyn Fn(u32) -> bool,
    keep: impl Fn(&[u8; 32]) -> bool,
) -> Vec<String> {
    let Ok(txn) = Transaction::new(ndb) else {
        return Vec::new();
    };

    // Threaded accumulator: the winning revision per addressable coordinate, plus
    // every immutable event in arrival order.
    type Latest = HashMap<(u32, String), (u64, [u8; 32], String)>;
    let (latest, plain) = ndb
        .fold(
            &txn,
            std::slice::from_ref(filter),
            (Latest::new(), Vec::<([u8; 32], String)>::new()),
            |(mut latest, mut plain), note| {
                let id = *note.id();
                // Build the client `["EVENT", {...}]` frame straight from the
                // note's own JSON. (nostrdb_net's ClientMessage::event takes an
                // owned JSON Note, not a &nostrdb::Note, so we splice here.)
                let Ok(note_json) = note.json() else {
                    return (latest, plain);
                };
                let frame = format!(r#"["EVENT",{note_json}]"#);

                let kind = note.kind();
                if is_addressable(kind)
                    && let Some(d) = d_tag(&note)
                {
                    let at = note.created_at();
                    let win = latest
                        .get(&(kind, d.clone()))
                        .is_none_or(|(t, i, _)| at > *t || (at == *t && id < *i));
                    if win {
                        latest.insert((kind, d), (at, id, frame));
                    }
                } else {
                    plain.push((id, frame));
                }
                (latest, plain)
            },
        )
        .unwrap_or_default();

    latest
        .into_values()
        .map(|(_, id, frame)| (id, frame))
        .chain(plain)
        .filter(|(id, _)| keep(id))
        .map(|(_, frame)| frame)
        .collect()
}

/// The value of a note's `d` tag, if any.
///
/// A `d` whose value is a 64-char hex string — as relation/blockers/related
/// events use, where the `d` is a card id — is stored by nostrdb as a 32-byte
/// id, not a string, so `get_str(1)` returns `None` for it. Fall back to reading
/// the element as an id and hex-encoding it. Without the fallback those
/// coordinates slip past [`frames_where`]'s dedup into the un-deduped `plain`
/// bucket, so every stale revision re-flushes to the relay on every run (the
/// relay keeps only the latest per `d` and rejects the rest as "replaced", so it
/// never converges). Kinds whose `d` isn't hex (board slug, `board:card`
/// placement, `container:card` sequence) are unaffected and keep reading as
/// strings.
fn d_tag(note: &Note) -> Option<String> {
    note.tags().iter().find_map(|tag| {
        if tag.get_str(0) != Some("d") {
            return None;
        }
        tag.get_str(1)
            .map(str::to_owned)
            .or_else(|| tag.get_id(1).map(hex::encode))
    })
}

/// Degraded sync for relays that don't speak NIP-77: `REQ` the whole document in,
/// ingest it, then push any local event the relay didn't return. O(document) on
/// the wire instead of O(difference), but it keeps the CLI working against plain
/// NIP-01 relays (or a notedeck whose relay predates negentropy).
async fn fallback_sync(
    relay: &mut Relay,
    ndb: &Ndb,
    kinds: &[u32],
    author: &Pubkey,
    filter: &Filter,
    is_addressable: &dyn Fn(u32) -> bool,
) -> Result<()> {
    let wire = json!({ "kinds": kinds, "authors": [author.hex()] });
    // sync_into returns only once the received events are queryable.
    let received = relay.sync_into(ndb, &wire.to_string()).await?;

    let on_relay: HashSet<[u8; 32]> = received.into_iter().collect();
    let pending = frames_where(ndb, filter, is_addressable, |id| !on_relay.contains(id));
    if !pending.is_empty() {
        relay.publish(&pending).await?;
        eprintln!("flushed {} local event(s) to the relay", pending.len());
    }
    Ok(())
}

/// Forward an edit's collected frames to the relay if one is connected. With no
/// relay the events are already in the local cache; they simply won't reach the
/// running app until it's reachable, so this is a no-op.
pub async fn publish(relay: &mut Option<Relay>, frames: &[String]) -> Result<()> {
    if let Some(relay) = relay {
        relay.publish(frames).await?;
    }
    Ok(())
}

/// A trailing note for command output, flagging when a change landed only in the
/// local cache because no relay was reachable.
pub fn offline_note(relay: &Option<Relay>) -> &'static str {
    if relay.is_some() {
        ""
    } else {
        " — offline, not forwarded to the app"
    }
}

/// Restore the OS default disposition for `SIGPIPE` at process startup.
///
/// The Rust runtime installs `SIG_IGN` for `SIGPIPE` before `main`, so writing
/// to a closed pipe returns `EPIPE` instead of killing the process — and
/// `println!` turns that `EPIPE` into a panic ("failed printing to stdout:
/// Broken pipe"). CLIs are piped constantly (`headway show | head`), so restore
/// the default `SIG_DFL` here: a closed reader then terminates us quietly via
/// the signal, which is the conventional behavior for a text-emitting tool. Call
/// once at the top of `main`. A no-op on non-unix platforms.
pub fn reset_sigpipe() {
    #[cfg(unix)]
    // SAFETY: `signal(2)` with `SIG_DFL` for `SIGPIPE` just resets a signal
    // disposition to its OS default; it touches no memory we own and is safe to
    // call from a single-threaded startup before any pipe writes happen.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Mute `s` with an ANSI color, but only when stdout is a terminal — so ids
/// read as muted beside titles interactively, while a piped or redirected listing
/// stays plain text for scripts to parse.
///
/// Uses the "bright black" foreground (SGR 90), not the dim attribute (SGR 2):
/// dim is widely unimplemented — urxvt, among others, ignores it and renders
/// the id at full strength — whereas the bright-black color is part of the
/// standard 16-color palette every terminal honors.
pub fn dim(s: &str) -> String {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() {
        format!("\x1b[90m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// stored signing key
// ---------------------------------------------------------------------------

/// Path to a small piece of persisted CLI state named `name`, under
/// `<data-dir>/<app>/` (e.g. `~/.local/share/headway-cli/` on Linux) alongside
/// the cache. Used for the stored signing key (`nsec`) and other set-once bits of
/// state like the current board, so later runs — and the agents driving them —
/// don't have to repeat a flag or env var.
pub fn config_path(app: &str, name: &str) -> Result<std::path::PathBuf> {
    Ok(dirs::data_dir()
        .ok_or("no data dir; set the value via env var or flag instead")?
        .join(app)
        .join(name))
}

/// Read a stored config value (`name`) for `app`, trimmed. A missing, unreadable,
/// or empty file all read as `None` so the caller can fall back to a flag, env
/// var, or default.
pub fn read_config(app: &str, name: &str) -> Option<String> {
    let contents = std::fs::read_to_string(config_path(app, name).ok()?).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Persist a config value (`name`) for `app`, creating the directory if needed.
pub fn write_config(app: &str, name: &str, value: &str) -> Result<()> {
    let path = config_path(app, name)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, format!("{value}\n"))?;
    Ok(())
}

/// Where the signing key lives when stored via `login`: a single `nsec...` line
/// in `<data-dir>/<app>/nsec`. It lets a key be set once so later runs never have
/// to pass `--nsec` or export the env var.
pub fn nsec_config_path(app: &str) -> Result<std::path::PathBuf> {
    config_path(app, "nsec")
}

/// Read the stored signing key for `app`, if any. Missing file, unreadable file,
/// or an empty one all read as "no stored key" — the caller falls back to the env
/// var or `--nsec`.
pub fn stored_nsec(app: &str) -> Option<String> {
    let contents = std::fs::read_to_string(nsec_config_path(app).ok()?).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Validate `nsec` and store it for later runs of `app`. We derive the pubkey
/// first so a malformed key is rejected before it's written, and lock the file to
/// the owner since it holds a secret.
pub fn login(nsec: &str, app: &str) -> Result<()> {
    let (_, pubkey) = parse_nsec(nsec)?;
    let path = nsec_config_path(app)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, format!("{nsec}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!(
        "stored signing key for {} in {}",
        pubkey.hex(),
        path.display()
    );
    Ok(())
}

/// Forget the stored signing key for `app`. Removing a key that isn't there is
/// not an error.
pub fn logout(app: &str) -> Result<()> {
    let path = nsec_config_path(app)?;
    match std::fs::remove_file(&path) {
        Ok(()) => println!("removed stored signing key at {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => println!("no stored signing key"),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Decode an `nsec...` into raw secret bytes and its derived pubkey.
pub fn parse_nsec(nsec: &str) -> Result<([u8; 32], Pubkey)> {
    let (hrp, data) = bech32::decode(nsec).map_err(|_| "invalid nsec (not bech32)")?;
    if hrp.as_str() != "nsec" {
        return Err(format!("expected an nsec, got '{}' key", hrp.as_str()).into());
    }
    let secret: [u8; 32] = data
        .try_into()
        .map_err(|_| "nsec did not decode to 32 bytes")?;
    let keypair = crate::Keypair::from_secret(crate::SecretKey::from_slice(&secret)?);
    Ok((secret, keypair.pubkey))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostrdb::NoteBuilder;
    use tempfile::TempDir;

    const TEST_SECKEY: [u8; 32] = [7u8; 32];
    /// A 64-char hex `d` value — the shape nostrdb stores as a 32-byte id rather
    /// than a string (a card id, as an addressable relation/blockers event uses).
    const HEX_D: &str = "d667280d8ae3ff165baf238a6d531f6547906065e53cbdee6409550a2a5cb11e";

    fn temp_ndb() -> (TempDir, Ndb) {
        let dir = TempDir::new().expect("tmp dir");
        let ndb = Ndb::new(dir.path().to_str().expect("path"), &Config::new()).expect("ndb");
        (dir, ndb)
    }

    /// Ingest a signed addressable note (`kind`, `d`, `created_at`) with distinct
    /// `content` (so each revision gets a distinct id), and return its id.
    fn ingest_addr(ndb: &Ndb, kind: u32, d: &str, created_at: u64, content: &str) -> [u8; 32] {
        let note = NoteBuilder::new()
            .kind(kind)
            .content(content)
            .created_at(created_at)
            .start_tag()
            .tag_str("d")
            .tag_str(d)
            .sign(&TEST_SECKEY)
            .build()
            .expect("build note");
        let id = *note.id();
        let frame = format!(r#"["EVENT",{}]"#, note.json().expect("json"));
        ndb.process_client_event(&frame).expect("ingest");
        // Wait until it is queryable.
        for _ in 0..1000 {
            let txn = Transaction::new(ndb).expect("txn");
            if ndb
                .query(&txn, &[Filter::new().kinds([kind as u64]).build()], 1_000_000)
                .expect("query")
                .iter()
                .any(|n| n.note.id() == &id)
            {
                return id;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("note {kind} never became queryable");
    }

    /// A `d` whose value is a 64-hex card id is stored as an id, not a string, so
    /// the old string-only `d_tag` read it as `None` and every stale revision of
    /// such a coordinate slipped past the dedup and re-flushed forever. The
    /// id-fallback keeps them collapsed to their winning revision, exactly like a
    /// string-keyed coordinate.
    #[test]
    fn frames_where_dedups_id_valued_d_tags() {
        let (_dir, ndb) = temp_ndb();
        let kind = 30621; // relation: d = a 64-hex card id

        // Two revisions of the same coordinate; the newer must win.
        ingest_addr(&ndb, kind, HEX_D, 100, "older");
        let newer = ingest_addr(&ndb, kind, HEX_D, 200, "newer");

        let filter = Filter::new().kinds([kind as u64]).build();
        let frames = frames_where(&ndb, &filter, &|k| (30_000..40_000).contains(&k), |_| true);

        assert_eq!(
            frames.len(),
            1,
            "an id-valued d coordinate must dedup to one winning revision, got {frames:#?}"
        );
        let pushed: serde_json::Value =
            serde_json::from_str(&frames[0]).expect("frame json");
        assert_eq!(
            pushed[1]["id"].as_str().unwrap(),
            hex::encode(newer),
            "the newest revision must be the survivor"
        );
    }

    /// The id fallback must not disturb string-valued `d` tags (board slug,
    /// `board:card` placement, `container:card` sequence): those still read as
    /// strings and dedup as before.
    #[test]
    fn frames_where_still_dedups_string_d_tags() {
        let (_dir, ndb) = temp_ndb();
        let kind = 30620; // placement: d = "board:card" (not pure hex)
        let d = "myboard:abc123";

        ingest_addr(&ndb, kind, d, 100, "older");
        let newer = ingest_addr(&ndb, kind, d, 200, "newer");

        let filter = Filter::new().kinds([kind as u64]).build();
        let frames = frames_where(&ndb, &filter, &|k| (30_000..40_000).contains(&k), |_| true);

        assert_eq!(frames.len(), 1, "string-d coordinate must dedup too");
        let pushed: serde_json::Value =
            serde_json::from_str(&frames[0]).expect("frame json");
        assert_eq!(pushed[1]["id"].as_str().unwrap(), hex::encode(newer));
    }
}
