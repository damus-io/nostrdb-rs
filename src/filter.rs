use crate::{bindings, Error, FilterError, Note, Result};
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
    #[allow(clippy::new_ret_no_self)]
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

    pub fn field(&self, index: i32) -> Option<FilterField<'_>> {
        let ptr = unsafe { bindings::ndb_filter_get_elements(self.as_ptr(), index) };

        if ptr.is_null() {
            return None;
        }

        Some(FilterElements::new(self, ptr).field())
    }

    pub fn elements(&self, index: i32) -> Option<FilterElements<'_>> {
        let ptr = unsafe { bindings::ndb_filter_get_elements(self.as_ptr(), index) };

        if ptr.is_null() {
            return None;
        }

        Some(FilterElements::new(self, ptr))
    }

    pub fn num_elements(&self) -> i32 {
        unsafe { &*(self.as_ptr()) }.num_elements
    }

    pub fn as_ptr(&self) -> *const bindings::ndb_filter {
        self.data.as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_filter {
        self.data.as_mut_ptr()
    }

    pub fn json_with_bufsize(&self, bufsize: usize) -> Result<String> {
        let mut buf = Vec::with_capacity(bufsize);
        unsafe {
            let size = bindings::ndb_filter_json(
                self.as_ptr(),
                buf.as_mut_ptr() as *mut ::std::os::raw::c_char,
                bufsize as ::std::os::raw::c_int,
            ) as usize;

            // Step 4: Check the return value for success
            if size == 0 {
                return Err(Error::BufferOverflow); // Handle the error appropriately
            }

            buf.set_len(size);

            Ok(std::str::from_utf8_unchecked(&buf[..size - 1]).to_string())
        }
    }

    pub fn json(&self) -> Result<String> {
        // 1mb buffer
        self.json_with_bufsize(1024usize * 1024usize)
    }
}

impl Default for FilterBuilder {
    fn default() -> Self {
        FilterBuilder::new()
    }
}

impl FilterBuilder {
    pub fn new() -> FilterBuilder {
        Self {
            data: Default::default(),
        }
    }

    pub fn as_ptr(&self) -> *const bindings::ndb_filter {
        self.data.as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_filter {
        self.data.as_mut_ptr()
    }

    pub fn add_int_element(&mut self, i: u64) -> Result<()> {
        let res = unsafe { bindings::ndb_filter_add_int_element(self.as_mut_ptr(), i) };
        if res == 0 {
            return Err(FilterError::already_exists());
        }

        Ok(())
    }

    pub fn add_str_element(&mut self, s: &str) -> Result<()> {
        let c_str = CString::new(s).expect("string to cstring conversion failed");
        let r = unsafe { bindings::ndb_filter_add_str_element(self.as_mut_ptr(), c_str.as_ptr()) };

        if r == 0 {
            return Err(FilterError::already_exists());
        }

        Ok(())
    }

    pub fn add_id_element(&mut self, id: &[u8; 32]) -> Result<()> {
        let ptr: *const ::std::os::raw::c_uchar = id.as_ptr() as *const ::std::os::raw::c_uchar;
        let r = unsafe { bindings::ndb_filter_add_id_element(self.as_mut_ptr(), ptr) };

        if r == 0 {
            return Err(FilterError::already_exists());
        }

        Ok(())
    }

    pub fn start_field(&mut self, field: bindings::ndb_filter_fieldtype) -> Result<()> {
        let r = unsafe { bindings::ndb_filter_start_field(self.as_mut_ptr(), field) };

        if r == 0 {
            return Err(FilterError::already_started());
        }

        Ok(())
    }

    pub fn start_tags_field(&mut self, tag: char) -> Result<()> {
        let r =
            unsafe { bindings::ndb_filter_start_tag_field(self.as_mut_ptr(), tag as u8 as c_char) };
        if r == 0 {
            return Err(FilterError::already_started());
        }
        Ok(())
    }

    pub fn start_kinds_field(&mut self) -> Result<()> {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_KINDS)
    }

