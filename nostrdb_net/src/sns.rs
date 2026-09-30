//! NIP-SNS: Shared Note Storage
//!
//! The sealed, multi-writer sibling of [`crate::pns`]. A shared 32-byte
//! `team_root` secret derives a channel keypair that every member holds. Any
//! member publishes a kind-1081 **envelope** (signed by the shared team
//! keypair, symmetric-encrypted with the shared nip44 key) wrapping a kind-13
//! **seal** (signed by the member, ECDH-encrypted to the team keypair) wrapping
//! the **rumor** — the actual board action, carrying the member's real pubkey.
//! The seal is what proves authorship even though every member can decrypt.
//!
//! This module is the *publish + derivation* half only. nostrdb owns the ingest
//! auto-unwrap (register a `team_root`, match kind-1081 by the team pubkey,
//! symmetric-decrypt the envelope, then reuse the existing seal peel), so the
//! running app never unwraps an envelope itself — it queries the rumors nostrdb
//! has already stored. The derivation and envelope byte format defined here MUST
//! match the nostrdb C side — see `docs/nip-sns-sealed-shared-storage.md`. The
//! tests below re-implement that ingest peel purely to round-trip the format.
//!
//! Key derivation:
//!   team_keypair   = derive_secp256k1_keypair(team_root)
//!   team_nip44_key = hkdf_extract(ikm=team_root, salt="nip44-v2")

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use hkdf::Hkdf;
use nostr::key::PublicKey;
use nostr::nips::nip19::FromBech32;
use nostr::nips::nip44;
use nostr::nips::nip44::v2::{self, ConversationKey};
use nostrdb::{Note, NoteBuilder};
use sha2::Sha256;

use crate::{FullKeypair, Pubkey};

/// Kind number for the SNS envelope (outer wrapper). Provisional, per the spec.
pub const SNS_ENVELOPE_KIND: u32 = 1081;
/// Kind number for the NIP-59 seal (reused verbatim).
pub const SEAL_KIND: u16 = 13;
/// Kind number for the SNS key-share rumor (delivered gift-wrapped).
pub const KEYSHARE_KIND: u32 = 1082;

/// Salt for deriving the envelope's NIP-44 symmetric key from `team_root`.
const NIP44_SALT: &[u8] = b"nip44-v2";

/// Salt prefix domain-separating [`derive_board_root`] from every other HKDF use.
const BOARD_ROOT_SALT: &[u8] = b"nip-sns-board-root";

/// Everything derived from a `team_root` needed to publish to and read a
/// channel.
pub struct SnsKeys {
    /// Shared channel keypair. Its pubkey **is** the channel (members subscribe
    /// to envelopes authored by it, and it signs them); its secret decrypts
    /// seals (as the ECDH recipient).
    pub team_keypair: FullKeypair,
    /// Symmetric NIP-44 conversation key for the outer envelope layer.
    pub envelope_key: ConversationKey,
}

/// Derive all SNS channel keys from a 32-byte `team_root`.
///
/// Deterministic: the same root always yields the same channel keypair and
/// envelope key. Returns `None` only if `team_root` is not a valid secp256k1
/// secret key (negligible for a random root, but the root arrives over the wire
/// so it is not assumed valid). This derivation must byte-match the nostrdb C
/// `ndb_ingester_add_sns_key`.
pub fn derive_sns_keys(team_root: &[u8; 32]) -> Option<SnsKeys> {
    let team_keypair = FullKeypair::from_secret_bytes(team_root)?;
    let nip44_key = hkdf_extract(team_root, NIP44_SALT);
    let envelope_key = ConversationKey::new(nip44_key);
    Some(SnsKeys {
        team_keypair,
        envelope_key,
    })
}

