//! NIP-42 AUTH from the client side: the one-shot sync [`Relay`] and the
//! long-lived [`Session`] against the embedded relay locked down by an
//! [`AccessPolicy`], the same policy wss://sns.damus.io runs.
//!
//! The relay challenges on connect and refuses everything until a member
//! answers, so each case here is "with the right signer it syncs; without one,
//! or with the wrong one, it fails fast and says why".

use std::time::Duration;

use futures_util::StreamExt;
use negentropy::NegentropyStorageVector;
use nostrdb::{Config, Filter, Ndb, NoteBuilder, SubscriptionStream, Transaction};
use nostrdb_net::FullKeypair;
use nostrdb_net::relay::auth::AuthSigner;
use nostrdb_net::relay::server::{AccessPolicy, RelayHandle, spawn_with_policy};
use nostrdb_net::relay::sync::{AuthRequired, Relay, Restricted, Session};

/// The name the relay serves as. Tests dial `ws://127.0.0.1:<port>`, so every
/// signer names this in its `relay` tag, as a client behind a proxy would.
const SERVICE: &str = "wss://sns.example";
/// The kind the relay accepts: an SNS envelope.
const KIND: u32 = 1081;
/// Far longer than any step here should take; a hang is a failure, not a wait.
const STEP: Duration = Duration::from_secs(10);
/// How fast a refusal must come back: well inside the relay's 10s AUTH
/// deadline, so the error is the client's, not the relay hanging up on it.
const FAST: Duration = Duration::from_secs(3);

fn temp_ndb() -> (tempfile::TempDir, Ndb) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ndb = Ndb::new(
        dir.path().to_str().unwrap(),
        &Config::new().set_ingester_threads(1),
    )
    .expect("ndb");
    (dir, ndb)
}

/// A relay serving [`KIND`] to `members`, and its db.
fn relay(members: &[&FullKeypair]) -> (RelayHandle, Ndb, tempfile::TempDir) {
    let (dir, ndb) = temp_ndb();
    let policy = AccessPolicy::new(SERVICE, members.iter().map(|kp| kp.pubkey), [KIND]);
    let handle =
        spawn_with_policy(ndb.clone(), "127.0.0.1:0".parse().unwrap(), policy).expect("spawn");
    (handle, ndb, dir)
}

fn signer(kp: &FullKeypair) -> AuthSigner {
    AuthSigner::new(kp.clone()).with_service_url(SERVICE)
}

/// A signed [`KIND`] event: its bare JSON and id.
fn note(kp: &FullKeypair, content: &str) -> (String, [u8; 32]) {
    let note = NoteBuilder::new()
        .kind(KIND)
        .content(content)
        .sign(&kp.secret_key.secret_bytes())
        .build()
        .expect("note");
    (note.json().expect("json"), *note.id())
}

