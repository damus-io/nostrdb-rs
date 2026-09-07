use crate::bindings;
use crate::Note;
use std::sync::Arc;

/// The subscription callback, as stored by [`Config`].
///
/// `Fn`, `Send` and `Sync` rather than `FnMut`: a [`Config`] is [`Clone`], so
/// one closure can back several [`crate::Ndb`]s, and each of those has its own
/// writer thread calling it. See [`Config::set_sub_callback`].
pub(crate) type SubCallback = dyn Fn(u64) + Send + Sync + 'static;

/// The context pointer nostrdb hands back to a trampoline is a thin `void *`,
/// so the (fat) trait object gets one level of indirection to point at.
pub(crate) type SubCallbackCtx = Box<SubCallback>;

/// The ingest filter, as stored by [`Config`]. See
/// [`Config::set_ingest_filter`].
///
/// `Sync` is not optional here the way it arguably is for [`SubCallback`]:
/// nostrdb runs `ingester_threads` ingesters (one per core by default), all of
/// which can be in the filter at the same time.
pub(crate) type IngestFilter = dyn Fn(&Note<'_>) -> bool + Send + Sync + 'static;

/// See [`SubCallbackCtx`].
pub(crate) type IngestFilterCtx = Box<IngestFilter>;

/// Configuration for opening an [`crate::Ndb`].
///
/// Note that this is [`Clone`] but deliberately not [`Copy`]: it can own
/// callbacks, and copying a raw owning pointer is how you get a double free.
#[derive(Clone)]
pub struct Config {
    pub config: bindings::ndb_config,

    /// Keeps the closure behind [`bindings::ndb_config::sub_cb_ctx`] alive.
    ///
    /// The raw pointer nostrdb holds is a *borrow* of this allocation, so
    /// ownership never leaves Rust and there is nothing for the FFI layer to
    /// free. Whoever opens a database clones this into the [`crate::Ndb`], so
    /// the closure outlives the writer thread that calls it.
    pub(crate) sub_cb: Option<Arc<SubCallbackCtx>>,

    /// Keeps the closure behind [`bindings::ndb_config::filter_context`]
    /// alive, on the same borrow-not-transfer terms as [`Self::sub_cb`].
    pub(crate) ingest_filter: Option<Arc<IngestFilterCtx>>,
}

impl Default for Config {
    fn default() -> Self {
        Config::new()
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("flags", &self.config.flags)
            .field("ingester_threads", &self.config.ingester_threads)
            .field("mapsize", &self.config.mapsize)
            .field("sub_cb", &self.sub_cb.is_some())
            .field("ingest_filter", &self.ingest_filter.is_some())
            .finish()
    }
}

impl Config {
    pub fn new() -> Self {
        let mut config = bindings::ndb_config {
            filter_context: std::ptr::null_mut(),
            sub_cb: None,
            sub_cb_ctx: std::ptr::null_mut(),
            ingest_filter: None,
            flags: 0,
            ingester_threads: 0,
            writer_scratch_buffer_size: 1024 * 1024,
            mapsize: 0,
        };

        unsafe {
            bindings::ndb_default_config(&mut config);
        }

        Config {
            config,
            sub_cb: None,
            ingest_filter: None,
        }
    }

    //
    pub fn set_flags(mut self, flags: i32) -> Self {
        self.config.flags = flags;
        self
    }

    pub fn skip_validation(mut self, skip: bool) -> Self {
        let skip_note_verify: i32 = 1 << 1;

        if skip {
            self.config.flags |= skip_note_verify;
        } else {
            self.config.flags &= !skip_note_verify;
        }

        self
    }

    /// Set a callback to be notified on updated subscriptions. The function
    /// will be called with the corresponsing subscription id.
    ///
    /// The closure is called from a thread nostrdb spawns itself (the writer
    /// thread), which is why it must be `Send`. It is `Fn + Sync` rather than
    /// `FnMut` because a [`Config`] is [`Clone`] and may open more than one
    /// [`crate::Ndb`], each with its own writer thread; use interior
    /// mutability if the callback needs state.
    ///
    /// The closure is kept alive by the [`Config`] and by every [`crate::Ndb`]
    /// opened from it, and is dropped once the last of those goes away.
    ///
    /// A closure that is not `Send` is rejected, because nostrdb would call it
    /// on a thread it owns:
    ///
    /// ```compile_fail
    /// use nostrdb::Config;
    /// use std::rc::Rc;
    ///
    /// let counter = Rc::new(42);
    /// Config::new().set_sub_callback(move |_sub_id| {
    ///     let _ = &counter;
    /// });
    /// ```
    pub fn set_sub_callback<F>(mut self, closure: F) -> Self
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        let ctx: Arc<SubCallbackCtx> = Arc::new(Box::new(closure));

        // nostrdb only borrows this for as long as the database is open; the
        // Arc above (cloned into the Ndb) is what actually owns it.
        self.config.sub_cb_ctx = Arc::as_ptr(&ctx) as *mut ::std::os::raw::c_void;
        self.config.sub_cb = Some(sub_callback_trampoline);
        self.sub_cb = Some(ctx);
        self
    }

    /// Decide what is allowed into the database, at nostrdb's own ingest
    /// boundary rather than at every call site that writes.
    ///
    /// Returning `true` accepts the note, `false` rejects it. A rejected note
    /// is dropped before anything is written, so it never becomes queryable
    /// and never reaches the writer thread.
    ///
    /// This is the gate a consumer wants when it ingests from relays it does
    /// not control. Nothing else in the ingest path filters: a relay is free
    /// to answer a filtered `REQ` with a validly-signed event from any other
    /// pubkey, and subscriptions gate *notifications*, not writes.
    ///
    /// # The filter runs before signature verification
    ///
    /// nostrdb calls this from `ndb_ingester_process_note`, ahead of
    /// `ndb_note_verify`. Two things follow, and neither is obvious:
    ///
    /// - **The note is unverified here**, so [`Note::pubkey`] is the pubkey the
    ///   note *claims*. Pinning an author is still sound, because a note this
    ///   filter accepts must then pass verification of a signature over that
    ///   same claimed pubkey before it lands — an attacker can claim your
    ///   pubkey, but cannot produce the signature to go with it. The one
    ///   exception is rumors (unsigned notes unwrapped from a giftwrap or
    ///   seal), which skip verification entirely; their authenticity comes
    ///   from the verified outer layer, not from this pubkey.
    /// - **Rejecting is cheaper than accepting**, because `false` skips the
    ///   secp256k1 verification the note would otherwise have paid for.
    ///
    /// # Threading
    ///
    /// Called concurrently from every ingester thread
    /// ([`Config::set_ingester_threads`], one per core by default), which is
    /// why the bound is `Fn + Send + Sync`. Use interior mutability for state.
    /// Keep it cheap: it is on the path of every note the database ingests.
    ///
    /// A panic here crosses an `extern "C"` boundary and aborts the process.
    /// That fails closed — nothing is written — but prefer returning `false`.
    ///
    /// Note this cannot switch signature verification *off*; the C API's
    /// `NDB_INGEST_SKIP_VALIDATION` is deliberately not reachable from here, so
    /// that "accept this note" and "stop checking signatures" are not adjacent
    /// choices. For bulk imports where that is wanted, see
    /// [`Config::skip_validation`].
    ///
    /// # Example
    ///
    /// Pin a single publisher, so nothing else can land whatever a relay says:
    ///
    /// ```no_run
    /// use nostrdb::{Config, Ndb};
    ///
    /// let publisher: [u8; 32] = [0xaa; 32];
    /// let config = Config::new().set_ingest_filter(move |note| note.pubkey() == &publisher);
    /// let ndb = Ndb::new("target/testdbs/doc_ingest_filter", &config).unwrap();
    /// ```
    ///
    /// A closure that is not `Send + Sync` is rejected, since the ingester
    /// threads share it:
    ///
    /// ```compile_fail
    /// use nostrdb::Config;
    /// use std::cell::Cell;
    ///
    /// let seen = Cell::new(0);
    /// Config::new().set_ingest_filter(move |_note| {
    ///     seen.set(seen.get() + 1);
    ///     true
    /// });
    /// ```
    pub fn set_ingest_filter<F>(mut self, filter: F) -> Self
    where
        F: Fn(&Note<'_>) -> bool + Send + Sync + 'static,
    {
        let ctx: Arc<IngestFilterCtx> = Arc::new(Box::new(filter));

        // Borrowed by nostrdb for as long as the database is open; the Arc
        // (cloned into the Ndb) is what owns it. See `set_sub_callback`.
        self.config.filter_context = Arc::as_ptr(&ctx) as *mut ::std::os::raw::c_void;
        self.config.ingest_filter = Some(ingest_filter_trampoline);
        self.ingest_filter = Some(ctx);
        self
    }

    pub fn set_mapsize(mut self, bytes: usize) -> Self {
        self.config.mapsize = bytes;
        self
    }

    pub fn set_ingester_threads(mut self, threads: i32) -> Self {
        self.config.ingester_threads = threads;
        self
    }

    // Internal method to get a raw pointer to the config, used in Ndb
    pub fn as_ptr(&self) -> *const bindings::ndb_config {
        &self.config
    }
}

extern "C" fn sub_callback_trampoline(ctx: *mut ::std::os::raw::c_void, subid: u64) {
    // SAFETY: `ctx` is the pointer installed by `set_sub_callback`, which
    // borrows an `Arc<SubCallbackCtx>` kept alive by the `Ndb` that nostrdb is
    // calling us on behalf of. It is shared, never uniquely borrowed, so
    // concurrent calls from several writer threads are fine.
    unsafe {
        let closure_ptr = ctx as *const SubCallbackCtx;
        assert!(!closure_ptr.is_null());
        (*closure_ptr)(subid);
    }
}

extern "C" fn ingest_filter_trampoline(
    ctx: *mut ::std::os::raw::c_void,
    note: *mut bindings::ndb_note,
) -> bindings::ndb_ingest_filter_action {
    // SAFETY: `ctx` is the pointer installed by `set_ingest_filter`, borrowing
    // an `Arc<IngestFilterCtx>` the `Ndb` keeps alive. `note` is owned by the
    // ingester for the duration of this call, and `Note` only borrows it.
    unsafe {
        let filter_ptr = ctx as *const IngestFilterCtx;
        assert!(!filter_ptr.is_null());
        assert!(!note.is_null());

        let note = Note::new_unowned(&*note);

        if (*filter_ptr)(&note) {
            bindings::ndb_ingest_filter_action_NDB_INGEST_ACCEPT
        } else {
            // Note that REJECT also skips verification, so this is the cheap
            // path. We never return SKIP_VALIDATION: accepting a note must
            // never be able to turn signature checking off.
            bindings::ndb_ingest_filter_action_NDB_INGEST_REJECT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_util, Ndb};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The context nostrdb hands to the writer thread is shared across
    /// threads, so it has to be both `Send` and `Sync`. This is the bound
    /// `set_sub_callback` exists to enforce; see the `compile_fail` doctest on
    /// [`Config::set_sub_callback`] for the negative case.
    #[test]
    fn sub_callback_ctx_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<SubCallbackCtx>>();
    }

    /// Signals when the closure it lives in is dropped.
    pub(super) struct DropFlag(pub(super) Arc<AtomicUsize>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The closure must outlive every `Ndb` opened from the config, and must
    /// be dropped exactly once when the last of them goes away.
    #[test]
    fn shared_callback_outlives_config_and_drops_with_last_ndb() {
        let db_a = "target/testdbs/config_shared_cb_a";
        let db_b = "target/testdbs/config_shared_cb_b";
        test_util::cleanup_db(db_a);
        test_util::cleanup_db(db_b);

        let drops = Arc::new(AtomicUsize::new(0));
        let flag = DropFlag(drops.clone());

        let config = Config::new()
            .set_mapsize(1024 * 1024 * 32)
            .set_sub_callback(move |_sub_id| {
                let _ = &flag;
            });

        let ndb_a = Ndb::new(db_a, &config).expect("ndb a");
        let ndb_b = Ndb::new(db_b, &config).expect("ndb b");

        // Dropping the config must not free a closure the databases still use.
        drop(config);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(ndb_a);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "freed while ndb_b still open"
        );

        drop(ndb_b);
        // Dropped exactly once, by the last holder. Under the old
        // `Box::into_raw` scheme the user closure was never freed at all.
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    /// Cloning a config shares one closure rather than duplicating ownership
    /// of it, so there is nothing to double free.
    #[test]
    fn cloning_a_config_shares_the_closure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();

        let config = Config::new().set_sub_callback(move |_sub_id| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let cloned = config.clone();

        // Same allocation, so the same `sub_cb_ctx` goes to nostrdb.
        assert_eq!(config.config.sub_cb_ctx, cloned.config.sub_cb_ctx);
        assert_eq!(Arc::strong_count(&calls), 2);

        drop(cloned);
        assert_eq!(Arc::strong_count(&calls), 2);

        drop(config);
        assert_eq!(Arc::strong_count(&calls), 1);
    }
}

#[cfg(test)]
mod ingest_filter_tests {
    use super::*;
    use crate::{test_util, Filter, Ndb, NoteBuilder, Transaction};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PINNED_SECKEY: [u8; 32] = [0x01; 32];
    const OTHER_SECKEY: [u8; 32] = [0x02; 32];

    /// A validly-signed kind-1 note from `seckey`, wrapped in the relay
    /// `["EVENT", ..]` message `process_event` expects. Also returns the
    /// note's pubkey and id so tests can assert on them.
    fn signed_event(seckey: &[u8; 32], content: &str) -> (String, [u8; 32], [u8; 32]) {
        let note = NoteBuilder::new()
            .kind(1)
            .content(content)
            .created_at(42)
            .sign(seckey)
            .build()
            .expect("note builds");

        let pubkey = *note.pubkey();
        let id = *note.id();
        let json = format!(r#"["EVENT","s",{}]"#, note.json().expect("note json"));

        (json, pubkey, id)
    }

    /// Blocks until the ingest filter has been called `n` times, so a test can
    /// tell "rejected" apart from "not looked at yet". A rejected note is
    /// dropped before any write is queued, so once the filter has seen it
    /// there is nothing further to wait for.
    fn await_filter_calls(calls: &Arc<AtomicUsize>, n: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while calls.load(Ordering::SeqCst) < n {
            assert!(
                std::time::Instant::now() < deadline,
                "ingest filter saw {} notes, expected {n}",
                calls.load(Ordering::SeqCst)
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// One ingester thread, so the queue is FIFO and a test can rely on note
    /// N being fully processed before note N+1 is written.
    fn pinned_config(pinned: [u8; 32], calls: Arc<AtomicUsize>) -> Config {
        Config::new()
            .set_ingester_threads(1)
            .set_ingest_filter(move |note| {
                calls.fetch_add(1, Ordering::SeqCst);
                note.pubkey() == &pinned
            })
    }

    /// The point of the whole thing: a relay can hand us a perfectly valid
    /// note from someone else, and it must not land.
    #[tokio::test]
    async fn ingest_filter_pins_an_author() {
        let db = "target/testdbs/ingest_filter_pins_author";
        test_util::cleanup_db(db);

        {
            let (_, pinned_pubkey, _) = signed_event(&PINNED_SECKEY, "probe");
            let calls = Arc::new(AtomicUsize::new(0));
            let config = pinned_config(pinned_pubkey, calls.clone());

            let ndb = Ndb::new(db, &config).expect("ndb");

            let (wanted, _, wanted_id) = signed_event(&PINNED_SECKEY, "from the pinned author");
            let (unwanted, other_pubkey, unwanted_id) =
                signed_event(&OTHER_SECKEY, "from someone else");
            assert_ne!(pinned_pubkey, other_pubkey);

            let sub = ndb
                .subscribe(&[Filter::new().kinds(vec![1]).build()])
                .expect("sub");
            let waiter = ndb.wait_for_all_notes(sub, 1);

            // Unwanted first: with one ingester thread it is filtered before
            // the wanted note is even looked at.
            ndb.process_event(&unwanted).expect("process unwanted");
            ndb.process_event(&wanted).expect("process wanted");

            waiter.await.expect("pinned note lands");
            await_filter_calls(&calls, 2);

            let txn = Transaction::new(&ndb).expect("txn");
            let notes = ndb
                .query(&txn, &[Filter::new().kinds(vec![1]).build()], 10)
                .expect("query");

            assert_eq!(notes.len(), 1, "only the pinned author's note is stored");
            assert_eq!(notes[0].note.id(), &wanted_id);
            assert_eq!(notes[0].note.pubkey(), &pinned_pubkey);

            // And the rejected one is not reachable by id either.
            assert!(ndb.get_note_by_id(&txn, &unwanted_id).is_err());

            // Control: the same two events, with no filter, both land. So the
            // assertion above is the filter's doing and not a bad fixture.
            let control_db = "target/testdbs/ingest_filter_pins_author_control";
            test_util::cleanup_db(control_db);
            {
                let control = Ndb::new(control_db, &Config::new()).expect("control ndb");
                let sub = control
                    .subscribe(&[Filter::new().kinds(vec![1]).build()])
                    .expect("sub");
                let waiter = control.wait_for_all_notes(sub, 2);

                control.process_event(&unwanted).expect("process unwanted");
                control.process_event(&wanted).expect("process wanted");
                waiter.await.expect("both notes land unfiltered");

                let txn = Transaction::new(&control).expect("txn");
                let notes = control
                    .query(&txn, &[Filter::new().kinds(vec![1]).build()], 10)
                    .expect("query");
                assert_eq!(
                    notes.len(),
                    2,
                    "both events are ingestable without a filter"
                );
            }
            test_util::cleanup_db(control_db);
        }

        test_util::cleanup_db(db);
    }

    /// A filter that rejects everything stores nothing at all.
    #[tokio::test]
    async fn ingest_filter_rejecting_everything_ingests_nothing() {
        let db = "target/testdbs/ingest_filter_rejects_all";
        test_util::cleanup_db(db);

        {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();

            let config = Config::new()
                .set_ingester_threads(1)
                .set_ingest_filter(move |_note| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    false
                });

            let ndb = Ndb::new(db, &config).expect("ndb");

            let (a, _, _) = signed_event(&PINNED_SECKEY, "a");
            let (b, _, _) = signed_event(&OTHER_SECKEY, "b");
            ndb.process_event(&a).expect("process a");
            ndb.process_event(&b).expect("process b");

            // Once the filter has seen both, both are already dropped: REJECT
            // returns before anything is queued to the writer.
            await_filter_calls(&calls, 2);

            let txn = Transaction::new(&ndb).expect("txn");
            let notes = ndb
                .query(&txn, &[Filter::new().kinds(vec![1]).build()], 10)
                .expect("query");
            assert!(notes.is_empty(), "nothing should have been ingested");
        }

        test_util::cleanup_db(db);
    }

    /// The filter runs *before* signature verification, so it sees the pubkey
    /// a note merely claims. This asserts the half that makes an author pin
    /// sound anyway: accepting a note does not skip verification, so a forged
    /// claim to the pinned pubkey still fails to land.
    #[tokio::test]
    async fn accepted_note_with_bad_signature_still_does_not_land() {
        let db = "target/testdbs/ingest_filter_bad_sig";
        test_util::cleanup_db(db);

        {
            let (_, pinned_pubkey, _) = signed_event(&PINNED_SECKEY, "probe");
            let calls = Arc::new(AtomicUsize::new(0));
            let seen_pinned = Arc::new(AtomicUsize::new(0));

            let counter = calls.clone();
            let seen = seen_pinned.clone();
            let config = Config::new()
                .set_ingester_threads(1)
                .set_ingest_filter(move |note| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if note.pubkey() == &pinned_pubkey {
                        seen.fetch_add(1, Ordering::SeqCst);
                        true
                    } else {
                        false
                    }
                });

            let ndb = Ndb::new(db, &config).expect("ndb");

            // Claims the pinned pubkey, but the signature over it is garbage.
            let (good, _, good_id) = signed_event(&PINNED_SECKEY, "forge me");
            let forged = good.replace(
                &good[good.find(r#""sig":""#).expect("sig field") + 7..][..64],
                &"f".repeat(64),
            );
            assert_ne!(forged, good, "the signature was actually corrupted");

            let sub = ndb
                .subscribe(&[Filter::new().kinds(vec![1]).build()])
                .expect("sub");
            let waiter = ndb.wait_for_all_notes(sub, 1);

            // The forged note goes first. One ingester thread means it is
            // fully processed — filtered, then verified and dropped — before
            // the genuine note that follows it is written.
            ndb.process_event(&forged).expect("process forged");

            let (genuine, _, genuine_id) = signed_event(&PINNED_SECKEY, "genuine");
            ndb.process_event(&genuine).expect("process genuine");

            waiter.await.expect("genuine note lands");
            await_filter_calls(&calls, 2);

            // The filter accepted the forged note on its claimed pubkey...
            assert_eq!(
                seen_pinned.load(Ordering::SeqCst),
                2,
                "filter accepted both notes on the claimed pubkey"
            );

            // ...and verification threw it out anyway.
            let txn = Transaction::new(&ndb).expect("txn");
            let notes = ndb
                .query(&txn, &[Filter::new().kinds(vec![1]).build()], 10)
                .expect("query");
            assert_eq!(notes.len(), 1, "only the genuinely signed note is stored");
            assert_eq!(notes[0].note.id(), &genuine_id);
            assert!(
                ndb.get_note_by_id(&txn, &good_id).is_err(),
                "a note the filter accepted but secp rejected must not be stored"
            );
        }

        test_util::cleanup_db(db);
    }

    /// Same ownership contract as the sub callback: held by every `Ndb` that
    /// uses it, dropped once when the last goes away.
    #[test]
    fn ingest_filter_drops_with_the_last_ndb() {
        let db_a = "target/testdbs/ingest_filter_drop_a";
        let db_b = "target/testdbs/ingest_filter_drop_b";
        test_util::cleanup_db(db_a);
        test_util::cleanup_db(db_b);

        let drops = Arc::new(AtomicUsize::new(0));
        let flag = super::tests::DropFlag(drops.clone());

        let config = Config::new()
            .set_mapsize(1024 * 1024 * 32)
            .set_ingest_filter(move |_note| {
                let _ = &flag;
                true
            });

        let ndb_a = Ndb::new(db_a, &config).expect("ndb a");
        let ndb_b = Ndb::new(db_b, &config).expect("ndb b");

        drop(config);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(ndb_a);
        assert_eq!(drops.load(Ordering::SeqCst), 0, "freed while ndb_b is open");

        drop(ndb_b);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        test_util::cleanup_db(db_a);
        test_util::cleanup_db(db_b);
    }
}
