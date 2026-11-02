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
//! By default there is no NIP-11 or NIP-42 auth. Access control is "bind to
//! localhost" — the plain [`spawn`] is a dogfooding port, not a public relay.
//! [`spawn_with_policy`] locks it down for the internet instead: NIP-42 AUTH on
//! connect, a members allowlist, a kinds allowlist and per-client limits. See
//! [`AccessPolicy`].
//!
//! Rumors are never served. See [`is_servable`].
//!
//! [NIP-01]: https://github.com/nostr-protocol/nips/blob/master/01.md
//! [NIP-77]: https://github.com/nostr-protocol/nips/blob/master/77.md

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ndb_filter_from_wire;
use crate::relay::auth;
use futures_util::{SinkExt, StreamExt};
use negentropy::{Id, Negentropy, NegentropyStorageVector};
use nostr::JsonUtil;
use nostrdb::{Filter, Ndb, Note, SubscriptionStream, Transaction};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{accept_async_with_config, accept_hdr_async_with_config};

mod policy;

pub use policy::AccessPolicy;
use policy::{ConnAuth, ConnSlot, ConnTracker};

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
    /// Optional cap on an inbound websocket message's size, modelling strfry's
    /// `max_message_length` (131072 on a default install). A client frame over it
    /// fails the read and drops the connection — which the client then sees as a
    /// broken pipe on its next write — exactly as strfry behaves. `None` (the
    /// default) keeps tungstenite's default limit.
    max_message_len: Option<usize>,
    /// Count of `["EVENT", …]` frames received from clients over the wire, across
    /// all connections. Lets a test observe how many events a client *pushed* (a
    /// reconcile that has converged pushes none), distinct from events seeded
    /// straight into ndb, which never traverse the wire.
    events_received: Arc<AtomicUsize>,
    /// Bumped by [`RelayHandle::drop_connections`]; each connection task watches
    /// it and hangs up when it changes, while the accept loop keeps running.
    kick: watch::Receiver<u64>,
    /// The access policy, when the relay is locked down; `None` is the open
    /// localhost port.
    policy: Option<Arc<AccessPolicy>>,
    /// Open connections per client address, counted only under a policy.
    conns: Arc<ConnTracker>,
}

impl ServerConfig {
    fn policy(&self) -> Option<&AccessPolicy> {
        self.policy.as_deref()
    }

    fn stored_query_limit(&self) -> i32 {
        self.policy()
            .map_or(STORED_QUERY_LIMIT, |p| p.stored_query_limit)
    }

    fn neg_frame_limit(&self) -> u64 {
        self.policy().map_or(NEG_FRAME_LIMIT, |p| p.neg_frame_limit)
    }

    /// Whether a stored note may be handed to a client: never a rumor (see
    /// [`is_servable`]), and under a policy only an allowed kind.
    fn servable(&self, note: &Note<'_>) -> bool {
        is_servable(note) && self.policy().is_none_or(|p| p.kind_allowed(note.kind()))
    }
}

/// A connection's outgoing frames. Subscription tasks push here and the
/// connection drains them to the socket. Under a policy the queue is bounded: a
/// push past [`AccessPolicy::outbound_queue`] fails and signals `overflow`, and
/// the connection hangs up rather than buffer without end for a client that
/// isn't reading.
#[derive(Clone)]
struct Outbox {
    tx: mpsc::UnboundedSender<Message>,
    depth: Arc<AtomicUsize>,
    cap: usize,
    overflow: Arc<Notify>,
}

impl Outbox {
    fn new(cap: Option<usize>) -> (Self, mpsc::UnboundedReceiver<Message>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let outbox = Self {
            tx,
            depth: Arc::new(AtomicUsize::new(0)),
            cap: cap.unwrap_or(usize::MAX),
            overflow: Arc::new(Notify::new()),
        };
        (outbox, rx)
    }

    fn send(&self, msg: Message) -> Result<(), ()> {
        if self.depth.fetch_add(1, Ordering::SeqCst) >= self.cap {
            self.depth.fetch_sub(1, Ordering::SeqCst);
            self.overflow.notify_one();
            return Err(());
        }
        self.tx.send(msg).map_err(|_| {
            self.depth.fetch_sub(1, Ordering::SeqCst);
        })
    }

