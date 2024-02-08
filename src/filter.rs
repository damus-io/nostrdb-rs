use crate::bindings;
use crate::Note;
use std::ffi::CString;
use std::os::raw::c_char;
use std::ptr::null_mut;

#[derive(Debug, Clone)]
pub struct Filter {
    pub data: bindings::ndb_filter,
}

impl bindings::ndb_filter {
    fn as_ptr(&self) -> *const bindings::ndb_filter {
        self as *const bindings::ndb_filter
    }
}

impl Filter {
    pub fn new() -> Filter {
        let null = null_mut();
        let mut filter_data = bindings::ndb_filter {
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
            current: null_mut(),
            elements: [
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
            ],
        };

        unsafe {
            bindings::ndb_filter_init(&mut filter_data as *mut bindings::ndb_filter);
        };

        Self { data: filter_data }
    }

    pub fn as_ptr(&self) -> *const bindings::ndb_filter {
        return self.data.as_ptr();
    }

    pub fn as_mut_ptr(&self) -> *mut bindings::ndb_filter {
        return self.data.as_ptr() as *mut bindings::ndb_filter;
    }

    fn add_int_element(&self, i: u64) {
        unsafe { bindings::ndb_filter_add_int_element(self.as_mut_ptr(), i) };
    }

    fn add_str_element(&self, s: &str) {
        let c_str = CString::new(s).expect("string to cstring conversion failed");
        unsafe {
            bindings::ndb_filter_add_str_element(self.as_mut_ptr(), c_str.as_ptr());
        };
    }

    fn add_id_element(&self, id: &[u8; 32]) {
        let ptr: *const ::std::os::raw::c_uchar = id.as_ptr() as *const ::std::os::raw::c_uchar;
        unsafe {
            bindings::ndb_filter_add_id_element(self.as_mut_ptr(), ptr);
        };
    }

    fn start_field(&self, field: bindings::ndb_filter_fieldtype) {
        unsafe { bindings::ndb_filter_start_field(self.as_mut_ptr(), field) };
    }

    fn start_tags_field(&self, tag: char) {
        unsafe { bindings::ndb_filter_start_tag_field(self.as_mut_ptr(), tag as i8) };
    }

    fn start_kinds_field(&self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_KINDS);
    }

    fn start_authors_field(&self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_AUTHORS);
    }

    fn start_since_field(&self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_SINCE);
    }

    fn start_limit_field(&self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_LIMIT);
    }

    fn start_ids_field(&self) {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_IDS);
    }

    fn start_events_field(&self) {
        self.start_tags_field('e');
    }

    fn start_pubkeys_field(&self) {
        self.start_tags_field('p');
    }

    fn start_tag_field(&self, tag: char) {
        unsafe { bindings::ndb_filter_start_tag_field(self.as_mut_ptr(), tag as u8 as c_char) };
    }

    fn end_field(&self) {
        unsafe { bindings::ndb_filter_end_field(self.as_mut_ptr()) }
    }

    pub fn events(&mut self, events: Vec<[u8; 32]>) -> &mut Filter {
        self.start_tag_field('e');
        for ref id in events {
            self.add_id_element(id);
        }
        self.end_field();
        self
    }

    pub fn ids(&mut self, ids: Vec<[u8; 32]>) -> &mut Filter {
        self.start_ids_field();
        for ref id in ids {
            self.add_id_element(id);
        }
        self.end_field();
        self
    }

    pub fn pubkeys(&mut self, pubkeys: Vec<[u8; 32]>) -> &mut Filter {
        self.start_tag_field('p');
        for ref pk in pubkeys {
            self.add_id_element(pk);
        }
        self.end_field();
        self
    }

    pub fn authors(&mut self, authors: Vec<[u8; 32]>) -> &mut Filter {
        self.start_authors_field();
        for author in authors {
            self.add_id_element(&author);
        }
        self.end_field();
        self
    }

    pub fn kinds(&mut self, kinds: Vec<u64>) -> &mut Filter {
        self.start_kinds_field();
        for kind in kinds {
            self.add_int_element(kind);
        }
        self.end_field();
        self
    }

    pub fn pubkey(&mut self, pubkeys: Vec<[u8; 32]>) -> &mut Filter {
        self.start_pubkeys_field();
        for ref pubkey in pubkeys {
            self.add_id_element(pubkey);
        }
        self.end_field();
        self
    }

    pub fn tags(&mut self, tags: Vec<String>, tag: char) -> &mut Filter {
        self.start_tag_field(tag);
        for tag in tags {
            self.add_str_element(&tag);
        }
        self.end_field();
        self
    }

    pub fn since(&mut self, since: u64) -> &mut Filter {
        self.start_since_field();
        self.add_int_element(since);
        self.end_field();
        self
    }

    pub fn limit(&mut self, limit: u64) -> &mut Filter {
        self.start_since_field();
        self.add_int_element(limit);
        self.end_field();
        self
    }

    pub fn matches(&self, note: &Note) -> bool {
        unsafe { bindings::ndb_filter_matches(self.as_mut_ptr(), note.as_ptr()) != 0 }
    }
}

/*
// This is unsafe.. but we still need a way to free the memory on these
impl Drop for Filter {
    fn drop(&mut self) {
        debug!("dropping filter {:?}", self);
        unsafe { bindings::ndb_filter_destroy(self.as_mut_ptr()) };
    }
}
*/
