//! Minimal async NIP-01 + NIP-77 client for a running notedeck's embedded relay.
//!
//! Shared by the headway and notebook CLIs (and any other tool that keeps its own
//! nostrdb cache and reconciles it against the app's relay):
//! - [`Relay::reconcile`] runs a NIP-77 negentropy session as the initiator to
//!   learn the set difference both ways — which events the relay has that we
//!   don't, and which we have that it doesn't — without transferring the data.
//! - [`Relay::sync_into`] `REQ`s events (by id, after reconcile) and processes
//!   each stored one into a local nostrdb, so the caller can fold locally.
//! - [`Relay::publish`] forwards the `["EVENT", {...}]` frames produced by an
//!   edit (or surfaced by reconcile) back to the relay, so the app sees them.

use std::collections::HashSet;
use std::time::Duration;

use crate::NoteId;
use futures_util::{SinkExt, StreamExt};
use negentropy::{Negentropy, NegentropyStorageVector};
use nostrdb::{Filter, Ndb, Subscription, SubscriptionStream, Transaction};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::Result;

/// The subscription id used for the one-shot sync and reconciliation. Arbitrary —
/// the relay only needs it to be consistent across the session's frames.
const SUB: &str = "sync";

/// How long to wait for nostrdb's background ingester to make freshly-synced
/// events queryable before giving up. Localhost ingest is sub-millisecond; this
/// is only a backstop so a dropped/stuck ingest can't hang the CLI.
const INGEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the websocket handshake in [`Relay::connect`]. The relay
/// is normally localhost and answers in milliseconds; this only bounds a remote
/// or tunnelled endpoint that completes the TCP connect but stalls the upgrade.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the next inbound frame in [`Relay::next_text`]. Every
/// read in the reconcile/sync/publish path flows through `next_text`, so this one
/// bound covers them all: a half-open relay that accepts the connection and the
/// `NEG-OPEN` but never sends a terminating frame would otherwise block forever
/// (a stalled stream never yields `None`, so the EOF guard never fires). The
/// resulting error degrades gracefully via the callers' best-effort handling.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// The set difference a [`Relay::reconcile`] uncovers, as raw event ids.
pub struct Diff {
    /// Ids the relay holds that the local cache is missing — pull these down.
    pub need: Vec<[u8; 32]>,
    /// Ids the local cache holds that the relay is missing — push these up.
    pub have: Vec<[u8; 32]>,
}

/// [`Relay::reconcile`] error: the relay refused the whole reconciliation because
/// the filter matches more events than its per-sync cap (strfry `maxSyncEvents`,
/// surfaced as `NEG-ERR ... "blocked: too many query results"`).
///
/// The relay counts the filter's *total* match, not the set difference, and
/// streams no partial result — so a single reconcile can't sync such a filter.
/// This is distinct from a fatal error precisely because it is *recoverable* by
/// narrowing the filter so each sub-query stays under the cap; see
/// [`pull_reconcile_windowed`](super::pull_reconcile_windowed), which bisects the
/// `created_at` range on exactly this error.
#[derive(Debug)]
pub struct TooManyResults {
    /// The cap the relay advertised in the `NEG-ERR` frame, if present (the 4th
    /// element — e.g. `5000`).
    pub limit: Option<u64>,
}

impl std::fmt::Display for TooManyResults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.limit {
            Some(limit) => write!(
                f,
                "reconcile filter matches more than the relay's cap of {limit}"
            ),
            None => write!(
                f,
                "reconcile filter matches more than the relay's per-sync cap"
            ),
        }
    }
}

impl std::error::Error for TooManyResults {}