/// Store `n` events straight into the relay's db, bypassing the wire. Returns
/// their ids once they are queryable.
async fn seed(ndb: &Ndb, kp: &FullKeypair, n: usize) -> Vec<[u8; 32]> {
    let mut ids = Vec::new();
    for i in 0..n {
        let (json, id) = note(kp, &format!("seed-{i}"));
        ndb.process_client_event(&format!(r#"["EVENT",{json}]"#))
            .expect("ingest");
        ids.push(id);
    }
    for id in &ids {
        assert!(await_note(ndb, *id).await, "seed should land");
    }
    ids
}

fn filter_json() -> String {
    format!(r#"{{"kinds":[{KIND}]}}"#)
}

fn empty_set() -> NegentropyStorageVector {
    let mut storage = NegentropyStorageVector::new();
    storage.seal().expect("seal");
    storage
}

/// Wait (bounded by [`STEP`]) until note `id` is queryable in `ndb`.
async fn await_note(ndb: &Ndb, id: [u8; 32]) -> bool {
    let filter = Filter::new().kinds([KIND as u64]).build();
    let sub = ndb.subscribe(&[filter]).expect("subscribe");
    let mut stream = SubscriptionStream::new(ndb.clone(), sub);
    tokio::time::timeout(STEP, async {
        loop {
            {
                let txn = Transaction::new(ndb).expect("txn");
                if ndb.get_note_by_id(&txn, &id).is_ok() {
                    return true;
                }
            }
            if stream.next().await.is_none() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// The error `result` should be, or a panic naming `what` should have failed.
fn refused<T>(
    result: nostrdb_net::relay::sync::Result<T>,
    what: &str,
) -> Box<dyn std::error::Error> {
    match result {
        Ok(_) => panic!("{what}"),
        Err(e) => e,
    }
}

fn has_note(ndb: &Ndb, id: &[u8; 32]) -> bool {
    let txn = Transaction::new(ndb).expect("txn");
    ndb.get_note_by_id(&txn, id).is_ok()
}

/// A member's one-shot connection answers the challenge, and reconcile,
/// sync_into and publish all go through — each was refused `auth-required:`
/// or raced the AUTH, and was retried once the relay accepted it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_relay_authenticates_then_syncs_and_publishes() {
    let member = FullKeypair::generate();
    let (handle, relay_ndb, _dir) = relay(&[&member]);
    let seeded = seed(&relay_ndb, &member, 3).await;

    // Reconcile first, while the AUTH is still in flight.
    let mut relay = Relay::connect_with_auth(&handle.url(), Some(signer(&member)))
        .await
        .expect("connect");
    let diff = tokio::time::timeout(STEP, relay.reconcile(&filter_json(), empty_set()))
        .await
        .expect("reconcile within STEP")
        .expect("reconcile");
    assert_eq!(diff.need.len(), seeded.len(), "relay's history, by id");

    let (_dir, local) = temp_ndb();
    let got = relay
        .sync_into(&local, &filter_json())
        .await
        .expect("sync_into");
    assert_eq!(got.len(), seeded.len());

    let (json, id) = note(&member, "published");
    relay
        .publish(&[format!(r#"["EVENT",{json}]"#)])
        .await
        .expect("publish");
    assert!(await_note(&relay_ndb, id).await, "the publish was stored");

    // A fresh connection gets a fresh challenge, and REQ is its first request.
    let mut relay = Relay::connect_with_auth(&handle.url(), Some(signer(&member)))
        .await
        .expect("reconnect");
    let (_dir, local) = temp_ndb();
    let got = relay
        .sync_into(&local, &filter_json())
        .await
        .expect("sync_into on a second connection");
    assert_eq!(got.len(), seeded.len() + 1);

    // And a publish as the first request.
    let mut relay = Relay::connect_with_auth(&handle.url(), Some(signer(&member)))
        .await
        .expect("reconnect");
    let (json, id) = note(&member, "first thing on the wire");
    relay
        .publish(&[format!(r#"["EVENT",{json}]"#)])
        .await
        .expect("publish as the first request");
    assert!(await_note(&relay_ndb, id).await);
}

/// Without a signer the connection never authenticates, and every request
/// fails at once with a typed [`AuthRequired`] — not a hang until the relay's
/// AUTH deadline hangs up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_relay_without_signer_fails_fast_with_auth_required() {
    let member = FullKeypair::generate();
    let (handle, relay_ndb, _dir) = relay(&[&member]);
    seed(&relay_ndb, &member, 1).await;

    let mut relay = Relay::connect(&handle.url()).await.expect("connect");
    let err = refused(
        tokio::time::timeout(FAST, relay.reconcile(&filter_json(), empty_set()))
            .await
            .expect("reconcile refused fast"),
        "reconcile should fail",
    );
    let auth = err.downcast_ref::<AuthRequired>().expect("AuthRequired");
    assert!(auth.reason.starts_with("auth-required:"), "{err}");

    let (_dir, local) = temp_ndb();
    let err = refused(
        tokio::time::timeout(FAST, relay.sync_into(&local, &filter_json()))
            .await
            .expect("sync_into refused fast"),
        "sync_into should fail",
    );
    assert!(err.is::<AuthRequired>(), "{err}");

    let (json, id) = note(&member, "unauthed");
    let err = refused(
        tokio::time::timeout(FAST, relay.publish(&[format!(r#"["EVENT",{json}]"#)]))
            .await
            .expect("publish refused fast"),
        "publish should fail",
    );
    assert!(err.is::<AuthRequired>(), "{err}");
    assert!(!has_note(&relay_ndb, &id), "nothing stored without AUTH");
}

/// A valid AUTH from a pubkey outside the members list is accepted as an
/// identity and then refused as one: a typed [`Restricted`], fast.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_shot_relay_non_member_is_restricted() {
    let member = FullKeypair::generate();
    let stranger = FullKeypair::generate();
    let (handle, relay_ndb, _dir) = relay(&[&member]);
    seed(&relay_ndb, &member, 1).await;

    let mut relay = Relay::connect_with_auth(&handle.url(), Some(signer(&stranger)))
        .await
        .expect("connect");
    let err = refused(
        tokio::time::timeout(FAST, relay.reconcile(&filter_json(), empty_set()))
            .await
            .expect("reconcile refused fast"),
        "reconcile should fail",
    );
    let restricted = err.downcast_ref::<Restricted>().expect("Restricted");
    assert!(restricted.reason.starts_with("restricted:"), "{err}");

    let (_dir, local) = temp_ndb();
    let err = refused(
        tokio::time::timeout(FAST, relay.sync_into(&local, &filter_json()))
            .await
            .expect("sync_into refused fast"),
        "sync_into should fail",
    );
    assert!(err.is::<Restricted>(), "{err}");

    let (json, _) = note(&stranger, "not a member");
    let err = refused(
        tokio::time::timeout(FAST, relay.publish(&[format!(r#"["EVENT",{json}]"#)]))
            .await
            .expect("publish refused fast"),
        "publish should fail",
    );
    assert!(err.is::<Restricted>(), "{err}");
}

/// A [`Session`] with a member's signer for the relay: the history backfill
/// authenticates on its own connection, the live `REQ` the relay closed before
/// AUTH is re-sent after it, and a publish refused before AUTH is re-sent and
/// stored. Then the relay drops every connection, and the reconnect answers a
/// fresh challenge, so live events keep coming.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_authenticates_backfills_lives_publishes_and_reauths() {
    let member = FullKeypair::generate();
    let other = FullKeypair::generate();
    let seeder = FullKeypair::generate();
    let (handle, relay_ndb, _dir) = relay(&[&member, &other, &seeder]);
    let url = handle.url();
    let seeded = seed(&relay_ndb, &seeder, 2).await;

    let (_dir, local) = temp_ndb();
    let session = Session::new(local.clone());
    session.set_auth_signer(url.clone(), Some(signer(&member)));
    // History is the seeder's alone, so `other`'s events below can only come
    // over the live REQ — not over a backfill, which a reconnect also re-runs.
    let live = Filter::new().kinds([KIND as u64]).build();
    let history = Filter::new()
        .kinds([KIND as u64])
        .authors([seeder.pubkey.bytes()])
        .build();
    session.set_subscription("sns", url.clone(), vec![live], vec![history]);
    let (json, published) = note(&member, "from the session");
    session.publish(json, vec![url.clone()]);

    tokio::time::timeout(STEP, session.wait_for_sync())
        .await
        .expect("backfill settles");
    assert!(
        !session.backfill_status().borrow().failed,
        "backfill authenticated"
    );
    for id in &seeded {
        assert!(has_note(&local, id), "history came down");
    }
    assert!(
        await_note(&relay_ndb, published).await,
        "the held publish was re-sent after AUTH and stored"
    );

    // A live event from another member, over the re-sent REQ.
    let mut writer = Relay::connect_with_auth(&url, Some(signer(&other)))
        .await
        .expect("writer");
    let (json, live_id) = note(&other, "live");
    writer
        .publish(&[format!(r#"["EVENT",{json}]"#)])
        .await
        .expect("writer publish");
    assert!(
        await_note(&local, live_id).await,
        "live REQ re-sent after AUTH"
    );

    // Drop the session's connection; the keepalive reconnects it, and only a
    // fresh AUTH gets its REQ served again.
    handle.drop_connections();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut writer = Relay::connect_with_auth(&url, Some(signer(&other)))
        .await
        .expect("writer after drop");
    let (json, after_id) = note(&other, "after the reconnect");
    writer
        .publish(&[format!(r#"["EVENT",{json}]"#)])
        .await
        .expect("writer publish after drop");
    assert!(
        await_note(&local, after_id).await,
        "reconnect re-authenticated and the live REQ is served again"
    );
}

/// A [`Session`] given no signer for an AUTH relay never authenticates: its
/// backfill fails (and says so) instead of retrying, nothing syncs, and its
/// publish isn't stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_without_signer_never_authenticates() {
    let member = FullKeypair::generate();
    let (handle, relay_ndb, _dir) = relay(&[&member]);
    let url = handle.url();
    let seeded = seed(&relay_ndb, &member, 1).await;

    let (_dir, local) = temp_ndb();
    let session = Session::new(local.clone());
    let live = Filter::new().kinds([KIND as u64]).build();
    let history = Filter::new().kinds([KIND as u64]).build();
    session.set_subscription("sns", url.clone(), vec![live], vec![history]);
    let (json, published) = note(&member, "no signer");
    session.publish(json, vec![url.clone()]);

    tokio::time::timeout(FAST, session.wait_for_sync())
        .await
        .expect("an auth-required backfill gives up at once, not after retries");
    assert!(session.backfill_status().borrow().failed);
    assert!(!has_note(&local, &seeded[0]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!has_note(&relay_ndb, &published));
}