/// Derive a **deterministic** `team_root` for a board the account owns, from that
/// account's own `secret` and a `label` (typically the board's slug).
///
/// Unlike a randomly minted root, every device holding the same account secret
/// derives the *same* root for the same label. That is what an auto-created board
/// needs: a board seeded independently on each of the account's devices at the
/// same coordinate would, with per-device random roots, mint a *divergent* SNS
/// channel per device (different team keypair → separate folds that never
/// converge). A deterministic root collapses them to one channel with no
/// cross-device coordination and no "wait for the other device's key-share" race.
///
/// `label` scopes the derivation so two boards under one account get isolated
/// roots (sharing one board's key can't decrypt another). The result is always a
/// valid secp256k1 scalar — HKDF-Extract output is out of range only with
/// probability ~2⁻¹²⁸, and that case is rehashed away — so [`derive_sns_keys`]
/// never rejects it. This is a *private* choice of root that no other party
/// re-derives, so (unlike [`derive_sns_keys`]) it needs no cross-implementation
/// byte match; only one account's own devices must agree, and they run this code.
pub fn derive_board_root(secret: &[u8; 32], label: &str) -> [u8; 32] {
    let salt = [BOARD_ROOT_SALT, b"/", label.as_bytes()].concat();
    let mut candidate = hkdf_extract(secret, &salt);
    // Rehash on the ~2⁻¹²⁸ chance the bytes aren't a valid secp256k1 secret
    // (≥ curve order, or zero), so the derivation deterministically terminates
    // with a root every device agrees on and derive_sns_keys accepts.
    while FullKeypair::from_secret_bytes(&candidate).is_none() {
        candidate = hkdf_extract(&candidate, BOARD_ROOT_SALT);
    }
    candidate
}

/// Wrap a rumor (the inner board action as event JSON, carrying the member's
/// real pubkey) into a signed kind-1081 SNS envelope ready to publish.
///
/// `member` is the authoring member's real keypair — the seal is signed by it,
/// so the rumor nostrdb ingests is attributable to that pubkey. `created_at`
/// stamps both wrapper layers (the rumor keeps its own timestamp inside
/// `rumor_json`). Returns `None` if encryption or note construction fails.
pub fn wrap_rumor(
    keys: &SnsKeys,
    member: &FullKeypair,
    rumor_json: &str,
    created_at: u64,
) -> Option<Note<'static>> {
    let team_pk = nostrcrate_pk(&keys.team_keypair.pubkey)?;

    // Seal (kind 13): signed by the member, ECDH-encrypted member ⇄ team pubkey.
    let sealed_rumor =
        nip44::encrypt(&member.secret_key, &team_pk, rumor_json, nip44::Version::V2).ok()?;
    let seal_json = NoteBuilder::new()
        .kind(SEAL_KIND as u32)
        .content(&sealed_rumor)
        .created_at(created_at)
        .sign(&member.secret_key.secret_bytes())
        .build()?
        .json()
        .ok()?;

    // Envelope (kind 1081): signed by the team keypair, symmetric-encrypted with
    // the shared envelope key. No routing tags — the channel pubkey is the only
    // addressing, and it is unguessable (derived from the secret root).
    let payload = v2::encrypt_to_bytes(&keys.envelope_key, &seal_json).ok()?;
    let content = BASE64.encode(payload);
    NoteBuilder::new()
        .kind(SNS_ENVELOPE_KIND)
        .content(&content)
        .created_at(created_at)
        .sign(&keys.team_keypair.secret_key.secret_bytes())
        .build()
}

