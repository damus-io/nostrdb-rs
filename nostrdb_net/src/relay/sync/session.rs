//! A long-lived client sync [`Session`]: the persistent sibling of this module's
//! one-shot [`reconcile_sync`](super::reconcile_sync)/[`pull_reconcile`](super::pull_reconcile).
//!
//! Where the one-shot path connects, does a single negentropy pass, and returns
//! (right for a CLI), a [`Session`] stays up: it **owns a [`RelayPool`]**, keeps a
//! **live `REQ`** open per subscription *and* **NIP-77 negentropy-backfills** that
//! subscription's history, tracks a **settle** point, and reconnects/keepalives —
//! all over **any** set of filters. It is entirely kind-agnostic: the caller
//! decides what to sync by handing it filters. Every embedding constructs one with
//! its own db and relays (notedeck-host over the account's private relays,
//! agentium/iOS over its own pool, the CLIs over the embedded relay) and drives it
//! through the same direct methods.
//!
//! # Shape
//!
//! [`Session::new`] `tokio::spawn`s a background [`session_loop`] that owns the
//! pool and ingests inbound events into the db; the returned handle is a thin
//! front end whose methods enqueue commands the loop drains over an internal mpsc
//! channel. Dropping the [`Session`] closes that channel and the loop returns
//! (tearing down the pool). Because the loop is spawned, `new` must be called from
//! within a Tokio runtime.
//!
//! # `!Send` discipline
//!
//! `nostrdb::Filter`/`Note`/`Transaction` wrap raw pointers and are `!Send`, so
//! the loop and its backfill tasks must never hold one across an `.await` — even a
//! value merely *in scope* across an await counts. Two rules keep every future
//! `Send` (so they spawn on the shared multi-thread runtime):
//!
//! - Filters cross the command channel and live in the loop's state as
//!   [`SendFilter`] (nostrdb's sendable, non-custom filter wrapper), converted from
//!   the caller's `Filter`s **on the caller's thread** in [`Session::set_subscription`].
//!   They are turned back into a transient `Filter` — only synchronously, never
//!   across an await — to build a `REQ` or a negentropy fold. (A filter with a
//!   custom predicate can't be a `SendFilter`, but such predicates are local-only
//!   and meaningless on the wire, so dropping them for a remote subscription is
//!   correct.)
//! - The backfill leg reduces each filter to its wire JSON synchronously, then
//!   hands it to the `Send` [`pull_reconcile_windowed`](super::pull_reconcile_windowed),
//!   whose future stays `Send` precisely because it takes no `Filter`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nostrdb::{Filter, IngestMetadata, Ndb, SendFilter};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::interval;

use super::{Progress, Relay, SyncEvent, Transient, pull_reconcile_windowed_with_progress};
use crate::{ClientMessage, RelayPool, RelayStatus, WsEvent, WsMessage};

/// How often the loop pings/reconnects relays to keep the pool alive.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

/// How many times a history backfill re-attempts a connect + reconcile after a
/// transient failure (a dropped/stalled connection, a `closed: …` NEG-ERR) before
/// giving up. Bounded so a *permanently* failing transient error can't spin the
/// task forever; on exhaustion the backfill settles (having synced what it could),
/// and the next (re)declaration — app restart, account switch, private-relay
/// change — starts a fresh set of attempts.
const BACKFILL_MAX_ATTEMPTS: u32 = 5;

/// Base backoff before the first backfill retry; each subsequent attempt doubles
/// it (200ms, 400ms, 800ms, 1.6s). Short, because a private relay is usually the
/// user's own and a transient hiccup clears quickly.
const BACKFILL_RETRY_BASE: Duration = Duration::from_millis(200);

/// Upper bound for a history backfill's initial `created_at` window: `u32::MAX`
/// (unix second `4294967295` ≈ year 2106).
///
/// Far past any real event time — so windowing covers events even when their
/// `created_at` runs ahead of this device's clock (the observed case for
/// freshly-authored envelopes) — yet still within the 32-bit range nostrdb's
/// filter `since`/`until` accept (a larger value fails `Filter::from_json` with
/// `BufferOverflow`). Windows over the relay's per-sync cap hone in by bisection,
/// so an over-wide upper bound costs only a few cheap empty-range reconciles.
const BACKFILL_UNTIL: u64 = u32::MAX as u64;

/// A handle onto a long-lived client sync loop.
///
/// Holds the sending half of the command channel into the background
/// [`session_loop`]; every method enqueues a [`Cmd`] the loop applies against its
/// owned [`RelayPool`]. Cheap to hold and `Send`; dropping it stops the loop.
///
/// See the [module docs](self) for the loop shape and the `!Send` discipline.
pub struct Session {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    /// The loop's backfill status, cloned out by [`backfill_status`](Self::backfill_status).
    /// This copy is never read, so it only keeps the channel's receiving side alive.
    status: watch::Receiver<BackfillStatus>,
}

/// A snapshot of a [`Session`]'s history backfills, published on a `watch`
/// channel (see [`Session::backfill_status`]) so a caller can show how far the
/// sync has got, and whether it gave up, instead of waiting on it blind.
///
/// Counts are cumulative over the session's life and across every backfill
/// (each subscription, each relay, each reconnect re-run).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackfillStatus {
    /// Backfill tasks started so far.
    pub started: u64,
    /// Backfill tasks finished so far, however they ended. Equal to `started`
    /// when nothing is in flight.
    pub done: u64,
    /// Events fetched across all backfills.
    pub fetched: u64,
    /// Ids the reconciles asked to fetch, summed over every reconcile (each
    /// window of each filter). `fetched / need` is the determinate fraction; a
    /// window too dense to bisect is pulled with a plain `REQ` and reports no
    /// need, so `fetched` can run slightly past it.
    pub need: u64,
    /// `created_at` windows finished.
    pub windows: u64,
    /// Coverage of the latest finished window's pull: `created_at` seconds
    /// reconciled so far out of its whole range, the range clamped to now (see
    /// [`SyncEvent::Window`]).
    pub covered_secs: u64,
    pub span_secs: u64,
    /// At least one backfill ended without completing: it gave up after
    /// [`BACKFILL_MAX_ATTEMPTS`] transient failures, or a filter failed
    /// permanently (a `blocked: …` NEG-ERR, a relay without NIP-77). A cancelled
    /// backfill is not a failure; it was replaced or no longer wanted.
    pub failed: bool,
}

