//! The windowed reconciles against a real strfry, not the in-process relay.
//!
//! The in-process relay is nostrdb on both sides, so it can share the client's
//! mistakes. strfry is the relay these paths run against in production
//! (relay.jb55.com), and it differs in the two ways that matter here: its
//! `until` is NIP-01's inclusive bound where nostrdb's is exclusive, and it
//! refuses a `NEG-OPEN` matching more than `relay.negentropy.maxSyncEvents`.
//! Each test starts its own strfry with a cap of [`CAP`] and a throwaway db, so
//! the cap refusal and the bisection run for real.
//!
//! The test kinds are regular (1000–9999), not addressable: strfry replaces
//! an addressable event by its `d` tag, so a run of them would collapse to one.
//!
//! Events are placed one per second across a dense range, so every split
//! boundary the bisection lands on inside the range carries an event. Before the
//! windows were made inclusive, the boundary events were pulled again on every
//! run (strfry served them, the client's window never held them) and a
//! client-only one was never pushed.
//!
//! strfry comes from `STRFRY_BIN` (a path to the binary), or else from podman
//! running the upstream image (`STRFRY_IMAGE`, default
//! `ghcr.io/hoytech/strfry:latest`). With neither, each test prints why and
//! passes. They are `#[ignore]`d so a plain `cargo test` never starts a
//! container. Run them with:
//!
//!   cargo test -p nostrdb_net --test strfry_windowed -- --ignored --nocapture

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use nostrdb::{Config, Filter, Ndb, NoteBuilder, Transaction};
use nostrdb_net::relay::sync::{
    Progress, Relay, SyncEvent, TooManyResults, pull_reconcile_windowed_with_progress,
    reconcile_sync_with_progress,
};
use nostrdb_net::{Keypair, Pubkey, SecretKey};
use serde_json::json;
use tempfile::TempDir;

/// strfry's `relay.negentropy.maxSyncEvents` for every test: a `NEG-OPEN`
/// matching more than this many events is refused with `too many query results`.
const CAP: usize = 20;

/// First `created_at` of the dense range the tests place events in.
const BASE: u64 = 1_000_000;

/// The key every test event is signed with.
const SECRET: [u8; 32] = [9u8; 32];

/// The image podman runs when `STRFRY_BIN` is unset.
const DEFAULT_IMAGE: &str = "ghcr.io/hoytech/strfry:latest";

/// A strfry config for tests: the given db path, port and negentropy cap, and
/// no age limits, since the test events are stamped in 1970.
fn strfry_conf(db: &str, port: u16) -> String {
    format!(
        r#"db = "{db}"
dbParams {{
    maxreaders = 256
    mapsize = 1073741824
}}
events {{
    rejectEventsNewerThanSeconds = 4000000000
    rejectEventsOlderThanSeconds = 4000000000
    rejectEphemeralEventsOlderThanSeconds = 60
}}
relay {{
    bind = "0.0.0.0"
    port = {port}
    nofiles = 0
    negentropy {{
        enabled = true
        maxSyncEvents = {CAP}
    }}
}}
"#
    )
}

/// How a test's strfry is running, so [`Drop`] can stop it.
enum Runner {
    /// A `STRFRY_BIN` child process.
    Bin(Child),
    /// A podman container, by name.
    Podman(String),
}

/// A running strfry with its own empty db, stopped on drop.
struct Strfry {
    url: String,
    runner: Runner,
    _dir: TempDir,
}

impl Strfry {
    /// Start a strfry, or `None` (with the reason printed) when there is no
    /// binary and no podman.
    async fn start() -> Option<Strfry> {
        let dir = TempDir::new().expect("tmp dir");
        let (runner, url) = match std::env::var("STRFRY_BIN") {
            Ok(bin) => start_bin(&bin, &dir),
            Err(_) if podman_available() => start_podman(&dir),
            Err(_) => {
                eprintln!("skipping: no strfry (set STRFRY_BIN, or install podman)");
                return None;
            }
        };
        let strfry = Strfry {
            url,
            runner,
            _dir: dir,
        };
        strfry.wait_ready().await;
        Some(strfry)
    }

