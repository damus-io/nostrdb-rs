use crate::bindings;

#[derive(Copy, Clone)]
pub struct Config {
    pub config: bindings::ndb_config,
    // We add a flag to know if we've installed a Rust closure so we can clean it up in Drop.
    is_rust_closure: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config::new()
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
            mapsize: 0,
        };

        unsafe {
            bindings::ndb_default_config(&mut config);
        }

        let is_rust_closure = false;
        Config {
            config,
            is_rust_closure,
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

    /// Set a callback for when we have  
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
        self.is_rust_closure = true;
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
    unsafe {
        // Convert the raw pointer back into a reference to our closure.
        // We know this pointer was created by Box::into_raw in `set_sub_callback_rust`.
        let closure_ptr = ctx as *mut Box<dyn FnMut(u64)>;
        assert!(!closure_ptr.is_null());
        let closure = &mut *closure_ptr;
        closure(subid);
    }
}