impl Session {
    /// Spawn a sync loop over `ndb` and return a handle onto it.
    ///
    /// `ndb` is held by value (it is `Clone` over an `Arc`-backed handle, so this
    /// is a cheap reference to the same database); inbound events ingest into it,
    /// and it is the settle target for backfills. Must be called from within a
    /// Tokio runtime, as it `tokio::spawn`s the loop.
    pub fn new(ndb: Ndb) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let progress = Arc::new(BackfillProgress::default());
        let status = progress.status.subscribe();
        tokio::spawn(session_loop(ndb, cmd_rx, progress));
        Self { cmd_tx, status }
    }

    /// Watch the session's history backfills: `borrow()` is the latest
    /// [`BackfillStatus`], and `changed()` resolves on the next update after this
    /// call. Cloned off the handle, so it needs no round trip through the loop.
    ///
    /// The status settles (`done == started`) by the time
    /// [`wait_for_sync`](Self::wait_for_sync) resolves for those backfills, so a
    /// caller can read `failed` straight after the wait.
    pub fn backfill_status(&self) -> watch::Receiver<BackfillStatus> {
        let mut rx = self.status.clone();
        rx.borrow_and_update();
        rx
    }

    /// Declare (upsert) a subscription `id` targeting one relay `url`.
    ///
    /// `live_filters` drive the ongoing live `REQ` (re-sent whenever the relay
    /// reconnects); `history_filters` (possibly empty) request a bounded NIP-77
    /// negentropy backfill of past events, re-run each time the relay reconnects so
    /// events missed while disconnected are caught up. Both are plain nostrdb `Filter`s,
    /// converted to the sendable [`SendFilter`] here on the caller's thread before
    /// they cross to the loop — a filter carrying a custom (local-only) predicate
    /// is dropped, since it has no wire meaning. Re-declaring an existing `id`
    /// replaces its live filters and spawns a fresh backfill.
    pub fn set_subscription(
        &self,
        id: impl Into<String>,
        url: impl Into<String>,
        live_filters: Vec<Filter>,
        history_filters: Vec<Filter>,
    ) {
        let _ = self.cmd_tx.send(Cmd::SetSubscription {
            id: id.into(),
            url: url.into(),
            live: to_send_filters(live_filters),
            history: to_send_filters(history_filters),
        });
    }

    /// Drop the subscription previously declared under `id`, sending a `CLOSE` to
    /// every relay and forgetting its live filters so a reconnect won't re-send it.
    pub fn drop_subscription(&self, id: impl Into<String>) {
        let _ = self.cmd_tx.send(Cmd::DropSubscription(id.into()));
    }

    /// Publish a pre-serialized event JSON (the bare `{...}` object) to each of
    /// `relays`, connecting to any not already in the pool. A relay that isn't up
    /// yet has the frame queued until it opens.
    pub fn publish(&self, note_json: String, relays: Vec<String>) {
        let _ = self.cmd_tx.send(Cmd::Publish { note_json, relays });
    }

    /// Resolve once the history backfill(s) in flight *at this call* have settled.
    ///
    /// Returns immediately if none are in flight (no history filters, or nothing
    /// subscribed yet). Deterministic: because the barrier rides the same FIFO
    /// command channel behind every prior [`set_subscription`](Self::set_subscription),
    /// by the time the loop handles it every earlier subscription has already
    /// spawned its backfill and bumped the started count — and each backfill's
    /// `sync_into` returns only once its received events are queryable, so
    /// "settled" means the reconciled history is actually readable, not that a
    /// timer elapsed. A dropped loop (session gone) resolves immediately.
    pub async fn wait_for_sync(&self) {
        let (reply, rx) = oneshot::channel();
        if self.cmd_tx.send(Cmd::WaitForSync(reply)).is_err() {
            return;
        }
        if let Ok(settle) = rx.await {
            settle.settled().await;
        }
    }
}

/// Wrap caller filters as [`SendFilter`] so they can cross to the loop thread,
/// dropping any with a custom predicate (not sendable, and not meaningful on the
/// wire for a remote subscription).
fn to_send_filters(filters: Vec<Filter>) -> Vec<SendFilter> {
    filters
        .into_iter()
        .filter_map(|f| SendFilter::try_from_filter(f).ok())
        .collect()
}

/// A command from a [`Session`] method to the background [`session_loop`].
///
/// Every field is `Send`: filters are wrapped as [`SendFilter`] and relay urls
/// are strings, converted on the caller's thread.
enum Cmd {
    SetSubscription {
        id: String,
        url: String,
        live: Vec<SendFilter>,
        history: Vec<SendFilter>,
    },
    DropSubscription(String),
    Publish {
        note_json: String,
        relays: Vec<String>,
    },
    /// A settle barrier (see [`Session::wait_for_sync`]). Because this rides the
    /// same FIFO command channel, by the time the loop handles it every
    /// previously-enqueued [`SetSubscription`](Cmd::SetSubscription) has already
    /// spawned its backfill and bumped the started count — so the loop can hand
    /// back a [`SyncSettle`] that resolves once exactly those backfills complete.
    WaitForSync(oneshot::Sender<SyncSettle>),
}

/// Shared backfill-completion tracker owned by the [`session_loop`] and observed
/// by [`SyncSettle`] waiters.
///
/// `done` counts every backfill task that has *finished* — success, error, or
/// panic all count, via a drop guard — so a settle wait can never hang on a task
/// that died. `notify` wakes waiters on each completion. The started count is held
/// loop-locally (only the loop spawns backfills), so it needs no atomic.
///
/// `status` publishes the [`BackfillStatus`] snapshot. The backfill tasks report
/// into it through this struct's [`Progress`] impl; `send_modify` never blocks a
/// reader, who only ever borrows the latest value.
struct BackfillProgress {
    done: AtomicU64,
    notify: Notify,
    status: watch::Sender<BackfillStatus>,
}

impl Default for BackfillProgress {
    fn default() -> Self {
        Self {
            done: AtomicU64::new(0),
            notify: Notify::new(),
            status: watch::Sender::new(BackfillStatus::default()),
        }
    }
}

impl BackfillProgress {
    /// Record that a backfill ended without completing (see
    /// [`BackfillStatus::failed`]).
    fn fail(&self) {
        self.status.send_modify(|s| s.failed = true);
    }
}

/// The backfill tasks' observer: folds each reconcile step into the published
/// [`BackfillStatus`]. `Sync`, so the backfill future stays `Send`.
impl Progress for BackfillProgress {
    fn event(&self, ev: SyncEvent) {
        match ev {
            SyncEvent::Need { count } => self.status.send_modify(|s| s.need += count as u64),
            SyncEvent::Fetched { count } => self.status.send_modify(|s| s.fetched += count as u64),
            SyncEvent::Window {
                covered_secs,
                span_secs,
            } => self.status.send_modify(|s| {
                s.windows += 1;
                s.covered_secs = covered_secs;
                s.span_secs = span_secs;
            }),
            SyncEvent::Round | SyncEvent::Pushed { .. } | SyncEvent::Split => {}
        }
    }
}

/// A one-shot settle handle: resolves once the tracked history backfill(s) have
/// completed. Captures a `target` snapshot of how many backfills had been started
/// when the barrier reached the loop, and resolves once that many are done.
struct SyncSettle {
    progress: Arc<BackfillProgress>,
    target: u64,
}

impl SyncSettle {
    /// Resolve once the tracked backfills have completed. Returns immediately if
    /// none were in flight at the barrier.
    async fn settled(self) {
        loop {
            // Register for the wakeup *before* re-checking, so a completion that
            // lands between the check and the await is not lost.
            let notified = self.progress.notify.notified();
            if self.progress.done.load(Ordering::Acquire) >= self.target {
                return;
            }
            notified.await;
        }
    }
}

