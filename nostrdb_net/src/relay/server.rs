//! A minimal embedded [NIP-01] nostr relay served over a nostrdb [`Ndb`] handle.
//!
//! This exists so external tooling — CLI utilities for dogfooding Headway — can
//! publish and read nostr events directly against a running app's local
//! nostrdb. It speaks just enough of NIP-01 to be useful:
//!
//! - `["EVENT", {…}]` — ingest the event into ndb, reply with `OK`.
//! - `["REQ", <sub>, <filter>…]` — replay stored matches, send `EOSE`, then
//!   live-stream newly ingested matches until the subscription is closed.
//! - `["CLOSE", <sub>]` — stop a live subscription.
//!
//! It also speaks [NIP-77] negentropy as the responder, so clients can
//! reconcile their set against the relay's in O(difference) rather than
//! re-downloading everything:
//!
//! - `["NEG-OPEN", <sub>, <filter>, <msg-hex>]` — start a reconciliation over a
//!   filter's matches; reply with `NEG-MSG`.
//! - `["NEG-MSG", <sub>, <msg-hex>]` — one reconciliation round; reply with
//!   `NEG-MSG`.
//! - `["NEG-CLOSE", <sub>]` — end a reconciliation.
//!
//! There is deliberately no NIP-11 or NIP-42 auth. Access control is "bind to
//! localhost" — this is a dogfooding port, not a public relay.
//!
//! Rumors are never served. See [`is_servable`].
//!
//! [NIP-01]: https://github.com/nostr-protocol/nips/blob/master/01.md
//! [NIP-77]: https://github.com/nostr-protocol/nips/blob/master/77.md

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::{SinkExt, StreamExt};
use negentropy::{Id, Negentropy, NegentropyStorageVector};
use nostrdb::{Filter, Ndb, Note, SubscriptionStream, Transaction};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A negentropy reconciliation in progress. The storage is owned, so the
/// instance is `'static` and can live in the connection's session map across
/// `NEG-MSG` rounds. Each round reuses the same sealed item set.
type NegSession = Negentropy<'static, NegentropyStorageVector>;

/// How many stored events a single `REQ` replays before `EOSE`.
const STORED_QUERY_LIMIT: i32 = 500;
/// How many freshly-ingested notes we drain per subscription wakeup.
const LIVE_BATCH: u32 = 64;
/// Upper bound on the events a single negentropy reconciliation covers. Unlike a
/// `REQ` (which pages with `EOSE`), reconciliation needs the *whole* matching set
/// at once, so this is set high; a board larger than this would reconcile only a
/// truncated prefix.
const NEG_QUERY_LIMIT: i32 = 100_000;
/// Negentropy frame size limit (`0` = unlimited). Localhost + small boards, so we
/// don't bound per-message size; the protocol still recurses across rounds.
const NEG_FRAME_LIMIT: u64 = 0;

/// Relay-wide knobs and counters shared by every connection task. Cloneable — the
/// `Arc` counter is shared, the scalar cap is copied.
#[derive(Clone)]
struct ServerConfig {
    /// Optional cap on the events a single negentropy reconciliation may match,
    /// modelling strfry's `maxSyncEvents`: a `NEG-OPEN` whose filter matches more
    /// is refused with a `NEG-ERR ... too many query results`, exactly the refusal
    /// the windowed reconcile in [`sync`](super::sync) recovers from. `None`
    /// (the default) never refuses, so the relay reconciles arbitrarily large sets.
    neg_cap: Option<usize>,
    /// Count of `["EVENT", …]` frames received from clients over the wire, across
    /// all connections. Lets a test observe how many events a client *pushed* (a
    /// reconcile that has converged pushes none), distinct from events seeded
    /// straight into ndb, which never traverse the wire.
    events_received: Arc<AtomicUsize>,
}

/// A running relay. Dropping the handle (or calling [`shutdown`](Self::shutdown))
/// stops the accept loop; in-flight connection tasks then wind down on their own.
pub struct RelayHandle {
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    events_received: Arc<AtomicUsize>,
}

impl RelayHandle {
    /// The address the relay actually bound to (useful when binding to port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The `ws://` URL clients should connect to.
    pub fn url(&self) -> String {
        format!("ws://{}", self.local_addr)
    }