    /// One frame left the queue for the socket.
    fn drained(&self) {
        self.depth.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A running relay. Dropping the handle (or calling [`shutdown`](Self::shutdown))
/// stops the accept loop; in-flight connection tasks then wind down on their own.
pub struct RelayHandle {
    local_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    events_received: Arc<AtomicUsize>,
    kick: watch::Sender<u64>,
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

    /// Hang up every connected client without stopping the relay, so clients see
    /// their connection drop and can reconnect to the same address — a relay blip,
    /// or a laptop waking from sleep. Connections accepted afterwards are
    /// unaffected.
    pub fn drop_connections(&self) {
        self.kick.send_modify(|generation| *generation += 1);
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
    spawn_with_limits(ndb, addr, neg_cap, None)
}

/// Like [`spawn_with_cap`], but also caps an inbound websocket message at
/// `max_message_len` bytes, modelling strfry's `max_message_length`: a larger
/// client frame drops the connection. Lets a test prove a client keeps every
/// frame it sends under a real relay's limit. `None` keeps tungstenite's default.
pub fn spawn_with_limits(
    ndb: Ndb,
    addr: SocketAddr,
    neg_cap: Option<usize>,
    max_message_len: Option<usize>,
) -> std::io::Result<RelayHandle> {
    spawn_inner(ndb, addr, neg_cap, max_message_len, None)
}

/// Like [`spawn`], but locked down by `policy`: NIP-42 AUTH on connect, members
/// and kinds allowlists, and per-client limits, for a relay exposed to the
/// internet. See [`AccessPolicy`] for exactly what it refuses and how.
pub fn spawn_with_policy(
    ndb: Ndb,
    addr: SocketAddr,
    policy: AccessPolicy,
) -> std::io::Result<RelayHandle> {
    let neg_cap = Some(policy.max_sync_events);
    let max_message_len = Some(policy.max_message_len);
    spawn_inner(ndb, addr, neg_cap, max_message_len, Some(Arc::new(policy)))
}

fn spawn_inner(
    ndb: Ndb,
    addr: SocketAddr,
    neg_cap: Option<usize>,
    max_message_len: Option<usize>,
    policy: Option<Arc<AccessPolicy>>,
) -> std::io::Result<RelayHandle> {
    let std_listener = std::net::TcpListener::bind(addr)?;
    let local_addr = std_listener.local_addr()?;
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;

    let (kick, kick_rx) = watch::channel(0u64);
    let config = ServerConfig {
        neg_cap,
        max_message_len,
        events_received: Arc::new(AtomicUsize::new(0)),
        kick: kick_rx,
        policy,
        conns: Arc::new(ConnTracker::default()),
    };
    let events_received = config.events_received.clone();
    let (shutdown, shutdown_rx) = watch::channel(false);
    tokio::spawn(accept_loop(listener, ndb, config, shutdown_rx));

    tracing::info!("nostrdb_relay listening on ws://{local_addr}");
    Ok(RelayHandle {
        local_addr,
        shutdown,
        events_received,
        kick,
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
                let Ok((stream, peer)) = accepted else { continue };
                let ndb = ndb.clone();
                let config = config.clone();
                let shutdown_rx = shutdown_rx.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve_connection(stream, peer, ndb, config, shutdown_rx).await {
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
    peer: SocketAddr,
    ndb: Ndb,
    config: ServerConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), BoxError> {
    let mut ws_config = WebSocketConfig::default();
    if let Some(max) = config.max_message_len {
        ws_config.max_message_size = Some(max);
        ws_config.max_frame_size = Some(max);
    }
    let (ws, _slot) = match config.policy() {
        None => (
            accept_async_with_config(stream, Some(ws_config)).await?,
            None,
        ),
        Some(policy) => {
            // The handshake is on the AUTH clock too, so a client that opens a
            // socket and never upgrades it doesn't hold a slot forever.
            let mut slot = None;
            let admit = admission(&config, policy, peer, &mut slot);
            let handshake = accept_hdr_async_with_config(stream, admit, Some(ws_config));
            let ws = tokio::time::timeout(policy.auth_timeout, handshake).await??;
            (ws, slot)
        }
    };
    // Only a kick *after* this connection was accepted hangs it up.
    let mut kick_rx = config.kick.clone();
    kick_rx.borrow_and_update();
    let (mut ws_tx, mut ws_rx) = ws.split();

    // Keeping the original `out_tx` alive means `recv()` never returns `None`
    // while the connection lives.
    let (out_tx, mut out_rx) = Outbox::new(config.policy().map(|p| p.outbound_queue));
    let mut conn = Conn {
        subs: HashMap::new(),
        neg_sessions: HashMap::new(),
        auth: config.policy().map(|_| ConnAuth::new()),
    };

    // Under a policy: challenge on connect, and hang up unless a member has
    // authenticated before the deadline.
    let auth_deadline = tokio::time::sleep(
        config
            .policy()
            .map_or(std::time::Duration::MAX, |p| p.auth_timeout),
    );
    tokio::pin!(auth_deadline);
    if let Some(auth) = &conn.auth {
        ws_tx
            .send(Message::Text(auth::challenge_message(auth.challenge())))
            .await?;
    }

    loop {
        let awaiting_member = conn.auth.as_ref().is_some_and(|a| !a.is_member());
        tokio::select! {
            // Drain replies before reading the next request, so one client's
            // queue only grows by what a single frame produces.
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = kick_rx.changed() => break,
            _ = out_tx.overflow.notified() => {
                tracing::debug!("nostrdb_relay: hanging up {peer}, outbound queue full");
                break;
            }
            _ = &mut auth_deadline, if awaiting_member => {
                let _ = ws_tx
                    .send(notice("auth-required: no member authenticated in time"))
                    .await;
                break;
            }
            outgoing = out_rx.recv() => {
                if let Some(msg) = outgoing {
                    out_tx.drained();
                    ws_tx.send(msg).await?;
                }
            }
            incoming = ws_rx.next() => {
                let Some(msg) = incoming else { break };
                match msg? {
                    Message::Text(text) => {
                        if awaiting_member
                            && let Some(policy) = config.policy()
                            && text.len() > policy.pre_auth_max_message_len
                        {
                            let _ = ws_tx
                                .send(notice("auth-required: message too large before AUTH"))
                                .await;
                            break;
                        }
                        handle_client_frame(&text, &ndb, &config, &out_tx, &mut conn);
                    }
                    Message::Ping(payload) => ws_tx.send(Message::Pong(payload)).await?,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }
    // Dropping the cancel senders stops every live subscription task, which then
    // unsubscribes from ndb.
    conn.subs.clear();
    Ok(())
}

/// The websocket handshake callback under a policy: work out the client's
/// address (the policy's client-IP header when present, else the socket peer)
/// and take a connection slot for it, refusing the upgrade with `429` when it,
/// or the relay, is at its cap.
// The error type is tungstenite's handshake callback signature, not ours.
#[allow(clippy::result_large_err)]
fn admission<'a>(
    config: &'a ServerConfig,
    policy: &'a AccessPolicy,
    peer: SocketAddr,
    slot: &'a mut Option<ConnSlot>,
) -> impl FnOnce(&Request, Response) -> Result<Response, ErrorResponse> + Unpin + 'a {
    move |request: &Request, response: Response| {
        let ip = client_ip(request, policy).unwrap_or(peer.ip());
        match config.conns.admit(ip, policy) {
            Some(admitted) => {
                *slot = Some(admitted);
                Ok(response)
            }
            None => {
                tracing::debug!("nostrdb_relay: refusing {ip}, too many connections");
                let mut refusal = ErrorResponse::new(Some("too many connections".to_owned()));
                *refusal.status_mut() = StatusCode::TOO_MANY_REQUESTS;
                Err(refusal)
            }
        }
    }
}

/// The client address named by the policy's client-IP header, if it is set and
/// the request carries a parseable one.
fn client_ip(request: &Request, policy: &AccessPolicy) -> Option<IpAddr> {
    let header = policy.client_ip_header.as_deref()?;
    request
        .headers()
        .get(header)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// One connection's protocol state.
struct Conn {
    /// subscription id -> cancel signal for its live-streaming task.
    subs: HashMap<String, oneshot::Sender<()>>,
    /// subscription id -> in-progress negentropy reconciliation.
    neg_sessions: HashMap<String, NegSession>,
    /// NIP-42 state, under a policy.
    auth: Option<ConnAuth>,
}

impl Conn {
    /// `Ok` if this connection may read and write: always without a policy, and
    /// with one once a member has authenticated. Otherwise the refusal reason.
    fn gate(&self) -> Result<(), String> {
        self.auth.as_ref().map_or(Ok(()), ConnAuth::gate)
    }

    /// Whether `policy` leaves room for one more subscription or negentropy
    /// session, not counting one that `sub_id` would replace.
    fn has_room(&self, policy: Option<&AccessPolicy>, sub_id: &str) -> bool {
        let Some(policy) = policy else { return true };
        let replacing = self.subs.contains_key(sub_id) || self.neg_sessions.contains_key(sub_id);
        replacing || self.subs.len() + self.neg_sessions.len() < policy.max_subscriptions
    }
}

/// Parse and act on one client text frame. Errors are reported to the client as
/// `NOTICE` rather than dropping the connection.
fn handle_client_frame(
    text: &str,
    ndb: &Ndb,
    config: &ServerConfig,
    out_tx: &Outbox,
    conn: &mut Conn,
) {
    let Ok(Value::Array(frame)) = serde_json::from_str::<Value>(text) else {
        let _ = out_tx.send(notice("could not parse message"));
        return;
    };

    match frame.first().and_then(Value::as_str) {
        Some("EVENT") => handle_event(text, &frame, ndb, config, out_tx, conn),
        Some("REQ") => handle_req(&frame, ndb, config, out_tx, conn),
        Some("CLOSE") => {
            if let Some(sub_id) = frame.get(1).and_then(Value::as_str) {
                conn.subs.remove(sub_id);
            }
        }
        Some("NEG-OPEN") => handle_neg_open(&frame, ndb, config, out_tx, conn),
        Some("NEG-MSG") => handle_neg_msg(&frame, out_tx, conn),
        Some("NEG-CLOSE") => {
            if let Some(sub_id) = frame.get(1).and_then(Value::as_str) {
                conn.neg_sessions.remove(sub_id);
            }
        }
        Some("AUTH") if conn.auth.is_some() => handle_auth(&frame, config, out_tx, conn),
        Some("COUNT") if conn.auth.is_some() => handle_count(&frame, out_tx, conn),
        _ => {
            let _ = out_tx.send(notice("unrecognized message"));
        }
    }
}

/// `["AUTH", <event>]` under a policy: check the 22242 against this
/// connection's challenge and the relay's service URL, and say so with `OK`.
/// A valid AUTH from a non-member succeeds — it proves who the client is — and
/// everything it then asks for is refused `restricted:`.
fn handle_auth(frame: &[Value], config: &ServerConfig, out_tx: &Outbox, conn: &mut Conn) {
    let (Some(policy), Some(auth)) = (config.policy(), conn.auth.as_mut()) else {
        return;
    };
    let Some(event) = frame.get(1).filter(|e| e.is_object()) else {
        let _ = out_tx.send(notice("invalid: AUTH missing event"));
        return;
    };
    let event_id = event.get("id").and_then(Value::as_str).unwrap_or("");
    let reply = match auth.authenticate(policy, &event.to_string(), unix_now()) {
        Ok(()) => ok(event_id, true, ""),
        Err(reason) => ok(event_id, false, &reason),
    };
    let _ = out_tx.send(reply);
}

/// `["COUNT", <sub>, <filter>…]` under a policy. Not implemented, but refused
/// for the same reason as a `REQ` first, so an unauthenticated client learns
/// to AUTH rather than that COUNT is missing.
fn handle_count(frame: &[Value], out_tx: &Outbox, conn: &Conn) {
    let Some(sub_id) = frame.get(1).and_then(Value::as_str) else {
        let _ = out_tx.send(notice("COUNT missing subscription id"));
        return;
    };
    let reason = match conn.gate() {
        Err(reason) => reason,
        Ok(()) => "error: COUNT is not supported".to_owned(),
    };
    let _ = out_tx.send(closed(sub_id, &reason));
}

fn handle_event(
    text: &str,
    frame: &[Value],
    ndb: &Ndb,
    config: &ServerConfig,
    out_tx: &Outbox,
    conn: &Conn,
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

    if let Some(policy) = config.policy() {
        match conn.gate() {
            Ok(()) => handle_member_event(text, frame, event_id, ndb, policy, out_tx),
            Err(reason) => {
                let _ = out_tx.send(ok(event_id, false, &reason));
            }
        }
        return;
    }

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

/// A member's `EVENT` under a policy. Refuse a kind the policy doesn't carry,
/// verify the id and signature, and recognise a duplicate, all before answering;
/// then ingest and answer `OK true` only once the event is stored, so `OK` means
/// what it says rather than "queued".
fn handle_member_event(
    text: &str,
    frame: &[Value],
    event_id: &str,
    ndb: &Ndb,
    policy: &AccessPolicy,
    out_tx: &Outbox,
) {
    let refuse = |reason: &str| {
        let _ = out_tx.send(ok(event_id, false, reason));
    };
    let Some(event) = frame.get(1).filter(|e| e.is_object()) else {
        return refuse("invalid: EVENT missing event");
    };
    match event.get("kind").and_then(Value::as_u64) {
        Some(kind) if u32::try_from(kind).is_ok_and(|k| policy.kind_allowed(k)) => {}
        Some(kind) => return refuse(&format!("blocked: kind {kind} is not accepted here")),
        None => return refuse("invalid: event has no kind"),
    }
    let Ok(event) = nostr::Event::from_json(event.to_string()) else {
        return refuse("invalid: not a valid nostr event");
    };
    if event.verify().is_err() {
        return refuse("invalid: event id or signature does not verify");
    }

    let id = event.id.to_bytes();
    let event_id = event.id.to_hex();
    if is_stored(ndb, &id) {
        let _ = out_tx.send(ok(&event_id, true, "duplicate: already have this event"));
        return;
    }

    // Watch for the event before ingesting it, so its landing can't slip past.
    let Ok(sub) = ndb.subscribe(&[Filter::new().ids([&id]).build()]) else {
        return refuse("error: could not watch for the event");
    };
    let mut landed = SubscriptionStream::new(ndb.clone(), sub).notes_per_await(1);
    if let Err(err) = ndb.process_client_event(text) {
        return refuse(&format!("error: {err}"));
    }

    let ndb = ndb.clone();
    let out_tx = out_tx.clone();
    let store_timeout = policy.store_timeout;
    tokio::spawn(async move {
        let reply = match tokio::time::timeout(store_timeout, landed.next()).await {
            Ok(Some(_)) => ok(&event_id, true, ""),
            // Another connection stored the same event first, so this ingest
            // was dropped as a duplicate and the watch never fired.
            _ if is_stored(&ndb, &id) => ok(&event_id, true, "duplicate: already have this event"),
            _ => ok(&event_id, false, "error: the event was not stored"),
        };
        let _ = out_tx.send(reply);
    });
}

/// Whether ndb already holds the event with this id.
fn is_stored(ndb: &Ndb, id: &[u8; 32]) -> bool {
    Transaction::new(ndb).is_ok_and(|txn| ndb.get_notekey_by_id(&txn, id).is_ok())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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

fn handle_req(frame: &[Value], ndb: &Ndb, config: &ServerConfig, out_tx: &Outbox, conn: &mut Conn) {
    let Some(sub_id) = frame.get(1).and_then(Value::as_str) else {
        let _ = out_tx.send(notice("REQ missing subscription id"));
        return;
    };
    let sub_id = sub_id.to_owned();

    if let Err(reason) = conn.gate() {
        let _ = out_tx.send(closed(&sub_id, &reason));
        return;
    }
    if let Some(policy) = config.policy()
        && frame.len() - 2 > policy.max_filters
    {
        let _ = out_tx.send(closed(&sub_id, "blocked: too many filters"));
        return;
    }
    if !conn.has_room(config.policy(), &sub_id) {
        let _ = out_tx.send(closed(&sub_id, "blocked: too many open subscriptions"));
        return;
    }

    let filters: Vec<Filter> = frame[2..]
        .iter()
        // `ndb_filter_from_wire`, not `Filter::from_json`: answer `until`
        // inclusively, as NIP-01 and strfry do, not with nostrdb's exclusive bound.
        .filter_map(|f| ndb_filter_from_wire(&f.to_string()).ok())
        .collect();

    // Stored phase, run synchronously here: everything already in ndb that
    // matches, then EOSE. Doing it before we spawn keeps the non-`Send` `Filter`
    // and `Transaction` off the awaiting task entirely.
    if let Ok(txn) = Transaction::new(ndb)
        && let Ok(results) = ndb.query(&txn, &filters, config.stored_query_limit())
    {
        for result in results {
            if !config.servable(&result.note) {
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
    conn.subs.insert(sub_id.clone(), cancel_tx);

    tokio::spawn(stream_subscription(
        ndb.clone(),
        config.clone(),
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
    config: ServerConfig,
    sub: nostrdb::Subscription,
    sub_id: String,
    out_tx: Outbox,
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
                        && config.servable(&note)
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
    out_tx: &Outbox,
    conn: &mut Conn,
) {
    let Some(sub_id) = frame.get(1).and_then(Value::as_str) else {
        let _ = out_tx.send(notice("NEG-OPEN missing subscription id"));
        return;
    };
    if let Err(reason) = conn.gate() {
        let _ = out_tx.send(neg_err(sub_id, &reason));
        return;
    }
    if !conn.has_room(config.policy(), sub_id) {
        let _ = out_tx.send(neg_err(sub_id, "blocked: too many open subscriptions"));
        return;
    }

    let Some(filter_json) = frame.get(2).map(Value::to_string) else {
        let _ = out_tx.send(neg_err(sub_id, "NEG-OPEN missing filter"));
        return;
    };
    // Inclusive `until`, like the REQ path (see `ndb_filter_from_wire`).
    let Ok(filter) = ndb_filter_from_wire(&filter_json) else {
        let _ = out_tx.send(neg_err(sub_id, "NEG-OPEN filter is invalid"));
        return;
    };

    let Some(Ok(query)) = frame.get(3).and_then(Value::as_str).map(hex::decode) else {
        let _ = out_tx.send(neg_err(sub_id, "NEG-OPEN message is not valid hex"));
        return;
    };

    let mut session = match build_neg_session(ndb, filter, config) {
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
            conn.neg_sessions.insert(sub_id.to_owned(), session);
        }
        Err(err) => {
            let _ = out_tx.send(neg_err(sub_id, &format!("reconcile failed: {err}")));
        }
    }
}

/// One reconciliation round: `["NEG-MSG", <sub>, <msg-hex>]`. Looks up the open
/// session, folds the client's message in, and replies with the next `NEG-MSG`.
fn handle_neg_msg(frame: &[Value], out_tx: &Outbox, conn: &mut Conn) {
    let Some(sub_id) = frame.get(1).and_then(Value::as_str) else {
        let _ = out_tx.send(notice("NEG-MSG missing subscription id"));
        return;
    };
    if let Err(reason) = conn.gate() {
        let _ = out_tx.send(neg_err(sub_id, &reason));
        return;
    }
    let neg_sessions = &mut conn.neg_sessions;
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
/// Returns `Ok(None)` when the match exceeds the config's `neg_cap` (strfry
/// `maxSyncEvents`): the caller refuses the whole reconciliation rather than
/// reconciling a truncated prefix, mirroring a real relay's per-sync cap. `None`
/// cap never refuses.
fn build_neg_session(
    ndb: &Ndb,
    filter: Filter,
    config: &ServerConfig,
) -> Result<Option<NegSession>, BoxError> {
    let neg_cap = config.neg_cap;
    // One past the cap is enough to know it's over; don't load more than that.
    let query_limit = neg_cap.map_or(NEG_QUERY_LIMIT, |cap| {
        i32::try_from(cap.saturating_add(1)).unwrap_or(i32::MAX)
    });
    let txn = Transaction::new(ndb)?;
    let results = ndb.query(&txn, &[filter], query_limit)?;

    if let Some(cap) = neg_cap
        && results.len() > cap
    {
        return Ok(None);
    }

    let mut storage = NegentropyStorageVector::with_capacity(results.len());
    for result in results {
        // Must match what a `REQ` will actually serve. An id advertised here but
        // withheld there is one the client asks for and never receives.
        if !config.servable(&result.note) {
            continue;
        }
        storage.insert(
            result.note.created_at(),
            Id::from_byte_array(*result.note.id()),
        )?;
    }
    storage.seal()?;

    Ok(Some(Negentropy::owned(storage, config.neg_frame_limit())?))
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

/// `["CLOSED", <sub>, <reason>]` — a subscription the relay refused or ended.
fn closed(sub_id: &str, reason: &str) -> Message {
    Message::Text(json!(["CLOSED", sub_id, reason]).to_string())
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