/// The mutable state the [`session_loop`] threads through each [`apply_cmd`]: the
/// relay pool, the desired-subscription and pending-publish maps, and the backfill
/// settle tracker. Bundled into one struct so commands are applied by
/// `&mut LoopState` rather than a long argument list.
#[derive(Default)]
struct LoopState {
    pool: RelayPool,
    /// Desired live subscriptions per relay url (subid -> filters), re-sent
    /// whenever a relay (re)connects.
    desired: HashMap<String, HashMap<String, Vec<SendFilter>>>,
    /// History filters per relay url (subid -> filters), for subscriptions that
    /// asked for a backfill. Re-run whenever a relay *re*connects (see
    /// [`opened`](Self::opened)): the live `REQ` re-sent on reconnect replays only
    /// the relay's newest stored events (strfry caps a stored replay at 500), so
    /// anything published while we were disconnected, beyond that slice, would
    /// otherwise never arrive.
    history: HashMap<String, HashMap<String, Vec<SendFilter>>>,
    /// Relay urls that have reported `Opened` at least once. A later `Opened` for
    /// one of these is a reconnect; the first is the initial connect, whose
    /// backfill [`Cmd::SetSubscription`] already spawned.
    opened: HashSet<String>,
    /// Publish frames queued until their target relay is connected.
    pending: HashMap<String, Vec<String>>,
    /// Published events awaiting the relay's `OK`, keyed by `(relay url, event id
    /// hex)` and holding the bare event JSON. When the relay acknowledges one (see
    /// [`record_publish_ok`]) the JSON is re-ingested tagged with that relay, so
    /// nostrdb records the relay as a seen-on source for a note we authored or
    /// already held. Entries for a relay are dropped when its connection closes;
    /// a re-publish after reconnect re-arms them.
    unacked: HashMap<(String, String), String>,
    /// Backfill settle tracking (see [`Session::wait_for_sync`]): the shared
    /// done-counter the detached backfill tasks bump, paired with `started`.
    progress: Arc<BackfillProgress>,
    /// Count of backfills spawned so far, snapshotted by a `WaitForSync` barrier
    /// as its settle target (against `progress.done`).
    started: u64,
    /// Cancel handles for in-flight backfill tasks, keyed by subscription id +
    /// relay url. Dropping a sender cancels its task — which, if a reconcile is in
    /// flight, emits a `NEG-CLOSE` before stopping (see [`backfill`]). Re-declaring a
    /// given `(id, url)` cancels the prior task there; dropping a subscription
    /// cancels every task under that id across all relays.
    backfills: HashMap<BackfillKey, oneshot::Sender<()>>,
}

/// Identifies one in-flight backfill task: a subscription's history reconcile
/// against a single relay. A logical subscription can span several relays (one
/// [`Session::set_subscription`] call per url under the same id), each with its own
/// backfill, so the relay url is part of the key — re-declaring one relay's leg
/// must not cancel a sibling relay's fresh backfill from the same declaration.
#[derive(Clone, PartialEq, Eq, Hash)]
struct BackfillKey {
    id: String,
    url: String,
}

/// The background relay loop: owns the [`RelayPool`], ingests inbound events into
/// `ndb`, and applies [`Cmd`]s. Returns when the command channel closes (the
/// [`Session`] handle dropped).
async fn session_loop(
    ndb: Ndb,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    progress: Arc<BackfillProgress>,
) {
    let notify = Arc::new(Notify::new());
    let wakeup = {
        let notify = notify.clone();
        move || notify.notify_one()
    };

    let mut state = LoopState {
        progress,
        ..LoopState::default()
    };
    let mut keepalive = interval(KEEPALIVE_INTERVAL);

    loop {
        // Drain everything the pool has ready: ingest events, and flush desired
        // subs / queued publishes to relays as they come up.
        while let Some(ev) = state.pool.try_recv().map(|e| e.into_owned()) {
            match ev.event {
                WsEvent::Opened => {
                    if let Some(subs) = state.desired.get(&ev.relay) {
                        for (sid, filters) in subs {
                            state
                                .pool
                                .send_to(&req_message(sid.clone(), filters), &ev.relay);
                        }
                    }
                    let reconnect = !state.opened.insert(ev.relay.clone());
                    if reconnect {
                        rebackfill_relay(&ndb, &mut state, &ev.relay);
                    }
                    if let Some(frames) = state.pending.remove(&ev.relay) {
                        for frame in frames {
                            state.pool.send_to(&ClientMessage::raw(frame), &ev.relay);
                        }
                    }
                }
                WsEvent::Message(WsMessage::Text(text)) => {
                    if text.starts_with(r#"["OK""#) {
                        record_publish_ok(&ndb, &mut state.unacked, &ev.relay, &text);
                        continue;
                    }
                    // Only EVENT frames ingest; EOSE/NOTICE aren't events, so a
                    // parse/ingest failure here is expected, not an error. Tag the
                    // ingest with the relay it came from so nostrdb records it as a
                    // seen-on source — fan-out paths skip relays a note was already
                    // seen on, and without this they would echo every synced event
                    // straight back to the relay that sent it.
                    if let Err(e) = ndb.process_event_with(&text, relay_ingest(&ev.relay)) {
                        tracing::trace!("session: skipped non-event relay message: {e}");
                    }
                }
                WsEvent::Closed | WsEvent::Error(_) => {
                    // An `OK` can't arrive on a dead connection; forget what was
                    // awaiting one there so the map can't grow without bound.
                    state.unacked.retain(|(url, _), _| url != &ev.relay);
                }
                _ => {}
            }
        }

        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break }; // session dropped
                apply_cmd(&ndb, &mut state, &wakeup, cmd);
            }
            // The pool signalled new data; loop back around to drain it.
            _ = notify.notified() => {}
            _ = keepalive.tick() => state.pool.keepalive_ping(wakeup.clone()),
        }
    }
}

/// Apply one [`Cmd`] against the loop's [`LoopState`].
fn apply_cmd(
    ndb: &Ndb,
    state: &mut LoopState,
    wakeup: &(impl Fn() + Send + Sync + Clone + 'static),
    cmd: Cmd,
) {
    match cmd {
        Cmd::SetSubscription {
            id,
            url,
            live,
            history,
        } => {
            // Key `desired` by the pool's canonical url so the `Opened` handler
            // (which sees the pool's own url) finds it. `add_url` canonicalizes
            // internally, so an un-normalized key here would silently never flush.
            let url = RelayPool::canonicalize_url(url);
            let _ = state.pool.add_url(url.clone(), wakeup.clone());
            // Send the live REQ now if the relay is already up; otherwise it is
            // flushed from `desired` on the relay's `Opened` event.
            if relay_connected(&state.pool, &url) {
                state.pool.send_to(&req_message(id.clone(), &live), &url);
            }
            state
                .desired
                .entry(url.clone())
                .or_default()
                .insert(id.clone(), live);
            if history.is_empty() {
                if let Some(subs) = state.history.get_mut(&url) {
                    subs.remove(&id);
                }
            } else {
                state
                    .history
                    .entry(url.clone())
                    .or_default()
                    .insert(id.clone(), history.clone());
                spawn_backfill(ndb, state, id, url, history);
            }
        }
        Cmd::DropSubscription(id) => {
            for subs in state.desired.values_mut() {
                subs.remove(&id);
            }
            for subs in state.history.values_mut() {
                subs.remove(&id);
            }
            // Cancel every in-flight backfill under this id (across all relays).
            // Dropping each sender fires its task's cancel, which emits a NEG-CLOSE
            // if a reconcile was still in flight, so the relay stops tracking the
            // negentropy session instead of leaving it half-open until it times out.
            state.backfills.retain(|key, _| key.id != id);
            state.pool.unsubscribe(id);
        }
        Cmd::Publish { note_json, relays } => {
            let frame = format!(r#"["EVENT",{note_json}]"#);
            let event_id = serde_json::from_str::<serde_json::Value>(&note_json)
                .ok()
                .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_owned));
            for url in relays {
                // Canonicalize so a queued frame is keyed the same way the pool
                // reports the relay on `Opened` — otherwise it never flushes.
                let url = RelayPool::canonicalize_url(url);
                let _ = state.pool.add_url(url.clone(), wakeup.clone());
                if let Some(id) = &event_id {
                    state
                        .unacked
                        .insert((url.clone(), id.clone()), note_json.clone());
                }
                if relay_connected(&state.pool, &url) {
                    state.pool.send_to(&ClientMessage::raw(frame.clone()), &url);
                } else {
                    state.pending.entry(url).or_default().push(frame.clone());
                }
            }
        }
        Cmd::WaitForSync(reply) => {
            // Snapshot how many backfills have been started by now; the returned
            // handle resolves once that many have completed. Because this command
            // rode the FIFO channel behind every prior `SetSubscription`, that
            // snapshot already includes their backfills. A dropped `reply` (the
            // waiter went away) is harmless.
            let _ = reply.send(SyncSettle {
                progress: state.progress.clone(),
                target: state.started,
            });
        }
    }
}

