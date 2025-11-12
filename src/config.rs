//! Configuration helpers around `ndb_config` (see the mdBook API Tour).
use crate::bindings;

/// High-level builder for `ndb_config` (see the nostrdb mdBook *Getting Started* chapter).
#[derive(Copy, Clone)]
pub struct Config {
    pub config: bindings::ndb_config,
}

impl Default for Config {
    fn default() -> Self {
        Config::new()
    }
}

impl Config {
    /// Create a config populated with nostrdb defaults.
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

        Config { config }
    }

    /// Set raw flag bits (advanced). Prefer the dedicated helpers when possible.
    pub fn set_flags(mut self, flags: i32) -> Self {
        self.config.flags = flags;
        self
    }

    /// Skip signature verification during ingestion. Mirror of `NDB_FLAG_SKIP_NOTE_VERIFY`.
    pub fn skip_validation(mut self, skip: bool) -> Self {
        let skip_note_verify: i32 = 1 << 1;

        if skip {
            self.config.flags |= skip_note_verify;
        } else {
            self.config.flags &= !skip_note_verify;
        }

        self
    }

    /// Convenience alias for [`Config::skip_validation`].
    pub fn skip_verification(self, skip: bool) -> Self {
        self.skip_validation(skip)
    }

    /// Set a callback to be notified on updated subscriptions. The function
    /// will be called with the corresponsing subscription id.
    pub fn set_sub_callback<F>(mut self, closure: F) -> Self
    where
        F: FnMut(u64) + 'static,
    {
        // Box the closure to ensure it has a stable address.
        let boxed_closure: Box<dyn FnMut(u64)> = Box::new(closure);

        // Convert it to a raw pointer to store in sub_cb_ctx.
        let ctx_ptr = Box::into_raw(Box::new(boxed_closure)) as *mut ::std::os::raw::c_void;

        self.config.sub_cb = Some(sub_callback_trampoline);
        self.config.sub_cb_ctx = ctx_ptr;
        self
    }

    /// Configure LMDB map size in bytes. Must be large enough to hold your dataset.
    pub fn set_mapsize(mut self, bytes: usize) -> Self {
        self.config.mapsize = bytes;
        self
    }

    /// Number of ingest worker threads (see mdBook *Architecture → Ingestion*).
    pub fn set_ingester_threads(mut self, threads: i32) -> Self {
        self.config.ingester_threads = threads;
        self
    }

    /// # Internal
    /// Raw pointer accessor used by `Ndb::open`.
    pub fn as_ptr(&self) -> *const bindings::ndb_config {
        &self.config
    }
}

extern "C" fn sub_callback_trampoline(ctx: *mut ::std::os::raw::c_void, subid: u64) {
    unsafe {
        // Convert the raw pointer back into a reference to our closure.
        // We know this pointer was created by Box::into_raw in `set_sub_callback_rust`.
        let closure_ptr = ctx as *mut Box<dyn FnMut(u64)>;
        assert!(!closure_ptr.is_null());
        let closure = &mut *closure_ptr;
        closure(subid);
    }
}