    /// Total `["EVENT", …]` frames clients have pushed over the wire since spawn.
    /// A converged reconcile pushes nothing, so this counter staying put across a
    /// second sync is the proof that the sync converged rather than re-flushing.
    pub fn events_received(&self) -> usize {
        self.events_received.load(Ordering::SeqCst)
    }

    /// Signal the accept loop to stop.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind a NIP-01 relay to `addr` and spawn its accept loop on the current Tokio
/// runtime. Returns immediately with a [`RelayHandle`].
///
/// Binds synchronously (so a port conflict surfaces here, not in a detached
/// task) and must be called from within a Tokio runtime context.
pub fn spawn(ndb: Ndb, addr: SocketAddr) -> std::io::Result<RelayHandle> {
    spawn_with_cap(ndb, addr, None)
}

/// Like [`spawn`], but caps a single negentropy reconciliation at `neg_cap`
/// matching events, modelling strfry's `maxSyncEvents`. A `NEG-OPEN` over the cap
/// is refused with `NEG-ERR ... too many query results` — the refusal the windowed
/// reconcile in [`sync`](super::sync) bisects under. `None` never refuses.
pub fn spawn_with_cap(
    ndb: Ndb,
    addr: SocketAddr,
    neg_cap: Option<usize>,
) -> std::io::Result<RelayHandle> {
    let std_listener = std::net::TcpListener::bind(addr)?;
    let local_addr = std_listener.local_addr()?;
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;

    let config = ServerConfig {
        neg_cap,
        events_received: Arc::new(AtomicUsize::new(0)),
    };
    let events_received = config.events_received.clone();
    let (shutdown, shutdown_rx) = watch::channel(false);
    tokio::spawn(accept_loop(listener, ndb, config, shutdown_rx));

    tracing::info!("nostrdb_relay listening on ws://{local_addr}");
    Ok(RelayHandle {
        local_addr,
        shutdown,
        events_received,
    })
}

async fn accept_loop(
    listener: TcpListener,
    ndb: Ndb,
    config: ServerConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { continue };
                let ndb = ndb.clone();
                let config = config.clone();
                let shutdown_rx = shutdown_rx.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_connection(stream, ndb, config, shutdown_rx).await {
                        tracing::debug!("nostrdb_relay connection ended: {err}");
                    }
                });
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
}