/// Spawn the NIP-77 negentropy history backfill for subscription `id` on `url`,
/// off the loop on its own task. Its completion is tracked (via a drop guard, so a
/// panic still counts) so a `WaitForSync` barrier can observe when history has
/// settled.
///
/// Any prior backfill for this exact `(id, url)` is cancelled first, so a
/// re-declaration or a reconnect replaces it rather than racing a duplicate
/// reconcile (and a duplicate `NEG-OPEN`) against the same relay.
fn spawn_backfill(
    ndb: &Ndb,
    state: &mut LoopState,
    id: String,
    url: String,
    history: Vec<SendFilter>,
) {
    let key = BackfillKey {
        id,
        url: url.clone(),
    };
    state.backfills.remove(&key);
    state.started += 1;
    state.progress.status.send_modify(|s| s.started += 1);
    let ndb = ndb.clone();
    let progress = state.progress.clone();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _guard = BackfillDoneGuard(progress.clone());
        if let BackfillEnd::Failed = backfill(ndb, url, history, cancel_rx, &progress).await {
            progress.fail();
        }
    });
    state.backfills.insert(key, cancel_tx);
}

/// Re-run the history backfill of every subscription on `relay` after it
/// reconnects. Negentropy transfers only what we lack, so on a short blip this is
/// a cheap no-op pass; after a long disconnect it is what fetches the events the
/// live `REQ`'s capped replay dropped.
fn rebackfill_relay(ndb: &Ndb, state: &mut LoopState, relay: &str) {
    let Some(subs) = state.history.get(relay) else {
        return;
    };
    let subs: Vec<(String, Vec<SendFilter>)> = subs
        .iter()
        .map(|(id, filters)| (id.clone(), filters.clone()))
        .collect();
    for (id, history) in subs {
        tracing::debug!("session: {relay} reconnected; re-running backfill for {id}");
        spawn_backfill(ndb, state, id, relay.to_owned(), history);
    }
}

/// Ingest metadata for an event received from (or acknowledged by) `relay`, so
/// nostrdb records the relay among the note's seen-on sources.
fn relay_ingest(relay: &str) -> IngestMetadata {
    IngestMetadata::new().client(false).relay(relay)
}

/// Handle a relay's `["OK", <id>, <accepted>, <reason>]` for an event this session
/// published: if the relay now holds the event — it accepted it, or rejected it as
/// a `duplicate:` of one it already had — re-ingest the event tagged with that
/// relay. nostrdb already has the note, so this only adds the relay to its seen-on
/// set, which is what stops fan-out paths re-publishing it on every run. Any other
/// rejection just clears the pending entry.
fn record_publish_ok(
    ndb: &Ndb,
    unacked: &mut HashMap<(String, String), String>,
    relay: &str,
    text: &str,
) {
    let Ok(frame) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        return;
    };
    let Some(id) = frame.get(1).and_then(|v| v.as_str()) else {
        return;
    };
    let Some(note_json) = unacked.remove(&(relay.to_owned(), id.to_owned())) else {
        return;
    };
    let accepted = frame.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
    let reason = frame.get(3).and_then(|v| v.as_str()).unwrap_or("");
    if !accepted && !reason.starts_with("duplicate:") {
        return;
    }
    let event = format!(r#"["EVENT","seen",{note_json}]"#);
    if let Err(e) = ndb.process_event_with(&event, relay_ingest(relay)) {
        tracing::debug!("session: couldn't record {relay} as seen-on for {id}: {e}");
    }
}

/// Drop guard that records a backfill task's completion on the shared
/// [`BackfillProgress`]. Using `Drop` (rather than a bump at the end of the task
/// body) means an error return or a panic unwinding through the task still counts
/// as done, so a [`SyncSettle`] wait can never hang on a dead backfill.
struct BackfillDoneGuard(Arc<BackfillProgress>);

impl Drop for BackfillDoneGuard {
    fn drop(&mut self) {
        // Status first, so a settle waiter woken by `done` reads a final status.
        self.0.status.send_modify(|s| s.done += 1);
        self.0.done.fetch_add(1, Ordering::Release);
        self.0.notify.notify_waiters();
    }
}

/// Build a `REQ` [`ClientMessage`] for `filters`. Cloning each [`SendFilter`] back
/// to a transient `Filter` is done synchronously (never across an await), so the
/// loop future stays `Send`.
fn req_message(sid: String, filters: &[SendFilter]) -> ClientMessage {
    ClientMessage::req(sid, filters.iter().map(|f| f.as_filter().clone()).collect())
}

/// Whether `url` is currently connected in the pool (so a `REQ`/`EVENT` can be
/// sent now rather than deferred until its `Opened` event).
fn relay_connected(pool: &RelayPool, url: &str) -> bool {
    pool.relays
        .iter()
        .any(|r| r.relay.url == url && matches!(r.relay.status, RelayStatus::Connected))
}

