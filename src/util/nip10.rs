use crate::{Error, Tag, Tags};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Marker {
    Reply,
    Root,
    Mention,
}

#[derive(Clone, Copy, Debug)]
pub struct NoteIdRef<'a> {
    pub id: &'a [u8; 32],
    pub relay: Option<&'a str>,
    pub marker: Option<Marker>,
}

#[derive(Clone, Copy, Debug)]
pub struct NoteReply<'a> {
    root: Option<NoteIdRef<'a>>,
    reply: Option<NoteIdRef<'a>>,
    mention: Option<NoteIdRef<'a>>,
}

impl<'a> NoteReply<'a> {
    pub fn reply_to_root(self) -> Option<NoteIdRef<'a>> {
        if self.is_reply_to_root() {
            self.root
        } else {
            None
        }
    }

    pub fn new(tags: Tags<'a>) -> NoteReply<'a> {
        tags_to_note_reply(tags)
    }

    pub fn is_reply_to_root(&self) -> bool {
        self.root.is_some() && self.reply.is_none()
    }

    pub fn is_reply(&self) -> bool {
        self.reply().is_some()
    }

    pub fn reply(self) -> Option<NoteIdRef<'a>> {
        if self.reply.is_some() {
            self.reply
        } else if self.root.is_some() {
            self.root
        } else {
            None
        }
    }

    pub fn mention(self) -> Option<NoteIdRef<'a>> {
        self.mention
    }
}

impl Marker {
    pub fn new(s: &str) -> Option<Self> {
        if s == "reply" {
            Some(Marker::Reply)
        } else if s == "root" {
            Some(Marker::Root)
        } else if s == "mention" {
            Some(Marker::Mention)
        } else {
            None
        }
    }
}

fn tags_to_note_reply<'a>(tags: Tags<'a>) -> NoteReply<'a> {
    let mut root: Option<NoteIdRef<'a>> = None;
    let mut reply: Option<NoteIdRef<'a>> = None;
    let mut mention: Option<NoteIdRef<'a>> = None;
    let mut first: bool = true;

    for tag in tags {
        if root.is_some() && reply.is_some() && mention.is_some() {
            break;
        }

        let note_ref = if let Ok(note_ref) = tag_to_noteid_ref(tag) {
            note_ref
        } else {
            continue;
        };

        if let Some(marker) = note_ref.marker {
            match marker {
                Marker::Root => root = Some(note_ref),
                Marker::Reply => {
                    if reply.is_none() {
                        reply = Some(note_ref)
                    }
                }
                Marker::Mention => {
                    if mention.is_none() {
                        mention = Some(note_ref)
                    }
                }
            }
        } else {
            if first {
                root = Some(note_ref);
                first = false;
            } else {
                if reply.is_none() {
                    reply = Some(note_ref)
                }
            }
        }
    }

    NoteReply {
        root,
        reply,
        mention,
    }
}

