use crate::{bindings, NdbStr, Note};

#[derive(Debug, Copy, Clone)]
pub struct Tag<'a> {
    ptr: *mut bindings::ndb_tag,
    note: &'a Note<'a>,
}

impl<'a> Tag<'a> {
    pub(crate) fn new(ptr: *mut bindings::ndb_tag, note: &'a Note<'a>) -> Self {
        Tag { ptr, note }
    }

    pub fn count(&self) -> u16 {
        unsafe { bindings::ndb_tag_count(self.as_ptr()) }
    }

    pub fn get(&self, ind: u16) -> Option<NdbStr<'a>> {
        if ind >= self.count() {
            return None;
        }
        let nstr = unsafe {
            bindings::ndb_tag_str(
                self.note().as_ptr(),
                self.as_ptr(),
                ind as ::std::os::raw::c_int,
            )
        };
        Some(NdbStr::new(nstr, self.note))
    }

    pub fn note(&self) -> &'a Note<'a> {
        self.note
    }

    pub fn as_ptr(&self) -> *mut bindings::ndb_tag {
        self.ptr
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Tags<'a> {
    ptr: *mut bindings::ndb_tags,
    note: &'a Note<'a>,
}

impl<'a> Tags<'a> {
    pub(crate) fn new(ptr: *mut bindings::ndb_tags, note: &'a Note<'a>) -> Self {
        Tags { ptr, note }
    }

    pub fn count(&self) -> u16 {
        unsafe { bindings::ndb_tags_count(self.as_ptr()) }
    }

    pub fn iter(&self) -> TagsIter<'a> {
        TagsIter::new(self.note)
    }

    pub fn note(&self) -> &'a Note<'a> {
        self.note
    }

    pub fn as_ptr(&self) -> *mut bindings::ndb_tags {
        self.ptr
    }
}

#[derive(Debug, Copy, Clone)]
pub struct TagsIter<'a> {
    iter: bindings::ndb_iterator,
    note: &'a Note<'a>,
}

impl<'a> TagsIter<'a> {
    pub fn new(note: &'a Note<'a>) -> Self {
        let iter = bindings::ndb_iterator {
            note: std::ptr::null_mut(),
            tag: std::ptr::null_mut(),
            index: 0,
        };
        let mut iter = TagsIter { note, iter };
        unsafe {
            bindings::ndb_tags_iterate_start(note.as_ptr(), &mut iter.iter);
        };
        iter
    }

    pub fn tag(&self) -> Option<Tag<'a>> {
        let tag_ptr = unsafe { *self.as_ptr() }.tag;
        if tag_ptr.is_null() {
            None
        } else {
            Some(Tag::new(tag_ptr, self.note()))
        }
    }

    pub fn note(&self) -> &'a Note<'a> {
        self.note
    }

    pub fn as_ptr(&self) -> *const bindings::ndb_iterator {
        &self.iter
    }

    pub fn as_mut_ptr(&mut self) -> *mut bindings::ndb_iterator {
        &mut self.iter
    }
}

#[derive(Debug, Copy, Clone)]
pub struct TagIter<'a> {
    tag: Tag<'a>,
    index: u16,
}

impl<'a> TagIter<'a> {
    pub fn new(tag: Tag<'a>) -> Self {
        let index = 0;
        TagIter { tag, index }
    }

    pub fn done(&self) -> bool {
        self.index >= self.tag.count()
    }
}

impl<'a> Iterator for TagIter<'a> {
    type Item = NdbStr<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let tag = self.tag.get(self.index);
        if tag.is_some() {
            self.index += 1;
            tag
        } else {
            None
        }
    }
}