    pub fn start_authors_field(&mut self) -> Result<()> {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_AUTHORS)
    }

    pub fn start_since_field(&mut self) -> Result<()> {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_SINCE)
    }

    pub fn start_until_field(&mut self) -> Result<()> {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_UNTIL)
    }

    pub fn start_limit_field(&mut self) -> Result<()> {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_LIMIT)
    }

    pub fn start_ids_field(&mut self) -> Result<()> {
        self.start_field(bindings::ndb_filter_fieldtype_NDB_FILTER_IDS)
    }

    #[allow(dead_code)]
    pub fn start_events_field(&mut self) -> Result<()> {
        self.start_tags_field('e')
    }

    pub fn start_pubkeys_field(&mut self) -> Result<()> {
        self.start_tags_field('p')
    }

    pub fn start_tag_field(&mut self, tag: char) -> Result<()> {
        let r =
            unsafe { bindings::ndb_filter_start_tag_field(self.as_mut_ptr(), tag as u8 as c_char) };
        if r == 0 {
            return Err(Error::filter(FilterError::FieldAlreadyStarted));
        }
        Ok(())
    }

    pub fn end_field(&mut self) {
        unsafe {
            bindings::ndb_filter_end_field(self.as_mut_ptr());
        };
    }

    pub fn events(mut self, events: Vec<[u8; 32]>) -> Result<Self> {
        self.start_tag_field('e')?;
        for ref id in events {
            self.add_id_element(id)?;
        }
        self.end_field();
        Ok(self)
    }

    pub fn event(mut self, id: &[u8; 32]) -> Result<Self> {
        self.start_tag_field('e')?;
        self.add_id_element(id)?;
        self.end_field();
        Ok(self)
    }

    pub fn ids<'a, I>(mut self, ids: I) -> Result<Self>
    where
        I: Iterator<Item = &'a [u8; 32]>,
    {
        self.start_ids_field()?;
        for id in ids {
            self.add_id_element(id)?;
        }
        self.end_field();
        Ok(self)
    }

    pub fn pubkeys(mut self, pubkeys: Vec<[u8; 32]>) -> Result<Self> {
        self.start_tag_field('p')?;
        for ref pk in pubkeys {
            self.add_id_element(pk)?;
        }
        self.end_field();
        Ok(self)
    }

    pub fn authors<'a, I>(mut self, authors: I) -> Result<Self>
    where
        I: Iterator<Item = &'a [u8; 32]>,
    {
        self.start_authors_field()?;
        for author in authors {
            self.add_id_element(author)?;
        }
        self.end_field();
        Ok(self)
    }

    pub fn kinds(mut self, kinds: Vec<u64>) -> Result<Self> {
        self.start_kinds_field()?;
        for kind in kinds {
            self.add_int_element(kind)?;
        }
        self.end_field();
        Ok(self)
    }

    pub fn pubkey(mut self, pubkeys: Vec<[u8; 32]>) -> Result<Self> {
        self.start_pubkeys_field()?;
        for ref pubkey in pubkeys {
            self.add_id_element(pubkey)?;
        }
        self.end_field();
        Ok(self)
    }

    pub fn tags(mut self, tags: Vec<String>, tag: char) -> Result<Self> {
        self.start_tag_field(tag)?;
        for tag in tags {
            self.add_str_element(&tag)?;
        }
        self.end_field();
        Ok(self)
    }

    pub fn since(mut self, since: u64) -> Result<Self> {
        self.start_since_field()?;
        self.add_int_element(since)?;
        self.end_field();
        Ok(self)
    }

    pub fn until(mut self, until: u64) -> Result<Self> {
        self.start_until_field()?;
        self.add_int_element(until)?;
        self.end_field();
        Ok(self)
    }

    pub fn limit(mut self, limit: u64) -> Result<Self> {
        self.start_limit_field()?;
        self.add_int_element(limit)?;
        self.end_field();
        Ok(self)
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

#[derive(Debug, Copy, Clone)]
pub struct FilterIter<'a> {
    filter: &'a Filter,
    index: i32,
}

/// Filter element: `authors`, `limit`, etc
#[derive(Copy, Clone, Debug)]
pub struct FilterElements<'a> {
    filter: &'a Filter,
    elements: *const bindings::ndb_filter_elements,
}

#[derive(Copy, Clone, Debug)]
pub struct FilterIdElements<'a> {
    filter: &'a Filter,
    elements: *const bindings::ndb_filter_elements,
}

#[derive(Copy, Clone, Debug)]
pub struct FilterIntElements<'a> {
    _filter: &'a Filter,
    elements: *const bindings::ndb_filter_elements,
}

pub struct FilterIdElemIter<'a> {
    ids: FilterIdElements<'a>,
    index: i32,
}

pub struct FilterIntElemIter<'a> {
    ints: FilterIntElements<'a>,
    index: i32,
}