pub fn tag_to_noteid_ref(tag: Tag<'_>) -> Result<NoteIdRef<'_>, Error> {
    if tag.count() < 2 {
        return Err(Error::DecodeError);
    }

    if tag.get_unchecked(0).variant().str() != Some("e") {
        return Err(Error::DecodeError);
    }

    let id = tag
        .get_unchecked(1)
        .variant()
        .id()
        .ok_or(Error::DecodeError)?;

    let relay = tag.get(2).and_then(|t| t.variant().str());
    let marker = tag
        .get(3)
        .and_then(|t| t.variant().str())
        .and_then(Marker::new);

    Ok(NoteIdRef { id, relay, marker })
}

#[cfg(test)]
mod test {
    use crate::*;

    #[tokio::test]
    async fn nip10_marker() {
        let db = "target/testdbs/nip10_marker";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let filter = Filter::new().kinds(vec![1]).build();
            let root_id: [u8; 32] =
                hex::decode("7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d4")
                    .unwrap()
                    .try_into()
                    .unwrap();
            let reply_id: [u8; 32] =
                hex::decode("7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d3")
                    .unwrap()
                    .try_into()
                    .unwrap();
            let sub = ndb.subscribe(vec![filter.clone()]).expect("sub_id");
            let waiter = ndb.wait_for_notes(&sub, 1);

            ndb.process_event(r#"
            [
              "EVENT",
              "huh",
              {
                "id": "19377cb4b9b807561830ab6d4c1fae7b9c9f1b623c15d10590cacc859cf19d76",
                "pubkey": "4871687b7b0aee3f1649c866e61724d79d51e673936a5378f5ed90bf7580791f",
                "created_at": 1714170678,
                "kind": 1,
                "tags": [
                  ["e", "7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d3", "", "reply" ],
                  ["e", "7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d4", "wss://relay.damus.io", "root" ]
                ],
                "content": "hi",
                "sig": "53921b1572c2e4373180a9f71513a0dee286cba6193d983052f96285c08f0e0158773d82ac97991ba8d390f6f54f84d5272c2e945f2e854a750f9cf038c0f759"
              }
            ]"#).expect("process ok");

            let res = waiter.await.expect("await ok");
            assert_eq!(res, vec![NoteKey::new(1)]);
            let txn = Transaction::new(&ndb).unwrap();
            let res = ndb.query(&txn, vec![filter], 1).expect("note");
            let note_reply = NoteReply::new(res[0].note.tags());

            assert_eq!(*note_reply.root.unwrap().id, root_id);
            assert_eq!(*note_reply.reply.unwrap().id, reply_id);
            assert_eq!(
                note_reply.root.unwrap().relay.unwrap(),
                "wss://relay.damus.io"
            );
        }
    }

    #[tokio::test]
    async fn nip10_deprecated() {
        let db = "target/testdbs/nip10_deprecated_reply";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let filter = Filter::new().kinds(vec![1]).build();
            let root_id: [u8; 32] =
                hex::decode("7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d4")
                    .unwrap()
                    .try_into()
                    .unwrap();
            let reply_id: [u8; 32] =
                hex::decode("7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d3")
                    .unwrap()
                    .try_into()
                    .unwrap();
            let sub = ndb.subscribe(vec![filter.clone()]).expect("sub_id");
            let waiter = ndb.wait_for_notes(&sub, 1);

            ndb.process_event(r#"
            [
              "EVENT",
              "huh",
              {
                "id": "ebac7df823ab975b6d2696505cf22a959067b74b1761c5581156f2a884036997",
                "pubkey": "118758f9a951c923b8502cfb8b2f329bee2a46356b6fc4f65c1b9b4730e0e9e5",
                "created_at": 1714175831,
                "kind": 1,
                "tags": [
                  [
                    "e",
                    "7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d4"
                  ],
                  [
                    "e",
                    "7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d3"
                  ]
                ],
                "content": "hi",
                "sig": "05913c7b19a70188d4dec5ac53d5da39fea4d5030c28176e52abb211e1bde60c5947aca8af359a00c8df8d96127b2f945af31f21fe01392b661bae12e7d14b1d"
              }
            ]"#).expect("process ok");

            let res = waiter.await.expect("await ok");
            assert_eq!(res, vec![NoteKey::new(1)]);
            let txn = Transaction::new(&ndb).unwrap();
            let res = ndb.query(&txn, vec![filter], 1).expect("note");
            let note_reply = NoteReply::new(res[0].note.tags());

            assert_eq!(*note_reply.root.unwrap().id, root_id);
            assert_eq!(*note_reply.reply.unwrap().id, reply_id);
            assert_eq!(note_reply.reply_to_root().is_none(), true);
            assert_eq!(*note_reply.reply().unwrap().id, reply_id);
        }
    }

    #[tokio::test]
    async fn nip10_mention() {
        let db = "target/testdbs/nip10_mention";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let filter = Filter::new().kinds(vec![1]).build();
            let root_id: [u8; 32] =
                hex::decode("7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d4")
                    .unwrap()
                    .try_into()
                    .unwrap();
            let mention_id: [u8; 32] =
                hex::decode("7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d3")
                    .unwrap()
                    .try_into()
                    .unwrap();
            let sub = ndb.subscribe(vec![filter.clone()]).expect("sub_id");
            let waiter = ndb.wait_for_notes(&sub, 1);

            ndb.process_event(r#"
            [
              "EVENT",
              "huh",
              {
                "id": "9521de81704269f9f61c042355eaa97a845a90c0ce6637b290800fa5a3c0b48d",
                "pubkey": "b3aceb5b36a235377c80dc2a1b3594a1d49e394b4d74fa11bc7cb4cf0bf677b2",
                "created_at": 1714177990,
                "kind": 1,
                "tags": [
                  [
                    "e",
                    "7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d3",
                    "",
                    "mention"
                  ],
                  [
                    "e",
                    "7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d4",
                    "wss://relay.damus.io",
                    "root"
                  ]
                ],
                "content": "hi",
                "sig": "e908ec395f6ea907a4b562b3ebf1bf61653566a5648574a1f8c752285797e5870e57416a0be933ce580fc3d65c874909c9dacbd1575c15bd97b8a68ea2b5160b"
              }
            ]"#).expect("process ok");

            let res = waiter.await.expect("await ok");
            assert_eq!(res, vec![NoteKey::new(1)]);
            let txn = Transaction::new(&ndb).unwrap();
            let res = ndb.query(&txn, vec![filter], 1).expect("note");
            let note_reply = NoteReply::new(res[0].note.tags());

            assert_eq!(*note_reply.reply_to_root().unwrap().id, root_id);
            assert_eq!(*note_reply.reply().unwrap().id, root_id);
            assert_eq!(*note_reply.mention().unwrap().id, mention_id);
            assert_eq!(note_reply.is_reply_to_root(), true);
            assert_eq!(note_reply.is_reply(), true);
        }
    }

    #[tokio::test]
    async fn nip10_deprecated_reply_to_root() {
        let db = "target/testdbs/nip10_deprecated_reply_to_root";
        test_util::cleanup_db(&db);

        {
            let ndb = Ndb::new(db, &Config::new()).expect("ndb");
            let filter = Filter::new().kinds(vec![1]).build();
            let root_id: [u8; 32] =
                hex::decode("7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d3")
                    .unwrap()
                    .try_into()
                    .unwrap();
            let sub = ndb.subscribe(vec![filter.clone()]).expect("sub_id");
            let waiter = ndb.wait_for_notes(&sub, 1);

            ndb.process_event(r#"
            [
              "EVENT",
              "huh",
              {
                "id": "140280b7886c48bddd99684b951c6bb61bebc8270a4989f316282c72aa35e5ba",
                "pubkey": "5ee7067e7155a9abf494e3e47e3249254cf95389a0c6e4f75cbbf35c8c675c23",
                "created_at": 1714178274,
                "kind": 1,
                "tags": [
                  [
                    "e",
                    "7d33c272a74e75c7328b891ab69420dd820cc7544fc65cd29a058c3495fd27d3"
                  ]
                ],
                "content": "hi",
                "sig": "e433d468d49fbc0f466b1a8ccefda71b0e17af471e579b56b8ce36477c116109c44d1065103ed6c01f838af92a13e51969d3b458f69c09b6f12785bd07053eb5"
              }
            ]"#).expect("process ok");

            let res = waiter.await.expect("await ok");
            assert_eq!(res, vec![NoteKey::new(1)]);
            let txn = Transaction::new(&ndb).unwrap();
            let res = ndb.query(&txn, vec![filter], 1).expect("note");
            let note_reply = NoteReply::new(res[0].note.tags());

            assert_eq!(*note_reply.reply_to_root().unwrap().id, root_id);
            assert_eq!(*note_reply.reply().unwrap().id, root_id);
            assert_eq!(note_reply.mention().is_none(), true);
        }
    }
}