/// Gift-wrap a kind-1082 key-share (the *outbound* half of membership) ready to
/// publish. The `sender` shares `team_root` — which unlocks the board named by
/// `board_addr` (`30619:<owner-hex>:<board-id>`) — with `recipient` by minting the
/// NIP-59 nest [`parse_keyshare`] reads back: a `1082` key-share rumor, a kind-13
/// **seal** signed by `sender`, and a kind-1059 **gift wrap** `p`-tagged to
/// `recipient`. nostrdb auto-unwraps the gift wrap (once `recipient`'s key is
/// registered via `ndb.add_key`) into a durable `1082` rumor.
///
/// This drives both membership flows: `sender == recipient` self-shares the root
/// of a board you just created (team-of-one), and `sender != recipient` invites a
/// member — with the *same* root, so an existing board is shared with no history
/// re-seal. `epoch`, if given, tags the rotation generation. `created_at` stamps
/// all three layers. Returns `None` if encryption or note construction fails.
///
/// Encryption mirrors [`wrap_rumor`]'s seal: NIP-44 v2 with `nip44::encrypt`,
/// whose internal RNG supplies the nonce; the gift wrap adds a throwaway ephemeral
/// keypair so the outer author reveals nothing about `sender`.
pub fn wrap_keyshare(
    sender: &FullKeypair,
    recipient: &Pubkey,
    team_root: &[u8; 32],
    board_addr: &str,
    epoch: Option<u32>,
    created_at: u64,
) -> Option<Note<'static>> {
    let recipient_pk = nostrcrate_pk(recipient)?;

    // Rumor (kind 1082): the share itself, signed by the sender. A 64-hex
    // `team_root` value is stored by nostrdb as a binary id element, which is what
    // `parse_keyshare` reads back first (see its `get_id` note).
    let mut rumor = NoteBuilder::new()
        .kind(KEYSHARE_KIND)
        .content("")
        .created_at(created_at)
        .start_tag()
        .tag_str("team_root")
        .tag_str(&hex::encode(team_root))
        .start_tag()
        .tag_str("a")
        .tag_str(board_addr);
    if let Some(epoch) = epoch {
        rumor = rumor
            .start_tag()
            .tag_str("epoch")
            .tag_str(&epoch.to_string());
    }
    let rumor_json = rumor
        .sign(&sender.secret_key.secret_bytes())
        .build()?
        .json()
        .ok()?;

    // Seal (kind 13): signed by the sender, ECDH-encrypted sender ⇄ recipient, so
    // the recipient can attribute the share to its real author.
    let sealed_rumor = nip44::encrypt(
        &sender.secret_key,
        &recipient_pk,
        &rumor_json,
        nip44::Version::V2,
    )
    .ok()?;
    let seal_json = NoteBuilder::new()
        .kind(SEAL_KIND as u32)
        .content(&sealed_rumor)
        .created_at(created_at)
        .sign(&sender.secret_key.secret_bytes())
        .build()?
        .json()
        .ok()?;

    // Gift wrap (kind 1059): a throwaway ephemeral key signs and ECDH-encrypts the
    // seal to the recipient, `p`-tagged so relays route it to their inbox.
    let wrap_keys = FullKeypair::generate();
    let encrypted_seal = nip44::encrypt(
        &wrap_keys.secret_key,
        &recipient_pk,
        &seal_json,
        nip44::Version::V2,
    )
    .ok()?;
    NoteBuilder::new()
        .kind(1059)
        .content(&encrypted_seal)
        .created_at(created_at)
        .start_tag()
        .tag_str("p")
        .tag_str(&recipient.hex())
        .sign(&wrap_keys.secret_key.secret_bytes())
        .build()
}

/// A parsed kind-1082 key-share rumor: the shared `team_root` secret plus which
/// board/channel it unlocks. This is the *inbound* half of membership — a member
/// gift-wraps one of these to a new member ([`wrap_keyshare`]), nostrdb auto-unwraps
/// the `1059` to the `1082` rumor (the recipient's key is registered via
/// `ndb.add_key`), and the app reads it here to decide whether to register the root
/// (an app-owned accept policy — see `docs/nip-sns-sealed-shared-storage.md`).
pub struct KeyShare {
    /// The 32-byte shared channel secret to register with `ndb.add_team_root`.
    pub team_root: [u8; 32],
    /// The board coordinate this root unlocks (`30619:<owner-hex>:<board-id>`), if
    /// the share names one. A key-share always should, but it's a wire value so a
    /// missing/blank `a` tag is tolerated rather than failing the whole parse.
    pub board_addr: Option<String>,
    /// The rotation generation, if the share names one (see *Rotation* in the SNS
    /// doc). `None` for a first-generation share.
    pub epoch: Option<u32>,
}

/// Parse a kind-1082 key-share rumor into its [`KeyShare`]. Returns `None` if the
/// note isn't a `1082` or carries no decodable `team_root` — the one field the
/// caller can't proceed without.
///
/// The `team_root` tag is a hex or bech32 encoding of the 32-byte secret (per the
/// SNS doc); both are accepted. The `a` (board coordinate) and `epoch` tags are
/// optional and surfaced verbatim for the app's registration policy.
pub fn parse_keyshare(note: &Note) -> Option<KeyShare> {
    if note.kind() != KEYSHARE_KIND {
        return None;
    }
    let mut team_root = None;
    let mut board_addr = None;
    let mut epoch = None;
    for tag in note.tags() {
        match tag.get_str(0) {
            // A 64-char hex root is stored by nostrdb as a binary *id* element, not
            // a string, so read `get_id` first; a bech32 (`nsec…`) root stays a
            // string and falls through to [`parse_team_root`].
            Some("team_root") => {
                team_root = tag
                    .get_id(1)
                    .copied()
                    .or_else(|| tag.get_str(1).and_then(parse_team_root))
            }
            Some("a") => board_addr = tag.get_str(1).filter(|s| !s.is_empty()).map(str::to_owned),
            Some("epoch") => epoch = tag.get_str(1).and_then(|s| s.parse().ok()),
            _ => {}
        }
    }
    Some(KeyShare {
        team_root: team_root?,
        board_addr,
        epoch,
    })
}