async fn serve_connection(
    stream: TcpStream,
    ndb: Ndb,
    config: ServerConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), BoxError> {
    let ws = accept_async(stream).await?;
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Subscription tasks push frames here; the connection drains them to the
    // socket. Keeping the original `out_tx` alive means `recv()` never returns
    // `None` while the connection lives.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    // subscription id -> cancel signal for its live-streaming task.
    let mut subs: HashMap<String, oneshot::Sender<()>> = HashMap::new();
    // subscription id -> in-progress negentropy reconciliation.
    let mut neg_sessions: HashMap<String, NegSession> = HashMap::new();

    loop {
        tokio::select! {
            outgoing = out_rx.recv() => {
                if let Some(msg) = outgoing {
                    ws_tx.send(msg).await?;
                }
            }
            incoming = ws_rx.next() => {
                let Some(msg) = incoming else { break };
                match msg? {
                    Message::Text(text) => {
                        handle_client_frame(
                            &text,
                            &ndb,
                            &config,
                            &out_tx,
                            &mut subs,
                            &mut neg_sessions,
                        );
                    }
                    Message::Ping(payload) => ws_tx.send(Message::Pong(payload)).await?,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    // Dropping the cancel senders stops every live subscription task, which then
    // unsubscribes from ndb.
    subs.clear();
    Ok(())
}

/// Parse and act on one client text frame. Errors are reported to the client as
/// `NOTICE` rather than dropping the connection.
fn handle_client_frame(
    text: &str,
    ndb: &Ndb,
    config: &ServerConfig,
    out_tx: &mpsc::UnboundedSender<Message>,
    subs: &mut HashMap<String, oneshot::Sender<()>>,
    neg_sessions: &mut HashMap<String, NegSession>,
) {
    let Ok(Value::Array(frame)) = serde_json::from_str::<Value>(text) else {
        let _ = out_tx.send(notice("could not parse message"));
        return;
    };

    match frame.first().and_then(Value::as_str) {
        Some("EVENT") => handle_event(text, &frame, ndb, config, out_tx),
        Some("REQ") => handle_req(&frame, ndb, out_tx, subs),
        Some("CLOSE") => {
            if let Some(sub_id) = frame.get(1).and_then(Value::as_str) {
                subs.remove(sub_id);
            }
        }
        Some("NEG-OPEN") => handle_neg_open(&frame, ndb, config, out_tx, neg_sessions),
        Some("NEG-MSG") => handle_neg_msg(&frame, out_tx, neg_sessions),
        Some("NEG-CLOSE") => {
            if let Some(sub_id) = frame.get(1).and_then(Value::as_str) {
                neg_sessions.remove(sub_id);
            }
        }
        _ => {
            let _ = out_tx.send(notice("unrecognized message"));
        }
    }
}

fn handle_event(
    text: &str,
    frame: &[Value],
    ndb: &Ndb,
    config: &ServerConfig,
    out_tx: &mpsc::UnboundedSender<Message>,
) {
    // Count every EVENT frame that crosses the wire, whether or not ndb ends up
    // storing it (a duplicate/replaced push still arrived) — the metric is what
    // the client *sent*, which is what a convergence test asserts on.
    config.events_received.fetch_add(1, Ordering::SeqCst);

    let event_id = frame
        .get(1)
        .and_then(|e| e.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");

    // The client frame is already `["EVENT", {…}]`, exactly what
    // `process_client_event` expects, so we hand it the raw text verbatim.
    match ndb.process_client_event(text) {
        Ok(()) => {
            let _ = out_tx.send(ok(event_id, true, ""));
        }
        Err(err) => {
            let _ = out_tx.send(ok(event_id, false, &format!("error: {err}")));
        }
    }
}

/// Whether a stored note may be handed to a client.
///
/// Rumors may not. A rumor is the decrypted inner event of a gift-wrap or SNS
/// envelope: nostrdb peels it on ingest and stores it alongside ordinary notes,
/// so it matches an ordinary filter, but it is private content that only the
/// receiving account was ever meant to read. Serving one leaks it to anything
/// that can open a `REQ`.
///
/// It also cannot survive the trip. A rumor carries no signature — nostrdb
/// reuses the 64-byte `sig` field to hold the receiver pubkey and giftwrap id —
/// so a client that re-ingests one fails signature validation and drops it.
/// Before this filter that became a permanent stall rather than a visible
/// error: the negentropy reconcile advertised rumor ids, the client asked for
/// them, stored none of them, then waited out its whole ingest timeout for
/// events that could never land — on *every* run.
///
/// Withholding the rumor costs a legitimate peer nothing: it is reproducible
/// from the envelope it arrived in by any client holding the key, and that
/// envelope is an ordinary signed event which is still served normally.
fn is_servable(note: &Note<'_>) -> bool {
    !note.is_rumor()
}

fn handle_req(
    frame: &[Value],
    ndb: &Ndb,
    out_tx: &mpsc::UnboundedSender<Message>,
    subs: &mut HashMap<String, oneshot::Sender<()>>,
) {
    let Some(sub_id) = frame.get(1).and_then(Value::as_str) else {
        let _ = out_tx.send(notice("REQ missing subscription id"));
        return;
    };
    let sub_id = sub_id.to_owned();

    let filters: Vec<Filter> = frame[2..]
        .iter()
        .filter_map(|f| Filter::from_json(&f.to_string()).ok())
        .collect();

    // Stored phase, run synchronously here: everything already in ndb that
    // matches, then EOSE. Doing it before we spawn keeps the non-`Send` `Filter`
    // and `Transaction` off the awaiting task entirely.
    if let Ok(txn) = Transaction::new(ndb)
        && let Ok(results) = ndb.query(&txn, &filters, STORED_QUERY_LIMIT)
    {
        for result in results {
            if !is_servable(&result.note) {
                continue;
            }
            if let Ok(note_json) = result.note.json()
                && out_tx.send(event(&sub_id, &note_json)).is_err()
            {
                return;
            }
        }
    }
    if out_tx.send(eose(&sub_id)).is_err() {
        return;
    }

    // Live phase: a fresh subscription only reports future ingests. Subscribe
    // here (still synchronous) so the spawned task captures only `Send` values.
    let Ok(sub) = ndb.subscribe(&filters) else {
        return;
    };

    // A re-REQ of an existing id replaces the old subscription: inserting drops
    // the previous cancel sender, which stops the old streaming task.
    let (cancel_tx, cancel_rx) = oneshot::channel();
    subs.insert(sub_id.clone(), cancel_tx);

    tokio::spawn(stream_subscription(
        ndb.clone(),
        sub,
        sub_id,
        out_tx.clone(),
        cancel_rx,
    ));
}

/// Live-stream newly ingested matches for one subscription until it's cancelled
/// (CLOSE, re-REQ, or connection drop) or the client's outgoing channel closes.
/// Captures only `Send` values so it can live on a spawned task.
///
/// Holds a single long-lived [`SubscriptionStream`] for the subscription's whole
/// life and polls it in the loop. Dropping the stream on exit unsubscribes from
/// ndb. (The deprecated `wait_for_notes` can't be used here: it builds a stream,
/// awaits one batch, then drops it — unsubscribing after the first note.)
async fn stream_subscription(
    ndb: Ndb,
    sub: nostrdb::Subscription,
    sub_id: String,
    out_tx: mpsc::UnboundedSender<Message>,
    mut cancel_rx: oneshot::Receiver<()>,
) {
    let mut stream = SubscriptionStream::new(ndb.clone(), sub).notes_per_await(LIVE_BATCH);
    loop {
        tokio::select! {
            _ = &mut cancel_rx => break,
            next = stream.next() => {
                let Some(keys) = next else { break };
                let Ok(txn) = Transaction::new(&ndb) else { break };
                for key in keys {
                    if let Ok(note) = ndb.get_note_by_key(&txn, key)
                        && is_servable(&note)
                        && let Ok(note_json) = note.json()
                        && out_tx.send(event(&sub_id, &note_json)).is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
}

/// Start a negentropy reconciliation: `["NEG-OPEN", <sub>, <filter>, <msg-hex>]`.
///
/// We're the responder. Build the reconciliation set from everything in ndb that
/// matches `filter`, run the client's opening message through it, and reply with
/// our `NEG-MSG`. The session is kept so subsequent `NEG-MSG` rounds reuse the
/// same item set. A re-`NEG-OPEN` of a live id replaces the prior session.
fn handle_neg_open(
    frame: &[Value],
    ndb: &Ndb,
    config: &ServerConfig,
    out_tx: &mpsc::UnboundedSender<Message>,
    neg_sessions: &mut HashMap<String, NegSession>,
) {
    let Some(sub_id) = frame.get(1).and_then(Value::as_str) else {
        let _ = out_tx.send(notice("NEG-OPEN missing subscription id"));
        return;
    };

    let Some(filter_json) = frame.get(2).map(Value::to_string) else {
        let _ = out_tx.send(neg_err(sub_id, "NEG-OPEN missing filter"));
        return;
    };
    let Ok(filter) = Filter::from_json(&filter_json) else {
        let _ = out_tx.send(neg_err(sub_id, "NEG-OPEN filter is invalid"));
        return;
    };

    let Some(Ok(query)) = frame.get(3).and_then(Value::as_str).map(hex::decode) else {
        let _ = out_tx.send(neg_err(sub_id, "NEG-OPEN message is not valid hex"));
        return;
    };

    let mut session = match build_neg_session(ndb, filter, config.neg_cap) {
        Ok(Some(session)) => session,
        // Over the per-sync cap: refuse the whole reconciliation, as strfry does
        // for a filter matching more than `maxSyncEvents`. The `too many query
        // results` reason (plus the numeric cap) is what a windowed client
        // downcasts to `TooManyResults` and recovers by bisecting the range.
        Ok(None) => {
            let _ = out_tx.send(neg_err_too_many(sub_id, config.neg_cap));
            return;
        }
        Err(err) => {
            let _ = out_tx.send(neg_err(sub_id, &format!("could not build set: {err}")));
            return;
        }
    };

    match session.reconcile(&query) {
        Ok(reply) => {
            let _ = out_tx.send(neg_msg(sub_id, &reply));
            neg_sessions.insert(sub_id.to_owned(), session);
        }
        Err(err) => {
            let _ = out_tx.send(neg_err(sub_id, &format!("reconcile failed: {err}")));
        }
    }
}

/// One reconciliation round: `["NEG-MSG", <sub>, <msg-hex>]`. Looks up the open
/// session, folds the client's message in, and replies with the next `NEG-MSG`.
fn handle_neg_msg(
    frame: &[Value],
    out_tx: &mpsc::UnboundedSender<Message>,
    neg_sessions: &mut HashMap<String, NegSession>,
) {
    let Some(sub_id) = frame.get(1).and_then(Value::as_str) else {
        let _ = out_tx.send(notice("NEG-MSG missing subscription id"));
        return;
    };
    let Some(session) = neg_sessions.get_mut(sub_id) else {
        let _ = out_tx.send(neg_err(sub_id, "no open negentropy session for this id"));
        return;
    };
    let Some(Ok(query)) = frame.get(2).and_then(Value::as_str).map(hex::decode) else {
        let _ = out_tx.send(neg_err(sub_id, "NEG-MSG message is not valid hex"));
        return;
    };

    match session.reconcile(&query) {
        Ok(reply) => {
            let _ = out_tx.send(neg_msg(sub_id, &reply));
        }
        Err(err) => {
            let _ = out_tx.send(neg_err(sub_id, &format!("reconcile failed: {err}")));
            neg_sessions.remove(sub_id);
        }
    }
}

/// Build a sealed negentropy reconciliation set from every event in ndb matching
/// `filter`, keyed by `(created_at, id)` as the protocol requires.
///
/// Returns `Ok(None)` when the match exceeds `neg_cap` (strfry `maxSyncEvents`):
/// the caller refuses the whole reconciliation rather than reconciling a truncated
/// prefix, mirroring a real relay's per-sync cap. `None` cap never refuses.
fn build_neg_session(
    ndb: &Ndb,
    filter: Filter,
    neg_cap: Option<usize>,
) -> Result<Option<NegSession>, BoxError> {
    let txn = Transaction::new(ndb)?;
    let results = ndb.query(&txn, &[filter], NEG_QUERY_LIMIT)?;

    if let Some(cap) = neg_cap
        && results.len() > cap
    {
        return Ok(None);
    }

    let mut storage = NegentropyStorageVector::with_capacity(results.len());
    for result in results {
        // Must match what a `REQ` will actually serve. An id advertised here but
        // withheld there is one the client asks for and never receives.
        if !is_servable(&result.note) {
            continue;
        }
        storage.insert(
            result.note.created_at(),
            Id::from_byte_array(*result.note.id()),
        )?;
    }
    storage.seal()?;

    Ok(Some(Negentropy::owned(storage, NEG_FRAME_LIMIT)?))
}

fn ok(event_id: &str, status: bool, message: &str) -> Message {
    Message::Text(json!(["OK", event_id, status, message]).to_string())
}

/// `["NEG-MSG", <sub>, <msg-hex>]` — one reconciliation round back to the client.
fn neg_msg(sub_id: &str, msg: &[u8]) -> Message {
    Message::Text(json!(["NEG-MSG", sub_id, hex::encode(msg)]).to_string())
}

/// `["NEG-ERR", <sub>, <reason>]` — abort a reconciliation with a reason.
fn neg_err(sub_id: &str, reason: &str) -> Message {
    Message::Text(json!(["NEG-ERR", sub_id, reason]).to_string())
}

/// The cap refusal: `["NEG-ERR", <sub>, "blocked: too many query results", <cap>]`.
/// The reason string is what a client matches to raise `TooManyResults`, and the
/// 4th element carries the numeric cap it advertises (strfry includes it), which
/// the client surfaces in its warning.
fn neg_err_too_many(sub_id: &str, cap: Option<usize>) -> Message {
    match cap {
        Some(cap) => Message::Text(
            json!(["NEG-ERR", sub_id, "blocked: too many query results", cap]).to_string(),
        ),
        None => neg_err(sub_id, "blocked: too many query results"),
    }
}

fn eose(sub_id: &str) -> Message {
    Message::Text(json!(["EOSE", sub_id]).to_string())
}

fn notice(message: &str) -> Message {
    Message::Text(json!(["NOTICE", message]).to_string())
}

/// `["EVENT", <sub>, <note>]`. The note is already serialized JSON, so we splice
/// it in rather than parse-and-reserialize.
fn event(sub_id: &str, note_json: &str) -> Message {
    Message::Text(format!(r#"["EVENT",{},{}]"#, json!(sub_id), note_json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FullKeypair;
    use crate::sns::{derive_sns_keys, wrap_rumor};
    use nostrdb::{Config, NoteBuilder};

    fn test_root() -> [u8; 32] {
        let mut root = [0u8; 32];
        root[0] = 0x11;
        root[31] = 0x22;
        root
    }

    /// `REQ` the given filter and collect the ids the relay replays before `EOSE`.
    async fn req_ids(url: &str, filter: Value) -> Vec<String> {
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect");
        ws.send(Message::Text(json!(["REQ", "s", filter]).to_string()))
            .await
            .expect("send REQ");
        let mut ids = Vec::new();
        loop {
            let msg = ws.next().await.expect("frame").expect("ok");
            let Message::Text(text) = msg else { continue };
            let frame: Vec<Value> = serde_json::from_str(&text).expect("json");
            match frame.first().and_then(Value::as_str) {
                Some("EVENT") => {
                    let id = frame[2]["id"].as_str().expect("id").to_owned();
                    ids.push(id);
                }
                Some("EOSE") => break,
                _ => {}
            }
        }
        ids
    }

    /// A peeled SNS rumor must never leave the relay.
    ///
    /// It is private content, and it carries no signature (nostrdb reuses `sig`
    /// for the receiver pubkey + giftwrap id), so a client that received one
    /// could not store it anyway — it would fail validation and the client would
    /// wait out its whole ingest timeout for an event that can never land. The
    /// envelope the rumor arrived in is an ordinary signed event and is still
    /// served, so a legitimate peer loses nothing.
    #[tokio::test]
    async fn rumors_are_not_served() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let ndb = Ndb::new(dir.path().to_str().unwrap(), &Config::new()).expect("ndb");

        let member = FullKeypair::generate();
        let keys = derive_sns_keys(&test_root()).expect("keys");
        assert!(ndb.add_team_root(&test_root()));

        // Seal a kind-1 note into an SNS envelope; nostrdb peels it on ingest, so
        // the rumor lands in the db beside the kind-1081 envelope itself.
        let inner = NoteBuilder::new()
            .kind(1)
            .content("private board edit")
            .created_at(1700000000)
            .sign(&member.secret_key.secret_bytes())
            .build()
            .expect("note");
        let inner_json = inner.json().expect("inner json");
        let envelope = wrap_rumor(&keys, &member, &inner_json, 1700000000).expect("envelope");
        let envelope_json = envelope.json().expect("envelope json");
        let envelope_id = hex::encode(envelope.id());

        let sub = ndb
            .subscribe(&[Filter::new().kinds(vec![1]).build()])
            .expect("sub");
        let waiter = ndb.wait_for_all_notes(sub, 1);
        ndb.process_event(&format!("[\"EVENT\",\"e\",{envelope_json}]"))
            .expect("ingest envelope");
        waiter.await.expect("rumor ingested");

        // The rumor really is in the db, and really is flagged as one.
        let rumor_id = {
            let txn = Transaction::new(&ndb).expect("txn");
            let stored = ndb
                .query(&txn, &[Filter::new().kinds(vec![1]).build()], 8)
                .expect("query");
            assert_eq!(stored.len(), 1, "the peeled rumor should be stored");
            assert!(stored[0].note.is_rumor(), "stored note should be a rumor");
            hex::encode(stored[0].note.id())
        };

        let relay = spawn(ndb.clone(), "127.0.0.1:0".parse().expect("addr")).expect("spawn");
        let url = relay.url();

        // The rumor is withheld...
        let served = req_ids(&url, json!({ "kinds": [1] })).await;
        assert!(
            served.is_empty(),
            "relay served a rumor: {served:?} (rumor id {rumor_id})"
        );

        // ...even when asked for by id, which is how the stalling client asked.
        let by_id = req_ids(&url, json!({ "ids": [rumor_id] })).await;
        assert!(by_id.is_empty(), "relay served a rumor asked for by id");

        // ...but the signed envelope it arrived in is still served normally.
        let envelopes = req_ids(&url, json!({ "kinds": [1081] })).await;
        assert_eq!(
            envelopes,
            vec![envelope_id],
            "the SNS envelope should still be served"
        );
    }
}
