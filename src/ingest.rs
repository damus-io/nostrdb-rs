use crate::bindings;
use std::ffi::CString;

pub struct IngestMetadata {
    meta: bindings::ndb_ingest_meta,
    relay: Option<CString>,
}

impl Default for IngestMetadata {
    fn default() -> Self {
        Self {
            relay: None,
            meta: bindings::ndb_ingest_meta {
                client: 0,
                relay: std::ptr::null_mut(),
            },
        }
    }
}

impl IngestMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest a relay-sent event in the form `["EVENT", {"id:"...}]`
    pub fn client(mut self, from_client: bool) -> Self {
        self.meta.client = if from_client { 1 } else { 0 };
        self
    }

    fn relay_str(&self) -> *const ::std::os::raw::c_char {
        match &self.relay {
            Some(relay_cstr) => relay_cstr.as_ptr(),
            None => std::ptr::null(),
        }
    }

    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_ingest_meta {
        // update the ingest relay str with our cstr if we have one
        self.meta.relay = self.relay_str();
        &mut self.meta
    }

    pub fn relay(mut self, relay: &str) -> Self {
        self.relay = Some(CString::new(relay).expect("should never happen"));
        self
    }
}