impl<'a> Iterator for TagsIter<'a> {
    type Item = Tag<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            bindings::ndb_tags_iterate_next(self.as_mut_ptr());
        };
        self.tag()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_util;
    use crate::{Filter, Ndb, NdbStrVariant, Transaction};

    #[tokio::test]
    async fn tag_iter_works() {
        let db = "target/testdbs/tag_iter_works";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let sub = ndb
                .subscribe(vec![Filter::new()
                    .ids(vec![[
                        0xc5, 0xd9, 0x8c, 0xbf, 0x4b, 0xcd, 0x81, 0x1e, 0x28, 0x66, 0x77, 0x0c,
                        0x3d, 0x38, 0x0c, 0x02, 0x84, 0xce, 0x1d, 0xaf, 0x3a, 0xe9, 0x98, 0x3d,
                        0x22, 0x56, 0x5c, 0xb0, 0x66, 0xcf, 0x2a, 0x19,
                    ]])
                    .build()])
                .expect("sub");
            let waiter = ndb.wait_for_notes(&sub, 1);
            ndb.process_event(r#"["EVENT","s",{"id": "c5d98cbf4bcd811e2866770c3d380c0284ce1daf3ae9983d22565cb066cf2a19","pubkey": "083727b7a6051673f399102dc48c229c0ec08186ecd7e54ad0e9116d38429c4f","created_at": 1712517119,"kind": 1,"tags": [["e","b9e548b4aa30fa4ce9edf552adaf458385716704994fbaa9e0aa0042a5a5e01e"],["p","140ee9ff21da6e6671f750a0a747c5a3487ee8835159c7ca863e867a1c537b4f"],["hi","3"]],"content": "hi","sig": "1eed792e4db69c2bde2f5be33a383ef8b17c6afd1411598d0c4618fbdf4dbcb9689354276a74614511907a45eec234e0786733e8a6fbb312e6abf153f15fd437"}]"#).expect("process ok");
            let res = waiter.await.expect("await ok");
            assert_eq!(res.len(), 1);
            let note_key = res[0];
            let txn = Transaction::new(&ndb).expect("txn");
            let note = ndb.get_note_by_key(&txn, note_key).expect("note");
            let tags = note.tags();
            assert_eq!(tags.count(), 3);

            let mut tags_iter = tags.iter();

            let t0 = tags_iter.next().expect("t0");
            let t0_e0 = t0.get(0).expect("e tag ok");
            let t0_e1 = t0.get(1).expect("e id ok");
            assert_eq!(t0.get(2).is_none(), true);
            assert_eq!(t0_e0.variant(), NdbStrVariant::Str("e"));
            assert_eq!(
                t0_e1.variant(),
                NdbStrVariant::Id(&[
                    0xb9, 0xe5, 0x48, 0xb4, 0xaa, 0x30, 0xfa, 0x4c, 0xe9, 0xed, 0xf5, 0x52, 0xad,
                    0xaf, 0x45, 0x83, 0x85, 0x71, 0x67, 0x04, 0x99, 0x4f, 0xba, 0xa9, 0xe0, 0xaa,
                    0x00, 0x42, 0xa5, 0xa5, 0xe0, 0x1e
                ])
            );

            let t1 = tags_iter.next().expect("t1");
            let t1_e0 = t1.get(0).expect("p tag ok");
            let t1_e1 = t1.get(1).expect("p id ok");
            assert_eq!(t1.get(2).is_none(), true);
            assert_eq!(t1_e0.variant(), NdbStrVariant::Str("p"));
            assert_eq!(
                t1_e1.variant(),
                NdbStrVariant::Id(&[
                    0x14, 0x0e, 0xe9, 0xff, 0x21, 0xda, 0x6e, 0x66, 0x71, 0xf7, 0x50, 0xa0, 0xa7,
                    0x47, 0xc5, 0xa3, 0x48, 0x7e, 0xe8, 0x83, 0x51, 0x59, 0xc7, 0xca, 0x86, 0x3e,
                    0x86, 0x7a, 0x1c, 0x53, 0x7b, 0x4f
                ])
            );

            let t2 = tags_iter.next().expect("t2");
            let t2_e0 = t2.get(0).expect("hi tag ok");
            let t2_e1 = t2.get(1).expect("hi value ok");
            assert_eq!(t2.get(2).is_none(), true);
            assert_eq!(t2_e0.variant(), NdbStrVariant::Str("hi"));
            assert_eq!(t2_e1.variant(), NdbStrVariant::Str("3"));
        }
    }
}
