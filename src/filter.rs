use crate::bindings;
use crate::Note;
use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr::null_mut;
use tracing::debug;

#[derive(Debug)]
pub struct FilterBuilder {
    pub data: bindings::ndb_filter,
}

#[derive(Debug)]
pub struct Filter {
    pub data: bindings::ndb_filter,
}

impl Clone for Filter {
    fn clone(&self) -> Self {
        let mut new_filter: bindings::ndb_filter = Default::default();
        unsafe {
            bindings::ndb_filter_clone(
                new_filter.as_mut_ptr(),
                self.as_ptr() as *mut bindings::ndb_filter,
            );
        };
        Filter { data: new_filter }
    }
}

impl bindings::ndb_filter {
    fn as_ptr(&self) -> *const bindings::ndb_filter {
        self as *const bindings::ndb_filter
    }

    fn as_mut_ptr(&mut self) -> *mut bindings::ndb_filter {
        self as *mut bindings::ndb_filter
    }
}

impl Default for bindings::ndb_filter {
    fn default() -> Self {
        let null = null_mut();
        let mut filter_data = bindings::ndb_filter {
            finalized: 0,
            elem_buf: bindings::cursor {
                start: null,
                p: null,
                end: null,
            },
            data_buf: bindings::cursor {
                start: null,
                p: null,
                end: null,
            },
            num_elements: 0,
            current: -1,
            elements: [0, 0, 0, 0, 0, 0, 0],
        };

        unsafe {
            bindings::ndb_filter_init(filter_data.as_mut_ptr());
        };

        filter_data
    }
}

impl Filter {
    pub fn new() -> FilterBuilder {
        FilterBuilder {
            data: Default::default(),
        }
    }

    pub fn matches(&self, note: &Note) -> bool {
        unsafe {
            bindings::ndb_filter_matches(self.as_ptr() as *mut bindings::ndb_filter, note.as_ptr())
                != 0
        }
    }

    pub fn as_ptr(&self) -> *const bindings::ndb_filter {
        return self.data.as_ptr();
    }

    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_filter {
        return self.data.as_mut_ptr() as *mut bindings::ndb_filter;
    }
}

impl FilterBuilder {
    pub fn new() -> FilterBuilder {
        Self {
            data: Default::default(),
        }
    }

    pub fn as_ptr(&self) -> *const bindings::ndb_filter {
        return self.data.as_ptr();
    }

    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_filter {
        return self.data.as_mut_ptr();
    }

    fn add_int_element(&mut self, i: u64) {
        unsafe { bindings::ndb_filter_add_int_element(self.as_mut_ptr(), i) };
    }

    fn add_str_element(&mut self, s: &str) {
        let c_str = CString::new(s).expect("string to cstring conversion failed");
        unsafe {
            bindings::ndb_filter_add_str_element(self.as_mut_ptr(), c_str.as_ptr());
        };
    }

    fn add_id_element(&mut self, id: &[u8; 32]) {
        let ptr: *const ::std::os::raw::c_uchar = id.as_ptr() as *const ::std::os::raw::c_uchar;
        unsafe {
            bindings::ndb_filter_add_id_element(self.as_mut_ptr(), ptr);
        };
    }

    fn start_field(&mut self, field: bindings::ndb_filter_fieldtype) {
        unsafe { bindings::ndb_filter_start_field(self.as_mut_ptr(), field) };
    }

    fn start_tags_field(&mut self, tag: char) {
        unsafe { bindings::ndb_filter_start_tag_field(self.as_mut_ptr(), tag as u8 as c_char) };
    }

    fn start_kinds_field(&mut self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_KINDS);
    }

    fn start_authors_field(&mut self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_AUTHORS);
    }

    fn start_since_field(&mut self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_SINCE);
    }

    fn start_limit_field(&mut self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_LIMIT);
    }

    fn start_ids_field(&mut self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_IDS);
    }

    #[allow(dead_code)]
    fn start_events_field(&mut self) {
        self.start_tags_field('e');
    }

    fn start_pubkeys_field(&mut self) {
        self.start_tags_field('p');
    }

    fn start_tag_field(&mut self, tag: char) {
        unsafe { bindings::ndb_filter_start_tag_field(self.as_mut_ptr(), tag as u8 as c_char) };
    }

    fn end_field(&mut self) {
        unsafe { bindings::ndb_filter_end_field(self.as_mut_ptr()) }
    }

    pub fn events(&mut self, events: Vec<[u8; 32]>) -> &mut Self {
        self.start_tag_field('e');
        for ref id in events {
            self.add_id_element(id);
        }
        self.end_field();
        self
    }

    pub fn ids(&mut self, ids: Vec<[u8; 32]>) -> &mut Self {
        self.start_ids_field();
        for ref id in ids {
            self.add_id_element(id);
        }
        self.end_field();
        self
    }

    pub fn pubkeys(&mut self, pubkeys: Vec<[u8; 32]>) -> &mut Self {
        self.start_tag_field('p');
        for ref pk in pubkeys {
            self.add_id_element(pk);
        }
        self.end_field();
        self
    }

    pub fn authors(&mut self, authors: Vec<[u8; 32]>) -> &mut Self {
        self.start_authors_field();
        for author in authors {
            self.add_id_element(&author);
        }
        self.end_field();
        self
    }

    pub fn kinds(&mut self, kinds: Vec<u64>) -> &mut Self {
        self.start_kinds_field();
        for kind in kinds {
            self.add_int_element(kind);
        }
        self.end_field();
        self
    }

    pub fn pubkey(&mut self, pubkeys: Vec<[u8; 32]>) -> &mut Self {
        self.start_pubkeys_field();
        for ref pubkey in pubkeys {
            self.add_id_element(pubkey);
        }
        self.end_field();
        self
    }

    pub fn tags(&mut self, tags: Vec<String>, tag: char) -> &mut Self {
        self.start_tag_field(tag);
        for tag in tags {
            self.add_str_element(&tag);
        }
        self.end_field();
        self
    }

    pub fn since(&mut self, since: u64) -> &mut Self {
        self.start_since_field();
        self.add_int_element(since);
        self.end_field();
        self
    }

    pub fn limit(&mut self, limit: u64) -> &mut Self {
        self.start_limit_field();
        self.add_int_element(limit);
        self.end_field();
        self
    }

    pub fn build(&mut self) -> Filter {
        unsafe {
            bindings::ndb_filter_end(self.as_mut_ptr());
        };
        Filter { data: self.data }
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        debug!("dropping filter {:?}", self);
        unsafe { bindings::ndb_filter_destroy(self.as_mut_ptr()) };
    }
}