/// Pull history for a subscription from `url` using NIP-77 negentropy, retrying a
/// transient failure with backoff and stopping promptly on `cancel`.
///
/// Each attempt opens a dedicated reconcile connection (separate from the live
/// pool) and, for each history filter, reconciles the relay against the local db
/// and fetches the ids the relay holds that we lack — via [`pull_reconcile_windowed`](super::pull_reconcile_windowed),
/// so a filter matching more events than the relay's per-sync cap still syncs (it
/// bisects the `created_at` range under the cap).
///
/// A transient interruption — a dropped/stalled connection or a `closed: …`
/// NEG-ERR ([`Transient`]) — retries the whole pass after a backoff, up to
/// [`BACKFILL_MAX_ATTEMPTS`], so a private relay's momentary hiccup doesn't strand
/// the account's history until the next (re)declaration. A permanent failure (a
/// `blocked: …` NEG-ERR, a non-NIP-77 relay) is logged and *not* retried, since a
/// retry can't fix it.
///
/// `cancel` fires when the loop tears this backfill down (a dropped subscription or
/// a re-declaration). When it fires mid-reconcile the task emits a `NEG-CLOSE` on
/// its connection before returning, so the relay stops tracking the negentropy
/// session rather than leaving it half-open until it times out.
///
/// Each reconcile step is reported to `progress`, and the returned [`BackfillEnd`]
/// says whether the history synced in full.
async fn backfill(
    ndb: Ndb,
    url: String,
    filters: Vec<SendFilter>,
    mut cancel: oneshot::Receiver<()>,
    progress: &BackfillProgress,
) -> BackfillEnd {
    // Reduce every filter to its wire JSON once, up front and synchronously: the
    // transient `nostrdb::Filter` from `as_filter()` (`!Send`) drops inside the
    // closure, so it never crosses an await, and the resulting `Vec<String>` is
    // `Send` — letting `backfill_once` iterate it across awaits without pinning a
    // `!Send` `SendFilter` (or its `!Send` slice iterator) into the future.
    let filter_jsons: Vec<String> = filters
        .iter()
        .filter_map(|f| f.as_filter().json().ok())
        .collect();

    for attempt in 1..=BACKFILL_MAX_ATTEMPTS {
        match backfill_once(&ndb, &url, &filter_jsons, &mut cancel, progress).await {
            BackfillOutcome::Done => return BackfillEnd::Complete,
            BackfillOutcome::Incomplete => return BackfillEnd::Failed,
            BackfillOutcome::Cancelled => return BackfillEnd::Cancelled,
            BackfillOutcome::Retry => {}
        }
        if attempt == BACKFILL_MAX_ATTEMPTS {
            tracing::warn!("session backfill: {url}: giving up after {attempt} attempts");
            return BackfillEnd::Failed;
        }
        // Back off before the next attempt, but abandon the wait at once if the
        // backfill is cancelled meanwhile.
        let delay = BACKFILL_RETRY_BASE * 2u32.pow(attempt - 1);
        tokio::select! {
            biased;
            _ = &mut cancel => return BackfillEnd::Cancelled,
            _ = tokio::time::sleep(delay) => {}
        }
    }
    BackfillEnd::Failed
}

/// How a whole [`backfill`] task ended, so the spawn site can flag
/// [`BackfillStatus::failed`].
enum BackfillEnd {
    /// Every filter reconciled.
    Complete,
    /// Gave up after [`BACKFILL_MAX_ATTEMPTS`], or a filter failed permanently.
    Failed,
    /// Cancelled by the loop: replaced or no longer wanted, not a failure.
    Cancelled,
}

/// The result of one [`backfill_once`] pass, telling [`backfill`] what to do next.
enum BackfillOutcome {
    /// Every filter reconciled. Stop.
    Done,
    /// Every filter ran, but at least one failed permanently and was skipped. Stop:
    /// a retry can't fix it.
    Incomplete,
    /// A transient failure interrupted the pass; retry the whole thing after a
    /// backoff.
    Retry,
    /// `cancel` fired; the task has emitted any needed `NEG-CLOSE` and should stop.
    Cancelled,
}

/// One connect + reconcile pass over every history filter, interruptible by
/// `cancel`.
///
/// Returns [`Retry`](BackfillOutcome::Retry) on a transient interruption (a failed
/// connect, a dropped/stalled connection, a `closed: …` NEG-ERR), so
/// [`backfill`] backs off and tries the whole pass again. A *permanent* per-filter
/// failure is logged and skipped rather than aborting the pass — a retry can't fix
/// it, and it must not stop the other filters — so the pass runs on and ends
/// [`Incomplete`](BackfillOutcome::Incomplete). On `cancel` mid-reconcile it emits a
/// `NEG-CLOSE` and returns [`Cancelled`](BackfillOutcome::Cancelled).
///
/// `filter_jsons` are the history filters already reduced to their wire JSON by
/// [`backfill`] (see there for why that reduction happens up front). Every error
/// `Box` (`!Send`) is classified synchronously before the next await, keeping this
/// future `Send` so it can run on the multi-thread runtime.
async fn backfill_once(
    ndb: &Ndb,
    url: &str,
    filter_jsons: &[String],
    cancel: &mut oneshot::Receiver<()>,
    progress: &BackfillProgress,
) -> BackfillOutcome {
    let mut relay = tokio::select! {
        biased;
        _ = &mut *cancel => return BackfillOutcome::Cancelled,
        connected = Relay::connect(url) => match connected {
            Ok(relay) => relay,
            Err(e) => {
                // A failed connect is transient — the relay may just be momentarily
                // down; retry after backoff.
                tracing::warn!("session backfill: connect {url} failed: {e}");
                return BackfillOutcome::Retry;
            }
        },
    };

    let mut skipped = false;
    for filter_json in filter_jsons {
        // Race the reconcile against cancellation. The cancel arm returns only a
        // flag (it must not touch `relay`, which the reconcile arm borrows); the
        // NEG-CLOSE is sent *after* the select, once that borrow is released.
        let cancelled = tokio::select! {
            biased;
            _ = &mut *cancel => true,
            result = pull_reconcile_windowed_with_progress(
                &mut relay, ndb, filter_json, BACKFILL_UNTIL, progress,
            ) => {
                if let Err(e) = result {
                    // Classify synchronously (the error `Box` is `!Send`) before the
                    // next await: a transient error retries the whole pass, a
                    // permanent one is logged and this filter skipped.
                    if is_transient(&*e) {
                        tracing::warn!("session backfill: {url}: {e} (retrying)");
                        return BackfillOutcome::Retry;
                    }
                    tracing::warn!("session backfill: {url}: {e}");
                    skipped = true;
                }
                false
            }
        };
        if cancelled {
            let _ = relay.close_negentropy().await;
            return BackfillOutcome::Cancelled;
        }
    }
    if skipped {
        return BackfillOutcome::Incomplete;
    }
    BackfillOutcome::Done
}