impl<'a> FilterIdElemIter<'a> {
    pub(crate) fn new(ids: FilterIdElements<'a>) -> Self {
        let index = 0;
        Self { ids, index }
    }

    pub fn done(&self) -> bool {
        self.index >= self.ids.count()
    }
}

impl<'a> FilterIntElemIter<'a> {
    pub(crate) fn new(ints: FilterIntElements<'a>) -> Self {
        let index = 0;
        Self { ints, index }
    }

    pub fn done(&self) -> bool {
        self.index >= self.ints.count()
    }
}

impl<'a> FilterIdElements<'a> {
    pub(crate) fn new(filter: &'a Filter, elements: *const bindings::ndb_filter_elements) -> Self {
        Self { filter, elements }
    }

    pub fn count(&self) -> i32 {
        unsafe { &*self.elements }.count
    }

    /// Field element type. In the case of ids, it would be FieldElemType::Id, etc
    fn elemtype(&self) -> FieldElemType {
        FieldElemType::new(unsafe { &*self.elements }.field.elem_type)
            .expect("expected valid filter element type")
    }

    pub fn get(self, index: i32) -> Option<&'a [u8; 32]> {
        assert!(self.elemtype() == FieldElemType::Id);

        let id = unsafe {
            bindings::ndb_filter_get_id_element(self.filter.as_ptr(), self.elements, index)
                as *const [u8; 32]
        };

        if id.is_null() {
            return None;
        }

        Some(unsafe { &*id })
    }
}

impl<'a> FilterIntElements<'a> {
    pub(crate) fn new(filter: &'a Filter, elements: *const bindings::ndb_filter_elements) -> Self {
        Self {
            _filter: filter,
            elements,
        }
    }

    pub fn count(&self) -> i32 {
        unsafe { &*self.elements }.count
    }

    /// Field element type. In the case of ids, it would be FieldElemType::Id, etc
    fn elemtype(&self) -> FieldElemType {
        FieldElemType::new(unsafe { &*self.elements }.field.elem_type)
            .expect("expected valid filter element type")
    }

    pub fn get(self, index: i32) -> Option<u64> {
        if index >= self.count() {
            return None;
        }
        assert!(self.elemtype() == FieldElemType::Int);
        Some(unsafe { bindings::ndb_filter_get_int_element(self.elements, index) })
    }
}

pub enum FilterField<'a> {
    Ids(FilterIdElements<'a>),
    Authors(FilterIdElements<'a>),
    Kinds(FilterIntElements<'a>),
    Tags(char, FilterElements<'a>),
    Since(u64),
    Until(u64),
    Limit(u64),
}

impl<'a> FilterField<'a> {
    pub fn new(elements: FilterElements<'a>) -> Self {
        match elements.fieldtype() {
            FilterFieldType::Ids => {
                FilterField::Ids(FilterIdElements::new(elements.filter(), elements.as_ptr()))
            }

            FilterFieldType::Authors => {
                FilterField::Authors(FilterIdElements::new(elements.filter(), elements.as_ptr()))
            }

            FilterFieldType::Kinds => {
                FilterField::Kinds(FilterIntElements::new(elements.filter(), elements.as_ptr()))
            }

            FilterFieldType::Tags => FilterField::Tags(elements.tag(), elements),

            FilterFieldType::Since => FilterField::Since(
                FilterIntElements::new(elements.filter(), elements.as_ptr())
                    .into_iter()
                    .next()
                    .expect("expected since in filter"),
            ),

            FilterFieldType::Until => FilterField::Until(
                FilterIntElements::new(elements.filter(), elements.as_ptr())
                    .into_iter()
                    .next()
                    .expect("expected until in filter"),
            ),

            FilterFieldType::Limit => FilterField::Limit(
                FilterIntElements::new(elements.filter(), elements.as_ptr())
                    .into_iter()
                    .next()
                    .expect("expected limit in filter"),
            ),
        }
    }
}

impl<'a> FilterElements<'a> {
    pub(crate) fn new(filter: &'a Filter, elements: *const bindings::ndb_filter_elements) -> Self {
        FilterElements { filter, elements }
    }

    pub fn filter(self) -> &'a Filter {
        self.filter
    }

    pub fn as_ptr(self) -> *const bindings::ndb_filter_elements {
        self.elements
    }

    pub fn count(&self) -> i32 {
        unsafe { &*self.elements }.count
    }

    pub fn field(self) -> FilterField<'a> {
        FilterField::new(self)
    }

    pub fn get(self, index: i32) -> Option<FilterElement<'a>> {
        if index >= self.count() {
            return None;
        }

        match self.elemtype() {
            FieldElemType::Id => {
                let id = unsafe {
                    bindings::ndb_filter_get_id_element(self.filter.as_ptr(), self.elements, index)
                        as *const [u8; 32]
                };
                if id.is_null() {
                    return None;
                }
                Some(FilterElement::Id(unsafe { &*id }))
            }

            FieldElemType::Str => {
                let cstr = unsafe {
                    bindings::ndb_filter_get_string_element(
                        self.filter.as_ptr(),
                        self.elements,
                        index,
                    )
                };
                if cstr.is_null() {
                    return None;
                }
                let str = unsafe {
                    let byte_slice =
                        std::slice::from_raw_parts(cstr as *const u8, libc::strlen(cstr));
                    std::str::from_utf8_unchecked(byte_slice)
                };
                Some(FilterElement::Str(str))
            }

            FieldElemType::Int => {
                let num = unsafe { bindings::ndb_filter_get_int_element(self.elements, index) };
                Some(FilterElement::Int(num))
            }
        }
    }

    /// Field element type. In the case of ids, it would be FieldElemType::Id, etc
    pub fn elemtype(&self) -> FieldElemType {
        FieldElemType::new(unsafe { &*self.elements }.field.elem_type)
            .expect("expected valid filter element type")
    }

    /// Field element type. In the case of ids, it would be FieldElemType::Id, etc
    pub fn tag(&self) -> char {
        (unsafe { &*self.elements }.field.tag as u8) as char
    }

    pub fn fieldtype(self) -> FilterFieldType {
        FilterFieldType::new(unsafe { &*self.elements }.field.type_)
            .expect("expected valid fieldtype")
    }
}