/// Decode a `team_root` tag value (hex or bech32) into the raw 32-byte secret.
/// Tries hex first (the canonical form the SNS builders emit), then a bech32
/// secret key (e.g. `nsec…`) so a human-pasted share still resolves.
fn parse_team_root(s: &str) -> Option<[u8; 32]> {
    if let Ok(bytes) = hex::decode(s)
        && let Ok(root) = <[u8; 32]>::try_from(bytes.as_slice())
    {
        return Some(root);
    }
    crate::SecretKey::from_bech32(s)
        .ok()
        .map(|sk| sk.secret_bytes())
}

/// HMAC-SHA256(key=salt, msg=ikm) → 32-byte key (HKDF-Extract only, matching the
/// nostrdb C derivation). Shared with [`crate::pns`]'s scheme.
fn hkdf_extract(ikm: &[u8; 32], salt: &[u8]) -> [u8; 32] {
    let (prk, _) = Hkdf::<Sha256>::extract(Some(salt), ikm);
    let mut out = [0u8; 32];
    out.copy_from_slice(&prk);
    out
}

/// Convert an enostr [`Pubkey`] (32-byte x-only) to a `nostr` [`PublicKey`].
fn nostrcrate_pk(pk: &Pubkey) -> Option<PublicKey> {
    PublicKey::from_slice(pk.bytes()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::nips::nip19::ToBech32;
    use nostr::util::JsonUtil;
    use nostr::{Event, Kind};

    fn test_root() -> [u8; 32] {
        let mut root = [0u8; 32];
        root[0] = 0x11;
        root[31] = 0x22;
        root
    }

    /// Re-implements nostrdb's ingest peel (symmetric envelope decrypt → verify
    /// and ECDH-decrypt the kind-13 seal) so a [`wrap_rumor`] can be round-tripped
    /// in-process. Production never does this — nostrdb unwraps on ingest and the
    /// app only ever sees the stored rumor. Returns the verified author and the
    /// inner rumor JSON.
    fn unwrap_envelope(keys: &SnsKeys, envelope: &Note) -> Option<(Pubkey, String)> {
        if envelope.kind() != SNS_ENVELOPE_KIND {
            return None;
        }
        let payload = BASE64.decode(envelope.content()).ok()?;
        let seal_bytes = v2::decrypt_to_bytes(&keys.envelope_key, &payload).ok()?;
        let seal = Event::from_json(String::from_utf8(seal_bytes).ok()?).ok()?;
        if seal.kind != Kind::from(SEAL_KIND) {
            return None;
        }
        seal.verify().ok()?;
        let rumor_json =
            nip44::decrypt(&keys.team_keypair.secret_key, &seal.pubkey, &seal.content).ok()?;
        Some((Pubkey::new(seal.pubkey.to_bytes()), rumor_json))
    }

    /// `wrap_keyshare` produces a well-formed outer gift wrap: kind 1059, authored
    /// by a throwaway ephemeral key (never the sharer), `p`-tagged to the recipient
    /// so relays route it to their inbox. The inner correctness — that nostrdb
    /// unwraps it to a `1082` carrying the right `team_root`/`a`/`epoch`, attributed
    /// to the sharer — is verified against a real `Ndb`'s auto-unwrap in notedeck's
    /// `teams.rs` (the app crate owns the ingest engine; we never unwrap in-process).
    #[test]
    fn wrap_keyshare_builds_a_routable_giftwrap() {
        let sender = FullKeypair::generate();
        let recipient = FullKeypair::generate();
        let board =
            "30619:d6623502bcf67f6758e25080111ad9221181c33cfcba14d74dc9e3784ecfe1f7:headway";

        let giftwrap = wrap_keyshare(
            &sender,
            &recipient.pubkey,
            &test_root(),
            board,
            Some(2),
            100,
        )
        .expect("giftwrap");
        assert_eq!(giftwrap.kind(), 1059);
        // Outer author is a throwaway ephemeral key, never the sharer.
        assert_ne!(giftwrap.pubkey(), sender.pubkey.bytes());
        // Routed to the recipient's inbox. A 64-hex `p` value is stored by nostrdb
        // as a binary id element, so read it with `get_id`, not `get_str`.
        assert!(giftwrap
            .tags()
            .into_iter()
            .any(|t| t.get_str(0) == Some("p") && t.get_id(1) == Some(recipient.pubkey.bytes())));
    }

    #[test]
    fn derive_is_deterministic() {
        let root = test_root();
        let a = derive_sns_keys(&root).expect("keys");
        let b = derive_sns_keys(&root).expect("keys");
        assert_eq!(a.team_keypair.pubkey, b.team_keypair.pubkey);
        assert_eq!(a.envelope_key.as_bytes(), b.envelope_key.as_bytes());
    }

    /// Fixed vector so the nostrdb C `ndb_ingester_add_sns_key` has a target to
    /// match. `team_root` = 0x11,0x00…0x00,0x22.
    /// A board root is deterministic in `(secret, label)`, isolates boards by
    /// label, differs per account, and is always a valid channel root. The
    /// determinism is the load-bearing property: it is what lets two of an
    /// account's devices seed the same auto-created board into one SNS channel.
    #[test]
    fn board_root_deterministic_scoped_and_valid() {
        let secret = FullKeypair::generate().secret_key.secret_bytes();
        let root = derive_board_root(&secret, "headway");

        assert_eq!(
            root,
            derive_board_root(&secret, "headway"),
            "same secret+label → same root (cross-device convergence)"
        );
        assert_ne!(
            root,
            derive_board_root(&secret, "work"),
            "distinct labels → isolated per-board roots"
        );
        let other = FullKeypair::generate().secret_key.secret_bytes();
        assert_ne!(
            root,
            derive_board_root(&other, "headway"),
            "distinct account secrets → distinct roots"
        );
        assert!(
            derive_sns_keys(&root).is_some(),
            "a derived root is always a valid secp256k1 channel root"
        );
    }

    #[test]
    fn derive_matches_fixed_vector() {
        let keys = derive_sns_keys(&test_root()).expect("keys");
        let expected_team_pubkey =
            "d6623502bcf67f6758e25080111ad9221181c33cfcba14d74dc9e3784ecfe1f7";
        assert_eq!(
            hex::encode(keys.team_keypair.pubkey.bytes()),
            expected_team_pubkey,
            "SNS team pubkey must match the nostrdb C derivation"
        );
    }

    #[test]
    fn wrap_unwrap_roundtrip_attributes_the_member() {
        let keys = derive_sns_keys(&test_root()).expect("keys");
        let member = FullKeypair::generate();
        let rumor = format!(
            r#"{{"kind":1621,"pubkey":"{}","content":"a card","tags":[],"created_at":42}}"#,
            member.pubkey.hex()
        );

        let envelope = wrap_rumor(&keys, &member, &rumor, 100).expect("envelope");
        assert_eq!(envelope.kind(), SNS_ENVELOPE_KIND);
        // Envelope is authored by the shared channel keypair, not the member.
        assert_eq!(envelope.pubkey(), keys.team_keypair.pubkey.bytes());

        let (author, rumor_json) = unwrap_envelope(&keys, &envelope).expect("unwrap");
        assert_eq!(rumor_json, rumor);
        // Authorship survives the wrap: attributed to the real member.
        assert_eq!(author, member.pubkey);
    }

    #[test]
    fn wrong_root_cannot_unwrap() {
        let keys = derive_sns_keys(&test_root()).expect("keys");
        let member = FullKeypair::generate();
        let envelope = wrap_rumor(&keys, &member, r#"{"kind":1}"#, 1).expect("envelope");

        let mut other = test_root();
        other[0] = 0x33;
        let other_keys = derive_sns_keys(&other).expect("keys");
        assert!(unwrap_envelope(&other_keys, &envelope).is_none());
    }

    /// Build a kind-1082 key-share rumor with the given `team_root` encoding,
    /// signed by a throwaway sharer key (the signature is irrelevant to parsing —
    /// nostrdb strips it on gift-wrap unwrap).
    fn keyshare_note(team_root_tag: &str, board_addr: &str, epoch: Option<&str>) -> Note<'static> {
        let sharer = FullKeypair::generate();
        let mut b = NoteBuilder::new()
            .kind(KEYSHARE_KIND)
            .content("")
            .start_tag()
            .tag_str("team_root")
            .tag_str(team_root_tag)
            .start_tag()
            .tag_str("a")
            .tag_str(board_addr);
        if let Some(epoch) = epoch {
            b = b.start_tag().tag_str("epoch").tag_str(epoch);
        }
        b.sign(&sharer.secret_key.secret_bytes())
            .build()
            .expect("keyshare note")
    }

    #[test]
    fn parse_keyshare_reads_hex_root_board_and_epoch() {
        let root = test_root();
        let board =
            "30619:d6623502bcf67f6758e25080111ad9221181c33cfcba14d74dc9e3784ecfe1f7:headway";
        let note = keyshare_note(&hex::encode(root), board, Some("2"));

        let share = parse_keyshare(&note).expect("parses");
        assert_eq!(share.team_root, root);
        assert_eq!(share.board_addr.as_deref(), Some(board));
        assert_eq!(share.epoch, Some(2));
    }

    /// A bech32 (`nsec…`) `team_root` decodes to the same 32 bytes as its hex form,
    /// so a human-pasted share resolves identically.
    #[test]
    fn parse_keyshare_reads_bech32_root() {
        let root = test_root();
        let nsec = crate::SecretKey::from_slice(&root)
            .expect("secret")
            .to_bech32()
            .expect("bech32");
        let note = keyshare_note(&nsec, "30619:owner:board", None);

        let share = parse_keyshare(&note).expect("parses");
        assert_eq!(share.team_root, root);
        assert_eq!(share.epoch, None);
    }

    /// A note of the wrong kind, or one missing a decodable `team_root`, is not a
    /// key-share.
    #[test]
    fn parse_keyshare_rejects_non_keyshare() {
        // Wrong kind (an envelope, not a share).
        let keys = derive_sns_keys(&test_root()).expect("keys");
        let member = FullKeypair::generate();
        let envelope = wrap_rumor(&keys, &member, r#"{"kind":1}"#, 1).expect("envelope");
        assert!(parse_keyshare(&envelope).is_none());

        // Right kind but an undecodable root.
        let note = keyshare_note("not-a-key", "30619:owner:board", None);
        assert!(parse_keyshare(&note).is_none());
    }

    /// Ingesting an SNS envelope whose inner rumor id already exists as a *plaintext*
    /// note promotes that stored note to a team-sealed rumor **in place** — same
    /// note_key/id, no duplicate — and does **not** re-notify subscriptions (the
    /// content is unchanged). This is what lets a plaintext board be migrated to SNS
    /// by re-sealing its existing notes; see nostrdb.c `ndb_write_note`.
    #[tokio::test]
    async fn promote_plaintext_note_to_sealed_rumor_in_place() {
        use nostrdb::{Config, Filter, Ndb, Transaction};

        let dir = tempfile::TempDir::new().expect("tempdir");
        let ndb = Ndb::new(dir.path().to_str().unwrap(), &Config::new()).expect("ndb");

        let member = FullKeypair::generate();
        let keys = derive_sns_keys(&test_root()).expect("keys");
        let team_pub = keys.team_keypair.pubkey;

        // 1. Ingest a plaintext kind-1 note authored by the member.
        let plaintext = NoteBuilder::new()
            .kind(1)
            .content("promote me")
            .created_at(1700000000)
            .sign(&member.secret_key.secret_bytes())
            .build()
            .expect("note");
        let note_id = *plaintext.id();
        let pjson = plaintext.json().expect("pjson");

        let sub = ndb
            .subscribe(&[Filter::new().kinds(vec![1]).build()])
            .expect("sub");
        let waiter = ndb.wait_for_all_notes(sub, 1);
        ndb.process_event(&format!("[\"EVENT\",\"p\",{pjson}]"))
            .expect("ingest plaintext");
        waiter.await.expect("plaintext ingested");

        // Stored as a plaintext (non-rumor) note.
        {
            let txn = Transaction::new(&ndb).expect("txn");
            let n = ndb.get_note_by_id(&txn, &note_id).expect("note");
            assert!(!n.is_rumor());
        }

        // 2. Register the team root and ingest an SNS envelope wrapping the SAME
        //    note (same content -> same recomputed rumor id).
        assert!(ndb.add_team_root(&test_root()));
        let envelope = wrap_rumor(&keys, &member, &pjson, 1700000000).expect("envelope");
        let ejson = envelope.json().expect("ejson");

        // Watch kind-1 to prove the promote does NOT re-notify, and wait on the
        // envelope's own (kind-1081) write — committed in the same writer batch as
        // the promote — to know the promote has landed.
        let rumor_sub = ndb
            .subscribe(&[Filter::new().kinds(vec![1]).build()])
            .expect("rumor sub");
        let env_sub = ndb
            .subscribe(&[Filter::new().kinds(vec![1081]).build()])
            .expect("env sub");
        let env_waiter = ndb.wait_for_all_notes(env_sub, 1);
        ndb.process_event(&format!("[\"EVENT\",\"e\",{ejson}]"))
            .expect("ingest envelope");
        env_waiter.await.expect("envelope ingested");

        // 3. The plaintext note was promoted in place: still exactly one kind-1
        //    note, now a rumor sealed under the team, same id — and no re-notify.
        let txn = Transaction::new(&ndb).expect("txn");
        let ones = ndb
            .query(&txn, &[Filter::new().kinds(vec![1]).build()], 10)
            .expect("query");
        assert_eq!(ones.len(), 1, "promote must not create a duplicate note");
        let n = &ones[0].note;
        assert!(n.is_rumor(), "note is now a rumor");
        assert_eq!(n.content(), "promote me");
        assert_eq!(n.id(), &note_id, "id preserved");
        assert_eq!(
            n.rumor_receiver_pubkey(),
            Some(team_pub.bytes()),
            "sealed under the team channel"
        );
        assert!(
            ndb.poll_for_notes(rumor_sub, 10).is_empty(),
            "promote must not notify subscriptions"
        );
    }

    /// A rumor over 32KB, sealed by the Rust `nostr` crate, unwraps in nostrdb.
    /// Both NIP-44 layers are past 32768 bytes here: the seal carries the 40KB
    /// rumor, and the wrap carries the seal's base64 of it. nostrdb's C `unpad`
    /// used to compute the padded length in `uint16_t`, which wraps above 32768,
    /// so it rejected spec-padded payloads of 32769..57344 bytes ("padding size
    /// is wrong") and the rumor never appeared.
    #[tokio::test]
    async fn rumor_over_32kb_unwraps() {
        use nostrdb::{Config, Filter, Ndb, Transaction};

        let dir = tempfile::TempDir::new().expect("tempdir");
        let ndb = Ndb::new(dir.path().to_str().unwrap(), &Config::new()).expect("ndb");
        assert!(ndb.add_team_root(&test_root()));

        let member = FullKeypair::generate();
        let keys = derive_sns_keys(&test_root()).expect("keys");
        let content = "x".repeat(40_000);
        let rumor = NoteBuilder::new()
            .kind(1)
            .content(&content)
            .created_at(1700000000)
            .sign(&member.secret_key.secret_bytes())
            .build()
            .expect("note");
        let rjson = rumor.json().expect("rjson");
        let envelope = wrap_rumor(&keys, &member, &rjson, 1700000000).expect("envelope");
        let ejson = envelope.json().expect("ejson");

        let sub = ndb
            .subscribe(&[Filter::new().kinds(vec![1]).build()])
            .expect("sub");
        let waiter = ndb.wait_for_all_notes_within(sub, 1, std::time::Duration::from_secs(10));
        ndb.process_event(&format!("[\"EVENT\",\"big\",{ejson}]"))
            .expect("ingest envelope");
        waiter.await.expect("a 40KB rumor unwraps");

        let txn = Transaction::new(&ndb).expect("txn");
        let ones = ndb
            .query(&txn, &[Filter::new().kinds(vec![1]).build()], 1)
            .expect("query");
        assert!(ones[0].note.is_rumor());
        assert_eq!(ones[0].note.content(), content);
    }

    /// A migrated board is many envelopes (its definition plus every card). If
    /// they all land before the team root is registered, a single `process_sns`
    /// catch-up — what notedeck's `register_teams` runs after `add_team_root` —
    /// must peel *all* of them. Regression test for `ndb_process_sns` dispatching
    /// only the first un-unwrapped envelope, which stranded migrated boards on
    /// "Loading shared board…" (the board-def, oldest, stayed sealed) forever.
    #[tokio::test]
    async fn one_process_sns_call_peels_all_stored_envelopes() {
        use nostrdb::{Config, Filter, Ndb, Transaction};

        let dir = tempfile::TempDir::new().expect("tempdir");
        let ndb = Ndb::new(dir.path().to_str().unwrap(), &Config::new()).expect("ndb");
        let member = FullKeypair::generate();
        let keys = derive_sns_keys(&test_root()).expect("keys");

        const N: usize = 5;
        let env_sub = ndb
            .subscribe(&[Filter::new().kinds(vec![1081]).build()])
            .expect("env sub");
        let env_waiter = ndb.wait_for_all_notes(env_sub, N as u32);
        for i in 0..N {
            let rumor = NoteBuilder::new()
                .kind(1)
                .content(&format!("card {i}"))
                .created_at(1700000000 + i as u64)
                .sign(&member.secret_key.secret_bytes())
                .build()
                .expect("note");
            let rjson = rumor.json().expect("rjson");
            let envelope =
                wrap_rumor(&keys, &member, &rjson, 1700000000 + i as u64).expect("envelope");
            let ejson = envelope.json().expect("ejson");
            ndb.process_event(&format!("[\"EVENT\",\"e{i}\",{ejson}]"))
                .expect("ingest envelope");
        }
        env_waiter.await.expect("all envelopes ingested");

        // Register the root (all envelopes already stored, un-unwrapped) and run a
        // single process_sns catch-up — exactly what register_teams does.
        assert!(ndb.add_team_root(&test_root()));
        {
            let txn = Transaction::new(&ndb).expect("txn");
            ndb.process_sns(&txn);
        }

        // Reprocessing is async; poll (bounded) until every rumor peels.
        let peeled = |ndb: &Ndb| {
            let txn = Transaction::new(ndb).expect("txn");
            ndb.query(&txn, &[Filter::new().kinds(vec![1]).build()], 100)
                .expect("query")
                .into_iter()
                .filter(|r| r.note.is_rumor())
                .count()
        };
        for _ in 0..300 {
            if peeled(&ndb) == N {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            peeled(&ndb),
            N,
            "one process_sns call must peel ALL stored envelopes, not just one"
        );
    }

    /// The same drain-all fix for `ndb_process_giftwraps`: a backlog of kind-1059
    /// gift wraps (here several shared-board key-shares) can arrive before the
    /// recipient's key is registered. A single `process_giftwraps` after `add_key`
    /// must peel all of them — else only one board's key-share unwraps into a 1082
    /// and only one board joins the roster.
    #[tokio::test]
    async fn one_process_giftwraps_call_peels_all_stored_wraps() {
        use nostrdb::{Config, Filter, Ndb, Transaction};

        let dir = tempfile::TempDir::new().expect("tempdir");
        let ndb = Ndb::new(dir.path().to_str().unwrap(), &Config::new()).expect("ndb");
        let sender = FullKeypair::generate();
        let recipient = FullKeypair::generate();

        const N: usize = 5;
        let wrap_sub = ndb
            .subscribe(&[Filter::new().kinds(vec![1059]).build()])
            .expect("wrap sub");
        let wrap_waiter = ndb.wait_for_all_notes(wrap_sub, N as u32);
        for i in 0..N {
            // Distinct boards → distinct key-share rumors, so each gift wrap is a
            // separate stored note to peel.
            let board = format!("30619:{}:board{i}", hex::encode(sender.pubkey.bytes()));
            let giftwrap = wrap_keyshare(
                &sender,
                &recipient.pubkey,
                &test_root(),
                &board,
                None,
                100 + i as u64,
            )
            .expect("giftwrap");
            let gjson = giftwrap.json().expect("gjson");
            ndb.process_event(&format!("[\"EVENT\",\"g{i}\",{gjson}]"))
                .expect("ingest giftwrap");
        }
        wrap_waiter.await.expect("all gift wraps ingested");

        // Register the recipient key (all wraps already stored) and run a single
        // process_giftwraps catch-up.
        assert!(ndb.add_key(&recipient.secret_key.secret_bytes()));
        {
            let txn = Transaction::new(&ndb).expect("txn");
            ndb.process_giftwraps(&txn);
        }

        let peeled = |ndb: &Ndb| {
            let txn = Transaction::new(ndb).expect("txn");
            ndb.query(
                &txn,
                &[Filter::new().kinds(vec![KEYSHARE_KIND as u64]).build()],
                100,
            )
            .expect("query")
            .into_iter()
            .filter(|r| r.note.is_rumor())
            .count()
        };
        for _ in 0..300 {
            if peeled(&ndb) == N {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            peeled(&ndb),
            N,
            "one process_giftwraps call must peel ALL stored gift wraps, not just one"
        );
    }
}