    /// Wait until the relay accepts a websocket connection.
    async fn wait_ready(&self) {
        for _ in 0..300 {
            if Relay::connect(&self.url).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("strfry at {} never came up", self.url);
    }

    async fn connect(&self) -> Relay {
        Relay::connect(&self.url).await.expect("connect to strfry")
    }
}

impl Drop for Strfry {
    fn drop(&mut self) {
        match &mut self.runner {
            Runner::Bin(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            Runner::Podman(name) => {
                // A failing test's strfry log says what the relay made of it.
                if std::thread::panicking() {
                    let _ = Command::new("podman").args(["logs", name]).status();
                }
                let _ = Command::new("podman")
                    .args(["rm", "-f", name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

fn podman_available() -> bool {
    Command::new("podman")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A free localhost port, found by binding port 0 and letting it go.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

/// Run `bin` with a config pointing at a db under `dir`.
fn start_bin(bin: &str, dir: &TempDir) -> (Runner, String) {
    let db = dir.path().join("db");
    std::fs::create_dir_all(&db).expect("db dir");
    let port = free_port();
    let conf = dir.path().join("strfry.conf");
    std::fs::write(&conf, strfry_conf(&format!("{}/", db.display()), port)).expect("conf");
    let child = Command::new(bin)
        .arg("--config")
        .arg(&conf)
        .arg("relay")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn STRFRY_BIN");
    (Runner::Bin(child), format!("ws://127.0.0.1:{port}"))
}

/// Run the strfry image under podman, its db on a tmpfs and its port published
/// to a free localhost port.
fn start_podman(dir: &TempDir) -> (Runner, String) {
    let image = std::env::var("STRFRY_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_owned());
    let conf = dir.path().join("strfry.conf");
    std::fs::write(&conf, strfry_conf("/strfry-db/", 7777)).expect("conf");
    let port = free_port();
    let name = format!("nostrdb-net-strfry-{}-{port}", std::process::id());
    let status = Command::new("podman")
        .args(["run", "--rm", "-d", "--pull=missing", "--name", &name])
        .args(["-p", &format!("127.0.0.1:{port}:7777")])
        .args(["--tmpfs", "/strfry-db"])
        .args([
            "-v",
            &format!("{}:/etc/test-strfry.conf:ro,Z", conf.display()),
        ])
        .arg(&image)
        .args(["--config", "/etc/test-strfry.conf", "relay"])
        .stdout(Stdio::null())
        .status()
        .expect("run podman");
    assert!(status.success(), "podman run {image} failed");
    (Runner::Podman(name), format!("ws://127.0.0.1:{port}"))
}

fn author() -> Pubkey {
    Keypair::from_secret(SecretKey::from_slice(&SECRET).expect("seckey")).pubkey
}

/// A signed `["EVENT", …]` frame of `kind` at `created_at`. The same arguments
/// make the same id, which is how one event is placed on both sides.
fn event_frame(kind: u32, created_at: u64, content: &str) -> String {
    let note = NoteBuilder::new()
        .kind(kind)
        .content(content)
        .created_at(created_at)
        .sign(&SECRET)
        .build()
        .expect("note");
    format!(r#"["EVENT",{}]"#, note.json().expect("json"))
}

fn temp_ndb() -> (TempDir, Ndb) {
    let dir = TempDir::new().expect("tmp dir");
    let ndb = Ndb::new(dir.path().to_str().expect("path"), &Config::new()).expect("ndb");
    (dir, ndb)
}

/// Events of `kind` queryable in `ndb`.
fn count_kind(ndb: &Ndb, kind: u32) -> usize {
    let Ok(txn) = Transaction::new(ndb) else {
        return 0;
    };
    ndb.query(&txn, &[Filter::new().kinds([kind as u64]).build()], 100_000)
        .map(|r| r.len())
        .unwrap_or(0)
}

/// Ingest `frames` into `ndb` and wait until all of them are queryable.
async fn ingest(ndb: &Ndb, kind: u32, frames: &[String]) {
    let target = count_kind(ndb, kind) + frames.len();
    for frame in frames {
        ndb.process_client_event(frame).expect("ingest");
    }
    for _ in 0..1000 {
        if count_kind(ndb, kind) >= target {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("ingest of {} events never settled", frames.len());
}

/// The wire filter every test reconciles: one kind, the test author.
fn wire_filter(kind: u32) -> String {
    json!({ "kinds": [kind], "authors": [author().hex()] }).to_string()
}

/// Records a sync's events, for summing what it fetched, needed and pushed.
#[derive(Default)]
struct Recorder(std::cell::RefCell<Vec<SyncEvent>>);

impl Progress for Recorder {
    fn event(&self, ev: SyncEvent) {
        self.0.borrow_mut().push(ev);
    }
}

impl Recorder {
    fn sum(&self, pick: impl Fn(&SyncEvent) -> Option<usize>) -> usize {
        self.0.borrow().iter().filter_map(pick).sum()
    }

    fn fetched(&self) -> usize {
        self.sum(|ev| match ev {
            SyncEvent::Fetched { count } => Some(*count),
            _ => None,
        })
    }

    fn needed(&self) -> usize {
        self.sum(|ev| match ev {
            SyncEvent::Need { count } => Some(*count),
            _ => None,
        })
    }

    fn pushed(&self) -> usize {
        self.sum(|ev| match ev {
            SyncEvent::Pushed { count } => Some(*count),
            _ => None,
        })
    }

    fn splits(&self) -> usize {
        self.0
            .borrow()
            .iter()
            .filter(|ev| **ev == SyncEvent::Split)
            .count()
    }
}

/// strfry's `until` is inclusive, the premise of the whole fix: a REQ for the
/// one second `[BASE + 1, BASE + 1]` returns the event stamped on it. It also
/// lands without waiting out `sync_into`'s ingest timeout, since the
/// subscription it awaits on matches the boundary event too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real strfry: set STRFRY_BIN, or have podman"]
async fn strfry_until_is_inclusive() {
    const KIND: u32 = 1091;
    let Some(strfry) = Strfry::start().await else {
        return;
    };
    let mut relay = strfry.connect().await;
    let frames: Vec<String> = (0..3)
        .map(|i| event_frame(KIND, BASE + i, &format!("ev-{i}")))
        .collect();
    relay.publish(&frames).await.expect("seed strfry");

    let (_dir, ndb) = temp_ndb();
    let one_second = json!({ "kinds": [KIND], "since": BASE + 1, "until": BASE + 1 });
    let started = std::time::Instant::now();
    let got = relay
        .sync_into(&ndb, &one_second.to_string())
        .await
        .expect("REQ");
    assert_eq!(got.len(), 1, "strfry serves the event on `until`");
    assert_eq!(count_kind(&ndb, KIND), 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "sync_into waited out its ingest timeout: {:?}",
        started.elapsed()
    );
}

/// strfry's cap refusal still parses as [`TooManyResults`], so the windowed
/// paths still recognise it and bisect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real strfry: set STRFRY_BIN, or have podman"]
async fn strfry_cap_refusal_is_too_many_results() {
    const KIND: u32 = 1092;
    let Some(strfry) = Strfry::start().await else {
        return;
    };
    let mut relay = strfry.connect().await;
    let frames: Vec<String> = (0..CAP as u64 + 5)
        .map(|i| event_frame(KIND, BASE + i, &format!("ev-{i}")))
        .collect();
    relay.publish(&frames).await.expect("seed strfry");

    let mut empty = negentropy::NegentropyStorageVector::new();
    empty.seal().expect("seal");
    let Err(err) = relay.reconcile(&wire_filter(KIND), empty).await else {
        panic!("an over-cap NEG-OPEN must be refused");
    };
    eprintln!("strfry refused: {err}");
    assert!(
        err.downcast_ref::<TooManyResults>().is_some(),
        "strfry's refusal must parse as TooManyResults, got: {err}"
    );
}

/// A windowed pull over the cap brings in every event, the ones on split
/// boundaries included, and a second run needs and fetches nothing. Before the
/// windows were inclusive, strfry served each boundary event in a window whose
/// local set never held it, so every run pulled it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real strfry: set STRFRY_BIN, or have podman"]
async fn strfry_windowed_pull_converges() {
    const KIND: u32 = 1093;
    const DENSE: u64 = 60;
    let Some(strfry) = Strfry::start().await else {
        return;
    };
    let mut relay = strfry.connect().await;

    // strfry holds one event per second; the client already holds every 4th.
    let all: Vec<String> = (0..DENSE)
        .map(|i| event_frame(KIND, BASE + i, &format!("ev-{i}")))
        .collect();
    relay.publish(&all).await.expect("seed strfry");
    let shared: Vec<String> = all.iter().step_by(4).cloned().collect();
    let (_dir, ndb) = temp_ndb();
    ingest(&ndb, KIND, &shared).await;
    let missing = all.len() - shared.len();

    let first = Recorder::default();
    pull_reconcile_windowed_with_progress(
        &mut relay,
        &ndb,
        &wire_filter(KIND),
        u32::MAX as u64,
        &first,
    )
    .await
    .expect("first pull");
    assert!(
        first.splits() > 0,
        "{DENSE} events over a cap of {CAP} bisect"
    );
    assert_eq!(first.needed(), missing, "needs exactly the missing events");
    assert_eq!(
        first.fetched(),
        missing,
        "fetches exactly the missing events"
    );
    assert_eq!(count_kind(&ndb, KIND), all.len(), "every event landed");

    let second = Recorder::default();
    pull_reconcile_windowed_with_progress(
        &mut relay,
        &ndb,
        &wire_filter(KIND),
        u32::MAX as u64,
        &second,
    )
    .await
    .expect("second pull");
    assert_eq!(second.needed(), 0, "converged: nothing needed");
    assert_eq!(second.fetched(), 0, "converged: nothing fetched");
}

/// A bidirectional reconcile over the cap pushes every client-only event, the
/// ones on split boundaries included, pulls every relay-only one, and a second
/// run pushes and fetches nothing. Mirrors the in-process
/// `over_cap_reconcile_converges_without_reflushing`, against strfry. Before the
/// windows were inclusive, a client-only event on a boundary was in no window's
/// local set, so the relay never reported it missing and it was never pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real strfry: set STRFRY_BIN, or have podman"]
async fn strfry_over_cap_reconcile_converges() {
    const KIND: u32 = 1094;
    const DENSE: u64 = 60;
    let Some(strfry) = Strfry::start().await else {
        return;
    };
    let mut relay = strfry.connect().await;

    // Every second holds one event: even seconds both sides have, odd seconds
    // only the client, and a tail only strfry.
    let shared: Vec<String> = (0..DENSE)
        .step_by(2)
        .map(|i| event_frame(KIND, BASE + i, &format!("shared-{i}")))
        .collect();
    let client_only: Vec<String> = (1..DENSE)
        .step_by(2)
        .map(|i| event_frame(KIND, BASE + i, &format!("client-{i}")))
        .collect();
    let relay_only: Vec<String> = (DENSE..DENSE + 10)
        .map(|i| event_frame(KIND, BASE + i, &format!("relay-{i}")))
        .collect();
    relay.publish(&shared).await.expect("seed shared");
    relay.publish(&relay_only).await.expect("seed relay-only");
    let (_dir, ndb) = temp_ndb();
    ingest(&ndb, KIND, &shared).await;
    ingest(&ndb, KIND, &client_only).await;

    let filter = Filter::new()
        .kinds([KIND as u64])
        .authors([author().bytes()])
        .build();
    let not_addressable = |_: u32| false;

    let first = Recorder::default();
    reconcile_sync_with_progress(
        &mut relay,
        &ndb,
        &author(),
        &[KIND],
        &filter,
        &not_addressable,
        &first,
    )
    .await
    .expect("first reconcile");
    assert!(first.splits() > 0, "an over-cap set bisects");
    assert_eq!(
        first.pushed(),
        client_only.len(),
        "pushes every client-only event"
    );
    assert_eq!(
        first.fetched(),
        relay_only.len(),
        "pulls every relay-only event"
    );

    let second = Recorder::default();
    reconcile_sync_with_progress(
        &mut relay,
        &ndb,
        &author(),
        &[KIND],
        &filter,
        &not_addressable,
        &second,
    )
    .await
    .expect("second reconcile");
    assert_eq!(second.pushed(), 0, "converged: nothing re-pushed");
    assert_eq!(second.fetched(), 0, "converged: nothing re-fetched");

    // And strfry holds the union: a fresh client pulls all of it.
    let (_fresh_dir, fresh) = temp_ndb();
    let mut fresh_relay = strfry.connect().await;
    pull_reconcile_windowed_with_progress(
        &mut fresh_relay,
        &fresh,
        &wire_filter(KIND),
        u32::MAX as u64,
        &(),
    )
    .await
    .expect("fresh pull");
    assert_eq!(
        count_kind(&fresh, KIND),
        shared.len() + client_only.len() + relay_only.len(),
        "strfry holds every event after the reconcile"
    );
}