impl<'a> FilterIter<'a> {
    pub fn new(filter: &'a Filter) -> Self {
        let index = 0;
        FilterIter { filter, index }
    }

    pub fn done(&self) -> bool {
        self.index >= self.filter.num_elements()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FilterFieldType {
    Ids,
    Authors,
    Kinds,
    Tags,
    Since,
    Until,
    Limit,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FieldElemType {
    Str,
    Id,
    Int,
}

impl FieldElemType {
    pub(crate) fn new(val: bindings::ndb_generic_element_type) -> Option<Self> {
        if val == bindings::ndb_generic_element_type_NDB_ELEMENT_UNKNOWN {
            None
        } else if val == bindings::ndb_generic_element_type_NDB_ELEMENT_STRING {
            Some(FieldElemType::Str)
        } else if val == bindings::ndb_generic_element_type_NDB_ELEMENT_ID {
            Some(FieldElemType::Id)
        } else if val == bindings::ndb_generic_element_type_NDB_ELEMENT_INT {
            Some(FieldElemType::Int)
        } else {
            None
        }
    }
}

impl FilterFieldType {
    pub(crate) fn new(val: bindings::ndb_filter_fieldtype) -> Option<Self> {
        if val == bindings::ndb_filter_fieldtype_NDB_FILTER_IDS {
            Some(FilterFieldType::Ids)
        } else if val == bindings::ndb_filter_fieldtype_NDB_FILTER_AUTHORS {
            Some(FilterFieldType::Authors)
        } else if val == bindings::ndb_filter_fieldtype_NDB_FILTER_KINDS {
            Some(FilterFieldType::Kinds)
        } else if val == bindings::ndb_filter_fieldtype_NDB_FILTER_TAGS {
            Some(FilterFieldType::Tags)
        } else if val == bindings::ndb_filter_fieldtype_NDB_FILTER_SINCE {
            Some(FilterFieldType::Since)
        } else if val == bindings::ndb_filter_fieldtype_NDB_FILTER_UNTIL {
            Some(FilterFieldType::Until)
        } else if val == bindings::ndb_filter_fieldtype_NDB_FILTER_LIMIT {
            Some(FilterFieldType::Limit)
        } else {
            None
        }
    }
}

impl<'a> IntoIterator for &'a Filter {
    type Item = FilterField<'a>;
    type IntoIter = FilterIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        FilterIter::new(self)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FilterElement<'a> {
    Str(&'a str),
    Id(&'a [u8; 32]),
    Int(u64),
}

impl<'a> Iterator for FilterIter<'a> {
    type Item = FilterField<'a>;

    fn next(&mut self) -> Option<FilterField<'a>> {
        if self.done() {
            return None;
        }

        let ind = self.index;
        self.index += 1;

        self.filter.field(ind)
    }
}

impl<'a> IntoIterator for FilterIdElements<'a> {
    type Item = &'a [u8; 32];
    type IntoIter = FilterIdElemIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        FilterIdElemIter::new(self)
    }
}

impl<'a> IntoIterator for FilterIntElements<'a> {
    type Item = u64;
    type IntoIter = FilterIntElemIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        FilterIntElemIter::new(self)
    }
}

impl<'a> Iterator for FilterIntElemIter<'a> {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        if self.done() {
            return None;
        }

        let ind = self.index;
        self.index += 1;

        self.ints.get(ind)
    }
}

impl<'a> Iterator for FilterIdElemIter<'a> {
    type Item = &'a [u8; 32];

