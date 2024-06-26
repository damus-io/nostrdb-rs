use crate::bindings;

pub struct Config {
    pub config: bindings::ndb_config,
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

        Config { config }
    }

    //
    pub fn set_flags(&mut self, flags: i32) -> &mut Self {
        self.config.flags = flags;
        self
    }

    pub fn skip_validation(&mut self, skip: bool) -> &mut Self {
        let skip_note_verify: i32 = 1 << 1;

        if skip {
            self.config.flags |= skip_note_verify;
        } else {
            self.config.flags &= !skip_note_verify;
        }

        self
    }

    pub fn set_ingester_threads(&mut self, threads: i32) -> &mut Self {
        self.config.ingester_threads = threads;
        self
    }

    // Add other setter methods as needed

    // Internal method to get a raw pointer to the config, used in Ndb
    pub fn as_ptr(&self) -> *const bindings::ndb_config {
        &self.config
    }
}