/// A *transient* reconcile/sync failure — one a retry can plausibly recover from,
/// as opposed to a permanent refusal. It marks three cases: the relay dropped or
/// stalled the connection mid-session, or it answered a `NEG-OPEN` with a
/// `closed: …` NEG-ERR ("retry later"). A long-lived
/// [`Session`](super::Session)'s backfill loop downcasts to this to decide whether
/// to back off and try again, versus give up on a permanent error (a `blocked: …`
/// NEG-ERR, a relay that doesn't speak NIP-77, a malformed frame). Distinct from
/// [`TooManyResults`], which is recovered not by retrying but by windowing the
/// filter under the relay's per-sync cap.
///
/// The one-shot CLI paths ([`reconcile_sync`](super::reconcile_sync),
/// [`pull_reconcile`](super::pull_reconcile)) don't distinguish it — any reconcile
/// error already routes them to their best-effort NIP-01 fallback — so this only
/// changes the failure's `Display` text there, not its handling.
#[derive(Debug)]
pub struct Transient(pub String);

impl std::fmt::Display for Transient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Transient {}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct Relay {
    ws: Ws,
    url: String,
}

impl Relay {
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(url))
            .await
            .map_err(|_| format!("connecting to {url} timed out"))?
            .map_err(|e| format!("connecting to {url}: {e}"))?;
        Ok(Relay {
            ws,
            url: url.to_string(),
        })
    }

    /// Run a NIP-77 negentropy reconciliation as the initiator over the events
    /// matching `filter_json`. `storage` is the sealed local set `(created_at,
    /// id)`. Returns the [`Diff`]: the ids each side is missing, computed in
    /// O(difference) without transferring the matching events themselves.
    pub async fn reconcile(
        &mut self,
        filter_json: &str,
        storage: NegentropyStorageVector,
    ) -> Result<Diff> {
        // frame_size_limit 0 = unlimited; the data is small and this is
        // localhost, so we don't bound per-message size.
        let mut neg = Negentropy::owned(storage, 0)?;
        let initial = neg.initiate()?;
        self.ws
            .send(Message::Text(format!(
                r#"["NEG-OPEN","{SUB}",{filter_json},"{}"]"#,
                hex::encode(&initial)
            )))
            .await?;

        let mut diff = Diff {
            need: Vec::new(),
            have: Vec::new(),
        };
        loop {
            let text = self.next_text().await?;
            let frame: Vec<Value> = serde_json::from_str(&text)?;
            match frame.first().and_then(Value::as_str) {
                Some("NEG-MSG") => {
                    let msg = frame
                        .get(2)
                        .and_then(Value::as_str)
                        .ok_or("NEG-MSG missing payload")?;
                    let msg = hex::decode(msg).map_err(|e| format!("bad NEG-MSG hex: {e}"))?;

                    // `have`/`need` are from our (the initiator's) perspective:
                    // have = we hold, relay lacks; need = relay holds, we lack.
                    let mut have = Vec::new();
                    let mut need = Vec::new();
                    let reply = neg.reconcile_with_ids(&msg, &mut have, &mut need)?;
                    diff.have.extend(have.iter().map(|id| *id.as_bytes()));
                    diff.need.extend(need.iter().map(|id| *id.as_bytes()));

                    match reply {
                        // More ranges to probe — answer and keep going.
                        Some(reply) => {
                            self.ws
                                .send(Message::Text(format!(
                                    r#"["NEG-MSG","{SUB}","{}"]"#,
                                    hex::encode(&reply)
                                )))
                                .await?;
                        }
                        // Nothing left to ask: reconciliation is complete.
                        None => break,
                    }
                }
                Some("NEG-ERR") => {
                    let reason = frame.get(2).and_then(Value::as_str).unwrap_or("");
                    // "too many query results" (strfry) / "RESULTS_TOO_BIG" (the
                    // NIP-77 spec code) is the recoverable cap refusal — surface it
                    // as a typed error so a windowed caller can narrow and retry,
                    // rather than a fatal opaque string.
                    if reason.contains("too many query results")
                        || reason.contains("RESULTS_TOO_BIG")
                    {
                        let limit = frame.get(3).and_then(Value::as_u64);
                        return Err(Box::new(TooManyResults { limit }));
                    }
                    // A `closed: …` NEG-ERR is the relay asking us to retry later
                    // (e.g. strfry `closed: retry later`) — a transient refusal, as
                    // opposed to a permanent `blocked: …`. Type it so a long-lived
                    // session's backfill can back off and retry rather than give up.
                    if reason.split(':').next().map(str::trim) == Some("closed") {
                        return Err(Box::new(Transient(format!(
                            "relay closed reconciliation: {reason}"
                        ))));
                    }
                    return Err(format!("relay refused reconciliation: {reason}").into());
                }
                // A relay that doesn't speak NIP-77 answers NEG-OPEN with a
                // NOTICE (or nothing). Anything that isn't a reconciliation frame
                // means the handshake won't progress — bail rather than block
                // forever waiting for a NEG-MSG that will never come.
                Some("NOTICE") => {
                    let msg = frame.get(1).and_then(Value::as_str).unwrap_or("");
                    return Err(format!("relay does not support NIP-77 negentropy: {msg}").into());
                }
                other => {
                    return Err(format!(
                        "unexpected '{}' frame during negentropy reconcile",
                        other.unwrap_or("?")
                    )
                    .into());
                }
            }
        }
        self.ws
            .send(Message::Text(format!(r#"["NEG-CLOSE","{SUB}"]"#)))
            .await?;
        Ok(diff)
    }

    /// `REQ` `filter_json`, processing every stored EVENT into `ndb`. Returns the
    /// ids of the events received once they're queryable in `ndb`.
    ///
    /// nostrdb ingests on a background thread, so an event handed to
    /// `process_event` isn't immediately readable. We subscribe to `filter_json`
    /// *before* the `REQ` and, after `EOSE`, await that [`SubscriptionStream`]
    /// until every received id has landed — driving the ingest off a wakeup
    /// rather than polling for it.
    pub async fn sync_into(&mut self, ndb: &Ndb, filter_json: &str) -> Result<Vec<[u8; 32]>> {
        // Subscribe first so notes ingested during this REQ wake the stream we
        // await below. (A subscription only fires for future ingests, so it must
        // exist before we feed events in.) The parsed `Filter` is dropped
        // immediately: a `nostrdb::Filter` is `!Send`, and holding one across the
        // await loop below would make this future `!Send`, blocking callers that
        // drive sync on a multi-thread runtime (e.g. a `tokio::spawn`ed engine).
        let sub = {
            let filter = Filter::from_json(filter_json)?;
            ndb.subscribe(std::slice::from_ref(&filter))?
        };

        self.ws
            .send(Message::Text(format!(r#"["REQ","{SUB}",{filter_json}]"#)))
            .await?;

        let mut received = Vec::new();
        loop {
            let text = self.next_text().await?;
            let frame: Vec<Value> = serde_json::from_str(&text)?;
            match frame.first().and_then(Value::as_str) {
                // Relay form is ["EVENT", <sub>, <note>]; hand nostrdb the whole
                // message verbatim so it ingests the note as received.
                Some("EVENT") => {
                    ndb.process_event(&text)
                        .map_err(|e| format!("ingesting event: {e}"))?;
                    if let Some(id) = frame
                        .get(2)
                        .and_then(|note| note.get("id"))
                        .and_then(Value::as_str)
                        .and_then(|hex| NoteId::from_hex(hex).ok())
                    {
                        received.push(*id.bytes());
                    }
                }
                Some("EOSE") => break,
                Some("CLOSED") => return Err(format!("relay closed subscription: {text}").into()),
                _ => {}
            }
        }
        self.ws
            .send(Message::Text(format!(r#"["CLOSE","{SUB}"]"#)))
            .await?;

        await_ingest(ndb, sub, &received).await;
        Ok(received)
    }

    /// Send each `["EVENT", {...}]` frame and wait for its `OK`, erroring if the
    /// relay genuinely rejects one.
    ///
    /// A `duplicate`/`replaced` rejection is **not** an error: it means the relay
    /// already holds the event (or, for an addressable event, a newer version
    /// with the same `d`-tag). Either way our copy — or something better — is
    /// already there, so the push has nothing to do. This is the common case when
    /// reconcile flushes a cached event whose newer replacement made the relay
    /// drop the old id we still hold.
    pub async fn publish(&mut self, frames: &[String]) -> Result<()> {
        for frame in frames {
            self.ws.send(Message::Text(frame.clone())).await?;
        }
        let mut acked = 0;
        while acked < frames.len() {
            let text = self.next_text().await?;
            let frame: Vec<Value> = serde_json::from_str(&text)?;
            if frame.first().and_then(Value::as_str) == Some("OK") {
                let accepted = frame.get(2).and_then(Value::as_bool).unwrap_or(false);
                if !accepted {
                    let reason = frame.get(3).and_then(Value::as_str).unwrap_or("");
                    if !is_benign_reject(reason) {
                        return Err(format!("relay rejected event: {reason}").into());
                    }
                }
                acked += 1;
            }
        }
        Ok(())
    }

    /// Await the next text frame, skipping pings/binary.
    ///
    /// Each read is bounded by [`RECV_TIMEOUT`] so a half-open relay that stops
    /// sending mid-session can't wedge the reconcile/sync/publish loops (a stalled
    /// stream never yields `None`, so the EOF guard below would never fire).
    async fn next_text(&mut self) -> Result<String> {
        loop {
            // A stall, a clean EOF, and a mid-stream websocket error are all
            // transient connection hiccups (the peer went away, not a permanent
            // refusal) — type them as [`Transient`] so a session backfill retries
            // rather than giving up on the first dropped connection.
            let msg = tokio::time::timeout(RECV_TIMEOUT, self.ws.next())
                .await
                .map_err(|_| Transient(format!("{} stalled waiting for a frame", self.url)))?
                .ok_or_else(|| Transient(format!("{} closed the connection", self.url)))?
                .map_err(|e| Transient(format!("{}: websocket read error: {e}", self.url)))?;
            if let Message::Text(text) = msg {
                return Ok(text);
            }
        }
    }

    /// Send a `NEG-CLOSE` for the reconcile subscription, telling the relay to drop
    /// any negentropy session it is still tracking for us.
    ///
    /// [`reconcile`](Self::reconcile) already emits this on a *successful* pass; this
    /// is the cancel path — a long-lived session whose backfill is torn down
    /// mid-reconcile (an account switch, a dropped subscription) calls it so the
    /// relay doesn't keep a half-open negentropy session around until it times out.
    /// Cheap and best-effort: sending it when no session is open (or to a relay that
    /// already forgot one) is a harmless no-op per NIP-77.
    pub async fn close_negentropy(&mut self) -> Result<()> {
        self.ws
            .send(Message::Text(format!(r#"["NEG-CLOSE","{SUB}"]"#)))
            .await?;
        Ok(())
    }
}

/// Wait until every id in `received` is queryable in `ndb`, driven by `sub`'s
/// [`SubscriptionStream`] rather than a sleep-poll. `sub` must have been created
/// before the events were fed in, so its stream fires as each one is ingested.
///
/// Ids already present when we start (a re-synced duplicate the relay returned
/// but ndb already held) never re-fire the subscription, so we discount them up
/// front; only genuinely-new ids are awaited. Dropping the stream on return
/// unsubscribes.
///
/// The stream is built **first**, ahead of both early returns below. A bare
/// [`Subscription`] is only a handle: nothing releases it but
/// [`SubscriptionStream`]'s `Drop`, so returning before that wrapper exists
/// leaks one for the life of the `Ndb`. The `pending.is_empty()` return is not
/// a rare path either — a relay that reports a difference it then declines to
/// serve hits it on *every* call, because each `REQ` comes back holding only
/// events the caller already has. See
/// `sync_into_releases_its_subscription_when_nothing_is_pending`.
async fn await_ingest(ndb: &Ndb, sub: Subscription, received: &[[u8; 32]]) {
    let mut stream = SubscriptionStream::new(ndb.clone(), sub);

    let mut pending: HashSet<[u8; 32]> = {
        let Ok(txn) = Transaction::new(ndb) else {
            return;
        };
        received
            .iter()
            .filter(|id| ndb.get_note_by_id(&txn, id).is_err())
            .copied()
            .collect()
    };
    if pending.is_empty() {
        return;
    }

    // A backstop deadline: localhost ingest is near-instant, but a stuck ingest
    // mustn't hang the CLI forever.
    let _ = tokio::time::timeout(INGEST_TIMEOUT, async {
        while let Some(keys) = stream.next().await {
            let Ok(txn) = Transaction::new(ndb) else {
                break;
            };
            for key in keys {
                if let Ok(note) = ndb.get_note_by_key(&txn, key) {
                    pending.remove(note.id());
                }
            }
            if pending.is_empty() {
                break;
            }
        }
    })
    .await;
}

/// Whether an `OK: false` reason means the relay already has the event (or a
/// newer replaceable version) rather than truly refusing it. NIP-01 machine
/// reasons are `<prefix>: <message>`; strfry replies `duplicate: ...` for an
/// event it already stored and `replaced: have newer event` for an addressable
/// event superseded by a newer one. Both leave the relay's state at-or-ahead of
/// what we pushed, so a best-effort sync should treat them as delivered.
fn is_benign_reject(reason: &str) -> bool {
    let prefix = reason.split(':').next().unwrap_or("").trim();
    matches!(prefix, "duplicate" | "replaced")
}

#[cfg(test)]
mod tests {
    use super::{CONNECT_TIMEOUT, NegentropyStorageVector, RECV_TIMEOUT, Relay, is_benign_reject};
    use futures_util::{SinkExt, StreamExt};
    use nostrdb::{Config, Ndb, NoteBuilder, Transaction};
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::tungstenite::Message;

    /// An empty, sealed negentropy set — enough to drive [`Relay::reconcile`]'s
    /// `NEG-OPEN` so the test can exercise the read that stalls afterwards.
    fn empty_storage() -> NegentropyStorageVector {
        let mut storage = NegentropyStorageVector::new();
        storage.seal().expect("seal empty storage");
        storage
    }

    /// Bind an ephemeral listener and accept connections, holding each open
    /// **without ever speaking** — the socket stays alive (dropping it would send
    /// EOF), so a client's handshake or read stalls until its own timeout fires.
    /// `on_accept` runs per connection: raw-silent, or upgrade-then-silent.
    async fn spawn_silent<F, Fut>(on_accept: F) -> String
    where
        F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(on_accept(stream));
            }
        });
        format!("ws://{addr}")
    }

    /// A never-completing hold that keeps the socket open and silent for the life
    /// of the test.
    async fn hold_forever<T>(_socket: T) {
        std::future::pending::<()>().await;
    }

    /// `connect_async`'s upgrade must be bounded: an endpoint that accepts the TCP
    /// connection but never completes the websocket handshake yields a connect
    /// `Err` before the backstop, rather than hanging forever.
    #[tokio::test]
    async fn connect_times_out_on_silent_endpoint() {
        let url = spawn_silent(hold_forever).await;
        // Longer than CONNECT_TIMEOUT: a bounded connect returns Err before this
        // fires; an unbounded one (a regression) trips it and fails the test.
        let backstop = CONNECT_TIMEOUT + Duration::from_secs(5);
        let result = tokio::time::timeout(backstop, Relay::connect(&url))
            .await
            .expect("connect should time out, not hang");
        assert!(result.is_err(), "silent endpoint must yield a connect Err");
    }

    /// `next_text`'s read must be bounded: a relay that completes the websocket
    /// upgrade and then goes silent (never answering the `NEG-OPEN`) makes
    /// [`Relay::reconcile`] error before the backstop rather than block forever.
    #[tokio::test]
    async fn reconcile_times_out_on_silent_relay() {
        let url = spawn_silent(|stream| async move {
            if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                hold_forever(ws).await;
            }
        })
        .await;

        let mut relay = Relay::connect(&url).await.expect("websocket upgrade");
        // Longer than RECV_TIMEOUT for the same reason as the connect backstop.
        let backstop = RECV_TIMEOUT + Duration::from_secs(5);
        let result = tokio::time::timeout(
            backstop,
            relay.reconcile(r#"{"kinds":[1]}"#, empty_storage()),
        )
        .await
        .expect("reconcile should time out, not hang");
        assert!(result.is_err(), "silent relay must yield a reconcile Err");
    }

    /// A relay that answers every `REQ` with `frames` and then `EOSE`. Enough
    /// wire for [`Relay::sync_into`]; it ignores the filter it is given, which
    /// is both simpler and what a relay is free to do.
    async fn spawn_req_relay(frames: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let frames = frames.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(msg)) = ws.next().await {
                        let Ok(text) = msg.into_text() else { continue };
                        if !text.starts_with(r#"["REQ""#) {
                            continue;
                        }
                        for frame in &frames {
                            if ws.send(Message::Text(frame.clone())).await.is_err() {
                                return;
                            }
                        }
                        let _ = ws.send(Message::Text(r#"["EOSE","sync"]"#.into())).await;
                    }
                });
            }
        });
        format!("ws://{addr}")
    }

    /// Block until `id` is queryable, without opening a subscription — this test
    /// counts subscriptions, so its own setup must not create one.
    fn await_stored(ndb: &Ndb, id: &[u8; 32]) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            {
                let txn = Transaction::new(ndb).expect("txn");
                if ndb.get_note_by_id(&txn, id).is_ok() {
                    return;
                }
            }
            assert!(std::time::Instant::now() < deadline, "note never landed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// **`sync_into` must release its subscription however it returns.**
    ///
    /// `await_ingest` subscribes before the `REQ` so freshly-ingested notes wake
    /// it, and a bare [`nostrdb::Subscription`] is released by nothing but
    /// [`nostrdb::SubscriptionStream`]'s `Drop`. Both of its early returns —
    /// nothing received, and everything received already stored — used to fire
    /// before that wrapper existed, leaking one subscription per call for the
    /// life of the `Ndb`.
    ///
    /// Neither is a corner. An empty `REQ` answer is the ordinary "nothing new"
    /// case, and the all-already-stored one is what a relay produces whenever it
    /// reports a difference it then declines to serve — so a caller reconciling
    /// on a schedule leaks on every pass, at a rate the relay chooses.
    ///
    /// Asserted as a delta rather than an absolute so the setup above is not
    /// part of the claim.
    #[tokio::test]
    async fn sync_into_releases_its_subscription_when_nothing_is_pending() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let ndb = Ndb::new(dir.path().to_str().expect("utf-8"), &Config::new()).expect("ndb");

        let note = NoteBuilder::new()
            .kind(1)
            .content("the relay already gave us this")
            .created_at(42)
            .sign(&[0x01; 32])
            .build()
            .expect("note builds");
        let id = *note.id();
        let frame = format!(r#"["EVENT","sync",{}]"#, note.json().expect("note json"));

        // Land it up front so the relay's answer below is all-already-stored.
        ndb.process_event(&frame).expect("process");
        await_stored(&ndb, &id);

        let filter_json = r#"{"kinds":[1]}"#;
        let baseline = ndb.subscription_count();

        // 1. The relay answers with nothing at all.
        let url = spawn_req_relay(Vec::new()).await;
        let mut relay = Relay::connect(&url).await.expect("connect");
        let received = relay.sync_into(&ndb, filter_json).await.expect("sync");
        assert!(received.is_empty(), "the relay sent no events");
        assert_eq!(
            ndb.subscription_count(),
            baseline,
            "an empty REQ answer leaked a subscription"
        );

        // 2. The relay answers only with what we already hold.
        let url = spawn_req_relay(vec![frame]).await;
        let mut relay = Relay::connect(&url).await.expect("connect");
        let received = relay.sync_into(&ndb, filter_json).await.expect("sync");
        assert_eq!(received.len(), 1, "the relay sent the stored event back");
        assert_eq!(
            ndb.subscription_count(),
            baseline,
            "an all-already-stored REQ answer leaked a subscription"
        );
    }

    #[test]
    fn classifies_ok_reasons() {
        // The relay already has this (or a newer replaceable version): nothing to do.
        assert!(is_benign_reject("replaced: have newer event"));
        assert!(is_benign_reject("duplicate: already have this event"));
        // A real refusal must still surface as an error.
        assert!(!is_benign_reject("blocked: pubkey not allowed"));
        assert!(!is_benign_reject("invalid: bad signature"));
        assert!(!is_benign_reject("rate-limited: slow down"));
        assert!(!is_benign_reject(""));
    }
}