    fn next(&mut self) -> Option<&'a [u8; 32]> {
        if self.done() {
            return None;
        }

        let ind = self.index;
        self.index += 1;

        self.ids.get(ind)
    }
}

impl<'a> IntoIterator for FilterElements<'a> {
    type Item = FilterElement<'a>;
    type IntoIter = FilterElemIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        FilterElemIter::new(self)
    }
}

impl<'a> Iterator for FilterElemIter<'a> {
    type Item = FilterElement<'a>;

    fn next(&mut self) -> Option<FilterElement<'a>> {
        let element = self.elements.get(self.index);
        if element.is_some() {
            self.index += 1;
            element
        } else {
            None
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct FilterElemIter<'a> {
    elements: FilterElements<'a>,
    index: i32,
}

impl<'a> FilterElemIter<'a> {
    pub(crate) fn new(elements: FilterElements<'a>) -> Self {
        let index = 0;
        FilterElemIter { elements, index }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_limit_iter_works() {
        let filter = Filter::new().limit(42).build();
        let mut hit = 0;
        for element in &filter {
            if let FilterField::Limit(42) = element {
                hit += 1;
            }
        }
        assert!(hit == 1);
    }

    #[test]
    fn filter_id_iter_works() {
        let id: [u8; 32] = [
            0xfb, 0x16, 0x5b, 0xe2, 0x2c, 0x7b, 0x25, 0x18, 0xb7, 0x49, 0xaa, 0xbb, 0x71, 0x40,
            0xc7, 0x3f, 0x08, 0x87, 0xfe, 0x84, 0x47, 0x5c, 0x82, 0x78, 0x57, 0x00, 0x66, 0x3b,
            0xe8, 0x5b, 0xa8, 0x59,
        ];

        let filter = Filter::new().ids(vec![id, id, id]).build();
        let mut hit = 0;
        for element in &filter {
            if let FilterField::Ids(ids) = element {
                for same_id in ids {
                    hit += 1;
                    assert!(same_id == &id);
                }
            }
        }
        assert!(hit == 3);
    }

    #[test]
    fn filter_int_iter_works() {
        let filter = Filter::new().kinds(vec![1, 2, 3]).build();
        let mut hit = 0;
        for element in &filter {
            if let FilterField::Kinds(ks) = element {
                hit += 1;
                assert!(vec![1, 2, 3] == ks.into_iter().collect::<Vec<u64>>());
            }
        }
        assert!(hit == 1);
    }

    #[test]
    fn filter_multiple_field_iter_works() {
        let id: [u8; 32] = [
            0xfb, 0x16, 0x5b, 0xe2, 0x2c, 0x7b, 0x25, 0x18, 0xb7, 0x49, 0xaa, 0xbb, 0x71, 0x40,
            0xc7, 0x3f, 0x08, 0x87, 0xfe, 0x84, 0x47, 0x5c, 0x82, 0x78, 0x57, 0x00, 0x66, 0x3b,
            0xe8, 0x5b, 0xa8, 0x59,
        ];
        let filter = Filter::new().event(&id).kinds(vec![1, 2, 3]).build();
        let mut hit = 0;
        for element in &filter {
            if let FilterField::Kinds(ks) = element {
                hit += 1;
                assert!(vec![1, 2, 3] == ks.into_iter().collect::<Vec<u64>>());
            } else if let FilterField::Tags('e', ids) = element {
                for i in ids {
                    hit += 1;
                    assert!(i == FilterElement::Id(&id));
                }
            }
        }
        assert!(hit == 2);
    }
}
