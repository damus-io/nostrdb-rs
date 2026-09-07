use crate::bindings;
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
    struct DropFlag(Arc<AtomicUsize>);

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