/// Whether a backfill error is a transient connection/relay hiccup worth retrying,
/// versus a permanent refusal a retry can't fix. See [`Transient`].
fn is_transient(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<Transient>().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::server::{self, RelayHandle};
    use futures_util::StreamExt;
    use nostrdb::{Config, Ndb, NoteBuilder, SubscriptionStream, Transaction};
    use tempfile::TempDir;

    /// A fixed secret key for signing test events, so ids are deterministic.
    const TEST_SECKEY: [u8; 32] = [7u8; 32];

    /// The kind we sync in these tests.
    const KIND: u32 = 30000;
    /// A kind the seeded event is *not*, so a live filter over it can't match —
    /// isolating the backfill leg.
    const UNRELATED_KIND: u32 = 30001;

    /// Open a fresh nostrdb under a throwaway directory. The returned [`TempDir`]
    /// must outlive the db.
    fn temp_ndb() -> (TempDir, Ndb) {
        let dir = TempDir::new().expect("tmp dir");
        let ndb = Ndb::new(dir.path().to_str().expect("path"), &Config::new()).expect("ndb");
        (dir, ndb)
    }

    /// Spawn an in-process hermetic relay backed by `ndb` on an ephemeral port.
    fn spawn_relay(ndb: Ndb) -> RelayHandle {
        server::spawn(ndb, "127.0.0.1:0".parse().expect("addr")).expect("spawn relay")
    }

    /// Count events of `kind` currently queryable in `ndb`.
    fn count_kind(ndb: &Ndb, kind: u64) -> usize {
        let txn = Transaction::new(ndb).expect("txn");
        let filter = Filter::new().kinds([kind]).build();
        ndb.query(&txn, &[filter], 1_000_000).expect("query").len()
    }

    /// Build a signed note, returning its bare event JSON and 32-byte id.
    fn signed_note(kind: u32, content: &str) -> (String, [u8; 32]) {
        let note = NoteBuilder::new()
            .kind(kind)
            .content(content)
            .sign(&TEST_SECKEY)
            .build()
            .expect("build note");
        (note.json().expect("note json"), *note.id())
    }

    /// The `["EVENT", {...}]` client frame for a bare event JSON.
    fn event_frame(note_json: &str) -> String {
        format!(r#"["EVENT",{note_json}]"#)
    }

    /// Ingest `n` distinct signed events of `kind` directly into `ndb` (used to
    /// stock a relay's store before a client reconciles it). Distinct content
    /// gives distinct ids. Returns the ids in creation order.
    fn seed_events(ndb: &Ndb, kind: u32, n: usize) -> Vec<[u8; 32]> {
        (0..n)
            .map(|i| {
                let (json, id) = signed_note(kind, &format!("seed-{i}"));
                ndb.process_client_event(&event_frame(&json))
                    .expect("ingest seed");
                id
            })
            .collect()
    }

    /// Wait until note `id` (of `kind`) is queryable in `ndb`, driven by a
    /// [`SubscriptionStream`] with a backstop `timeout`. Subscribes first, then
    /// checks presence and awaits ingests, so it catches a note whether it landed
    /// just before or after the call.
    async fn await_note(ndb: &Ndb, id: [u8; 32], kind: u64, timeout: Duration) -> bool {
        let filter = Filter::new().kinds([kind]).build();
        let sub = ndb
            .subscribe(std::slice::from_ref(&filter))
            .expect("subscribe");
        let mut stream = SubscriptionStream::new(ndb.clone(), sub);

        tokio::time::timeout(timeout, async {
            loop {
                {
                    let txn = Transaction::new(ndb).expect("txn");
                    if ndb.get_note_by_id(&txn, &id).is_ok() {
                        return true;
                    }
                }
                if stream.next().await.is_none() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    /// A [`Session`] backfills a relay's existing history into a second db via the
    /// negentropy leg — proven in isolation by a live filter that cannot match the
    /// seeded event, so only the history backfill can deliver it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfills_history_into_a_second_db() {
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();

        // Author on A: ingest locally, then publish the event to the relay.
        let (_a_dir, ndb_a) = temp_ndb();
        let (note_json, note_id) = signed_note(KIND, "backfill me");
        ndb_a
            .process_client_event(&event_frame(&note_json))
            .expect("ingest on A");
        let session_a = Session::new(ndb_a.clone());
        session_a.publish(note_json.clone(), vec![url.clone()]);

        // The relay must actually hold + index it before B reconciles history.
        assert!(
            await_note(&relay_ndb, note_id, KIND as u64, Duration::from_secs(5)).await,
            "relay should store A's published event"
        );

        // B subscribes with a live filter that can't match the seed (different
        // kind) and a history filter that can — so an arrival proves the backfill
        // leg, not live replay.
        let (_b_dir, ndb_b) = temp_ndb();
        let session_b = Session::new(ndb_b.clone());
        let live_none = Filter::new().kinds([UNRELATED_KIND as u64]).build();
        let history = Filter::new().kinds([KIND as u64]).build();
        session_b.set_subscription("backfill", url.clone(), vec![live_none], vec![history]);

        session_b.wait_for_sync().await;

        assert!(
            await_note(&ndb_b, note_id, KIND as u64, Duration::from_secs(5)).await,
            "B should backfill the event from the relay"
        );
        relay.shutdown();
    }

    /// `wait_for_sync` resolves only once the backfilled history is actually
    /// queryable: a plain *synchronous* count the instant it returns already sees
    /// the whole seeded set (settle = readable, not a timer), and the count
    /// matches what the relay held. The live filter is over an unrelated kind so
    /// it cannot match — only the negentropy backfill leg can deliver the events.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_sync_settles_with_whole_history_queryable() {
        const SEEDED: usize = 12;
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();

        let ids = seed_events(&relay_ndb, KIND, SEEDED);
        let last = *ids.last().expect("seeded ids");
        assert!(
            await_note(&relay_ndb, last, KIND as u64, Duration::from_secs(5)).await,
            "relay should index all seeded events before B reconciles"
        );

        // B subscribes with a live filter that can't match (different kind) and a
        // history filter that can, so an arrival proves the backfill, not replay.
        let (_b_dir, ndb_b) = temp_ndb();
        let session_b = Session::new(ndb_b.clone());
        let live_none = Filter::new().kinds([UNRELATED_KIND as u64]).build();
        let history = Filter::new().kinds([KIND as u64]).build();
        session_b.set_subscription("hist", url.clone(), vec![live_none], vec![history]);

        tokio::time::timeout(Duration::from_secs(10), session_b.wait_for_sync())
            .await
            .expect("backfill should settle within the bound");

        // The instant settle resolves, every event is queryable — no extra wait.
        assert_eq!(
            count_kind(&ndb_b, KIND as u64),
            SEEDED,
            "settle must mean the whole reconciled history is readable"
        );
        relay.shutdown();
    }

    /// The backfill status follows a successful backfill and is final by the
    /// time `wait_for_sync` resolves: every missing event counted as needed and
    /// fetched, the window range fully covered, the task done, nothing failed.
    /// A receiver taken before the subscription sees `changed()` fire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_status_counts_a_complete_backfill() {
        const SEEDED: usize = 7;
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();
        let ids = seed_events(&relay_ndb, KIND, SEEDED);
        let last = *ids.last().expect("seeded ids");
        assert!(await_note(&relay_ndb, last, KIND as u64, Duration::from_secs(5)).await);

        let (_b_dir, ndb_b) = temp_ndb();
        let session_b = Session::new(ndb_b.clone());
        let mut status = session_b.backfill_status();
        assert_eq!(*status.borrow(), BackfillStatus::default());

        let live_none = Filter::new().kinds([UNRELATED_KIND as u64]).build();
        let history = Filter::new().kinds([KIND as u64]).build();
        session_b.set_subscription("hist", url.clone(), vec![live_none], vec![history]);

        tokio::time::timeout(Duration::from_secs(10), status.changed())
            .await
            .expect("the status should update once the backfill starts")
            .expect("the session is alive");
        tokio::time::timeout(Duration::from_secs(10), session_b.wait_for_sync())
            .await
            .expect("backfill should settle within the bound");

        let s = status.borrow().clone();
        assert_eq!(s.started, 1, "{s:?}");
        assert_eq!(s.done, s.started, "{s:?}");
        assert_eq!(s.fetched, SEEDED as u64, "{s:?}");
        assert_eq!(s.need, SEEDED as u64, "{s:?}");
        assert!(s.windows >= 1, "{s:?}");
        assert!(s.span_secs > 0, "{s:?}");
        assert_eq!(
            s.covered_secs, s.span_secs,
            "the range is fully covered: {s:?}"
        );
        assert!(!s.failed, "{s:?}");
        relay.shutdown();
    }

    /// With no history filter there is nothing to backfill, so the barrier
    /// snapshots a target of zero and `wait_for_sync` returns immediately rather
    /// than blocking on a sync. Covers both a live-only subscription and a
    /// session that never subscribed at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_sync_returns_immediately_without_history() {
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();

        // Live-only subscription: no backfill task is ever started.
        let (_b_dir, ndb_b) = temp_ndb();
        let session = Session::new(ndb_b);
        let live = Filter::new().kinds([KIND as u64]).build();
        session.set_subscription("live", url, vec![live], vec![]);
        tokio::time::timeout(Duration::from_secs(2), session.wait_for_sync())
            .await
            .expect("live-only settle must not block on a backfill");

        // A session that never subscribed also settles at once.
        let (_c_dir, ndb_c) = temp_ndb();
        let fresh = Session::new(ndb_c);
        tokio::time::timeout(Duration::from_secs(2), fresh.wait_for_sync())
            .await
            .expect("un-subscribed settle must not block");
        relay.shutdown();
    }

    /// The settle barrier resolves when the backfill *task finishes*, and the
    /// [`BackfillDoneGuard`] drop-guard counts a task that errored (or panicked)
    /// as done too — so a backfill against an unreachable relay still settles,
    /// having synced nothing. This pins the settle-on-exhaustion semantics: a
    /// settle is *not* proof of a complete history, only that the attempt (now:
    /// every retry) finished.
    ///
    /// A failed connect is transient, so [`backfill`] retries it
    /// [`BACKFILL_MAX_ATTEMPTS`] times with backoff before giving up — the settle
    /// therefore lands only after the retries are exhausted, still well within the
    /// generous timeout below, and still having synced nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wait_for_sync_settles_even_when_backfill_fails() {
        let (_b_dir, ndb_b) = temp_ndb();
        let session = Session::new(ndb_b.clone());
        // Nothing listens on port 1, so the backfill's connect is refused fast.
        let dead = "ws://127.0.0.1:1".to_string();
        let live = Filter::new().kinds([UNRELATED_KIND as u64]).build();
        let history = Filter::new().kinds([KIND as u64]).build();
        session.set_subscription("dead", dead, vec![live], vec![history]);

        tokio::time::timeout(Duration::from_secs(10), session.wait_for_sync())
            .await
            .expect("settle must resolve even when the backfill fails");
        assert_eq!(
            count_kind(&ndb_b, KIND as u64),
            0,
            "a failed backfill syncs nothing"
        );

        // The settle doesn't say the backfill gave up; the status does.
        let s = session.backfill_status().borrow().clone();
        assert!(s.failed, "a backfill that gave up must flag failed: {s:?}");
        assert_eq!(s.done, s.started, "{s:?}");
        assert_eq!(s.fetched, 0, "{s:?}");
    }

    fn assert_send<T: Send>(_: T) {}

    /// Compile-time check: a backfill reporting into the shared status stays
    /// `Send`, so the loop can still `tokio::spawn` it on the multi-thread
    /// runtime. Never called — it only has to type-check.
    #[allow(dead_code)]
    fn backfill_with_status_is_send(
        ndb: Ndb,
        cancel: oneshot::Receiver<()>,
        progress: &BackfillProgress,
    ) {
        assert_send(backfill(ndb, String::new(), Vec::new(), cancel, progress));
    }

    /// A [`Session`]'s live `REQ` delivers an event published *after* it
    /// subscribes: B holds an open live subscription, A publishes, B receives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_req_delivers_new_events() {
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();

        // B opens a live subscription (no history) and settles the connect.
        let (_b_dir, ndb_b) = temp_ndb();
        let session_b = Session::new(ndb_b.clone());
        let live = Filter::new().kinds([KIND as u64]).build();
        session_b.set_subscription("live", url.clone(), vec![live], vec![]);
        session_b.wait_for_sync().await;

        // A publishes a fresh event to the relay.
        let (_a_dir, ndb_a) = temp_ndb();
        let (note_json, note_id) = signed_note(KIND, "live delivery");
        ndb_a
            .process_client_event(&event_frame(&note_json))
            .expect("ingest on A");
        let session_a = Session::new(ndb_a.clone());
        session_a.publish(note_json, vec![url.clone()]);

        assert!(
            await_note(&ndb_b, note_id, KIND as u64, Duration::from_secs(10)).await,
            "B's live subscription should receive A's new event"
        );
        relay.shutdown();
    }

    /// Wait until nostrdb lists `url` among note `id`'s seen-on relays. The relay
    /// write lands on nostrdb's writer thread with no subscription wakeup, so this
    /// polls briefly under a `timeout` backstop. Trailing slashes are ignored, since
    /// the pool canonicalizes urls.
    async fn await_seen_on(ndb: &Ndb, id: [u8; 32], url: &str, timeout: Duration) -> bool {
        let want = url.trim_end_matches('/');
        tokio::time::timeout(timeout, async {
            loop {
                {
                    let txn = Transaction::new(ndb).expect("txn");
                    if let Ok(note) = ndb.get_note_by_id(&txn, &id) {
                        if note
                            .relays(&txn)
                            .any(|seen| seen.trim_end_matches('/') == want)
                        {
                            return true;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or(false)
    }

    /// An event delivered over a [`Session`]'s live `REQ` is recorded as seen on
    /// the relay that sent it, so a fan-out that skips already-seen relays won't
    /// echo it straight back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_event_is_recorded_as_seen_on_its_relay() {
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();

        let (_b_dir, ndb_b) = temp_ndb();
        let session_b = Session::new(ndb_b.clone());
        let live = Filter::new().kinds([KIND as u64]).build();
        session_b.set_subscription("live", url.clone(), vec![live], vec![]);
        session_b.wait_for_sync().await;

        let (note_json, note_id) = signed_note(KIND, "seen on live");
        relay_ndb
            .process_client_event(&event_frame(&note_json))
            .expect("seed relay");
        // Seeded straight into the relay's db, so re-publish it over the wire for
        // the live REQ to see it.
        let (_a_dir, ndb_a) = temp_ndb();
        let session_a = Session::new(ndb_a.clone());
        session_a.publish(note_json, vec![url.clone()]);

        assert!(
            await_note(&ndb_b, note_id, KIND as u64, Duration::from_secs(10)).await,
            "B should receive the event"
        );
        assert!(
            await_seen_on(&ndb_b, note_id, &url, Duration::from_secs(5)).await,
            "B should record the relay as a seen-on source of the live event"
        );
        relay.shutdown();
    }

    /// An event history-backfilled via negentropy (`sync_into`) is recorded as
    /// seen on the relay it was pulled from.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfilled_event_is_recorded_as_seen_on_its_relay() {
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();
        let ids = seed_events(&relay_ndb, KIND, 3);

        let (_b_dir, ndb_b) = temp_ndb();
        let session_b = Session::new(ndb_b.clone());
        let live_none = Filter::new().kinds([UNRELATED_KIND as u64]).build();
        let history = Filter::new().kinds([KIND as u64]).build();
        session_b.set_subscription("backfill", url.clone(), vec![live_none], vec![history]);
        session_b.wait_for_sync().await;

        for id in ids {
            assert!(
                await_seen_on(&ndb_b, id, &url, Duration::from_secs(5)).await,
                "a backfilled event should record the relay as seen-on"
            );
        }
        relay.shutdown();
    }

    /// A note this session publishes is recorded as seen on the relay once the
    /// relay acknowledges it with `OK`, so a later catch-up fan-out (which skips
    /// relays a note was seen on) doesn't publish it again on every run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn published_event_is_recorded_as_seen_on_after_ok() {
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();

        let (_a_dir, ndb_a) = temp_ndb();
        let (note_json, note_id) = signed_note(KIND, "ack me");
        ndb_a
            .process_client_event(&event_frame(&note_json))
            .expect("ingest on A");
        assert!(await_note(&ndb_a, note_id, KIND as u64, Duration::from_secs(5)).await);

        let session_a = Session::new(ndb_a.clone());
        session_a.publish(note_json, vec![url.clone()]);

        assert!(
            await_seen_on(&ndb_a, note_id, &url, Duration::from_secs(10)).await,
            "A should record the relay as seen-on once it OKs the publish"
        );
        relay.shutdown();
    }

    /// A relay reconnect re-runs the subscription's history backfill. The live
    /// `REQ` re-sent on reconnect replays only a relay's newest stored events
    /// (strfry caps it at 500), so events published while we were away, beyond
    /// that slice, reach us only through the backfill. Here the live filter can't
    /// match the event at all, so only a re-run backfill can deliver it: it's added
    /// to the relay after B's first backfill settled, then the relay drops B.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_reruns_history_backfill() {
        let (_relay_dir, relay_ndb) = temp_ndb();
        let relay = spawn_relay(relay_ndb.clone());
        let url = relay.url();

        let (_b_dir, ndb_b) = temp_ndb();
        let session_b = Session::new(ndb_b.clone());
        let live = Filter::new().kinds([UNRELATED_KIND as u64]).build();
        let history = Filter::new().kinds([KIND as u64]).build();
        session_b.set_subscription("sub", url.clone(), vec![live], vec![history]);
        session_b.wait_for_sync().await;

        // Prove B's live connection is up (its first `Opened` has happened), so
        // the drop below is a reconnect rather than the initial connect.
        let (probe_json, probe_id) = signed_note(UNRELATED_KIND, "connected?");
        let (_a_dir, ndb_a) = temp_ndb();
        let session_a = Session::new(ndb_a.clone());
        session_a.publish(probe_json, vec![url.clone()]);
        assert!(
            await_note(
                &ndb_b,
                probe_id,
                UNRELATED_KIND as u64,
                Duration::from_secs(10)
            )
            .await,
            "B's live connection should be up"
        );

        // Published while B "wasn't looking": after its backfill, and invisible to
        // its live filter.
        let (missed_json, missed_id) = signed_note(KIND, "missed while away");
        relay_ndb
            .process_client_event(&event_frame(&missed_json))
            .expect("seed relay");
        assert!(await_note(&relay_ndb, missed_id, KIND as u64, Duration::from_secs(5)).await);

        relay.drop_connections();

        // The pool reconnects on its keepalive tick, then the backfill re-runs.
        assert!(
            await_note(&ndb_b, missed_id, KIND as u64, Duration::from_secs(30)).await,
            "a reconnect should re-run the backfill and fetch the missed event"
        );
        relay.shutdown();
    }

    /// PROBE (ignored, hits the network): run a raw NIP-77 session against the
    /// real relay to observe how strfry's `maxSyncEvents` cap manifests — whether
    /// it streams partial `need` ids across rounds and then `NEG-ERR`s, or refuses
    /// up front. Prints a frame-by-frame trace. Run with:
    ///   PROBE_RELAY=ws://relay.jb55.com cargo test -p nostrdb_net --lib \
    ///     probe_real_relay_negentropy -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "hits the real network relay; set PROBE_RELAY"]
    async fn probe_real_relay_negentropy() {
        use futures_util::SinkExt;
        use negentropy::{Negentropy, NegentropyStorageVector};
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::Message;

        let url = std::env::var("PROBE_RELAY").unwrap_or_else(|_| "ws://relay.jb55.com".into());
        let filter_json =
            std::env::var("PROBE_FILTER").unwrap_or_else(|_| r#"{"kinds":[1080]}"#.into());
        eprintln!("probe: {url} filter={filter_json}");

        let (mut ws, _) = connect_async(&url).await.expect("connect");

        // Empty local set: we hold nothing, so `need` is the relay's whole match.
        let mut storage = NegentropyStorageVector::new();
        storage.seal().expect("seal");
        let mut neg = Negentropy::owned(storage, 0).expect("neg");
        let initial = neg.initiate().expect("initiate");
        ws.send(Message::Text(format!(
            r#"["NEG-OPEN","probe",{filter_json},"{}"]"#,
            hex::encode(&initial)
        )))
        .await
        .expect("send NEG-OPEN");

        let mut round = 0u32;
        let mut total_need = 0usize;
        let started = std::time::Instant::now();
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(30), ws.next())
                .await
                .expect("frame timeout")
                .expect("stream end")
                .expect("ws error");
            let Message::Text(text) = msg else { continue };
            let frame: Vec<serde_json::Value> = serde_json::from_str(&text).expect("json");
            match frame.first().and_then(|v| v.as_str()) {
                Some("NEG-MSG") => {
                    round += 1;
                    let payload = frame.get(2).and_then(|v| v.as_str()).expect("payload");
                    let bytes = hex::decode(payload).expect("hex");
                    let mut have = Vec::new();
                    let mut need = Vec::new();
                    let reply = neg
                        .reconcile_with_ids(&bytes, &mut have, &mut need)
                        .expect("reconcile");
                    total_need += need.len();
                    eprintln!(
                        "round {round}: +{} need (total {total_need}), +{} have, more={}, {:?} elapsed",
                        need.len(),
                        have.len(),
                        reply.is_some(),
                        started.elapsed()
                    );
                    match reply {
                        Some(reply) => ws
                            .send(Message::Text(format!(
                                r#"["NEG-MSG","probe","{}"]"#,
                                hex::encode(&reply)
                            )))
                            .await
                            .expect("send NEG-MSG"),
                        None => {
                            eprintln!("CONVERGED: {total_need} need in {round} rounds");
                            break;
                        }
                    }
                }
                Some("NEG-ERR") => {
                    eprintln!("NEG-ERR after {round} rounds / {total_need} need: {}", text);
                    break;
                }
                other => {
                    eprintln!("other frame {other:?}: {}", &text[..text.len().min(200)]);
                    break;
                }
            }
        }
    }

    /// E2E (ignored, hits the network): a [`Session`] backfills a filter whose
    /// match exceeds the relay's per-sync cap into a fresh db, driven entirely
    /// through the public API (`set_subscription` + `wait_for_sync`). Proves the
    /// windowed backfill bisects under the cap and settles with the whole set
    /// queryable. Run with (the default relay has tens of thousands of kind-1080
    /// PNS envelopes behind `maxSyncEvents = 5000`):
    ///   PROBE_RELAY=ws://relay.jb55.com cargo test -p nostrdb_net --lib \
    ///     session_backfills_past_relay_cap -- --ignored --nocapture
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "hits the real network relay; set PROBE_RELAY"]
    async fn session_backfills_past_relay_cap() {
        let url = std::env::var("PROBE_RELAY").unwrap_or_else(|_| "ws://relay.jb55.com".into());
        let kind: u64 = std::env::var("PROBE_KIND")
            .ok()
            .and_then(|k| k.parse().ok())
            .unwrap_or(1080);
        eprintln!("e2e: {url} kind={kind}");

        let (_dir, ndb) = temp_ndb();
        let session = Session::new(ndb.clone());
        // Same filter for live + history; the history leg is what backfills the
        // capped set, the live leg just keeps the sub open (harmless replay).
        let hist = Filter::new().kinds([kind]).build();
        let live = Filter::new().kinds([kind]).build();
        session.set_subscription("probe", url, vec![live], vec![hist]);

        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(180), session.wait_for_sync())
            .await
            .expect("backfill should settle within 180s");
        let n = count_kind(&ndb, kind);
        eprintln!("settled: {n} kind-{kind} events in {:?}", started.elapsed());

        assert!(
            n > 5000,
            "windowed backfill should sync past the relay's 5000-event cap; got {n}"
        );
    }
}
