//! The embedded relay's opt-in AccessPolicy, over real websockets: the hardening
//! that makes it safe to expose (AUTH deadline, pre-AUTH size cap, per-client
//! connection cap, verify-before-OK, per-connection limits). The members/kinds
//! lockdown itself is covered end to end by the ndb-sns-relay binary's suite.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostrdb::{Config, Ndb, NoteBuilder};
use nostrdb_net::FullKeypair;
use nostrdb_net::relay::auth;
use nostrdb_net::relay::server::{AccessPolicy, RelayHandle, spawn_with_policy};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SERVICE: &str = "wss://sns.example";
const KIND: u32 = 1081;

fn relay(
    member: &FullKeypair,
    tune: impl FnOnce(&mut AccessPolicy),
) -> (RelayHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ndb = Ndb::new(
        dir.path().to_str().unwrap(),
        &Config::new().set_ingester_threads(1),
    )
    .expect("ndb");
    let mut policy = AccessPolicy::new(SERVICE, [member.pubkey], [KIND]);
    tune(&mut policy);
    let handle =
        spawn_with_policy(ndb.clone(), "127.0.0.1:0".parse().unwrap(), policy).expect("spawn");
    (handle, dir)
}

async fn connect_as(url: &str, client_ip: Option<&str>) -> Result<Ws, String> {
    let mut request = url.into_client_request().unwrap();
    if let Some(ip) = client_ip {
        request
            .headers_mut()
            .insert("CF-Connecting-IP", ip.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

async fn next_frame(ws: &mut Ws) -> Option<Vec<Value>> {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("frame within timeout")?;
        match msg {
            Ok(Message::Text(text)) => return Some(serde_json::from_str(&text).expect("json")),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

async fn send(ws: &mut Ws, frame: Value) {
    ws.send(Message::Text(frame.to_string()))
        .await
        .expect("send");
}

/// Connect, read the challenge, and AUTH as `kp`. Returns the socket.
async fn authed(url: &str, kp: &FullKeypair) -> Ws {
    let mut ws = connect_as(url, None).await.expect("connect");
    let frame = next_frame(&mut ws).await.expect("challenge");
    assert_eq!(frame[0], "AUTH");
    let challenge = frame[1].as_str().unwrap();
    let note = auth::build_auth_event(kp.to_filled(), SERVICE, challenge).unwrap();
    ws.send(Message::Text(auth::auth_message(&note).unwrap()))
        .await
        .unwrap();
    let ok = next_frame(&mut ws).await.expect("OK");
    assert_eq!(ok[2], true, "auth refused: {ok:?}");
    ws
}

fn event(kp: &FullKeypair, content: &str) -> Value {
    let note = NoteBuilder::new()
        .kind(KIND)
        .content(content)
        .sign(&kp.secret_key.secret_bytes())
        .build()
        .unwrap();
    serde_json::from_str(&note.json().unwrap()).unwrap()
}

#[tokio::test]
async fn unauthenticated_connection_is_dropped_at_the_deadline() {
    let member = FullKeypair::generate();
    let (relay, _dir) = relay(&member, |p| p.auth_timeout = Duration::from_millis(300));

    let mut ws = connect_as(&relay.url(), None).await.unwrap();
    assert_eq!(next_frame(&mut ws).await.unwrap()[0], "AUTH");
    let notice = next_frame(&mut ws).await.expect("notice before hangup");
    assert_eq!(notice[0], "NOTICE");
    assert!(notice[1].as_str().unwrap().starts_with("auth-required:"));
    assert!(
        next_frame(&mut ws).await.is_none(),
        "connection should close"
    );

    // A member who authenticated in time is not on the clock any more.
    let mut ws = authed(&relay.url(), &member).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    send(&mut ws, json!(["REQ", "s", {"kinds": [KIND]}])).await;
    assert_eq!(
        next_frame(&mut ws).await.unwrap(),
        vec![json!("EOSE"), json!("s")]
    );
}

#[tokio::test]
async fn oversized_frame_before_auth_hangs_up() {
    let member = FullKeypair::generate();
    let (relay, _dir) = relay(&member, |p| p.pre_auth_max_message_len = 1024);

    let mut ws = connect_as(&relay.url(), None).await.unwrap();
    next_frame(&mut ws).await.unwrap();
    send(&mut ws, json!(["REQ", "s", {"#t": ["x".repeat(2048)]}])).await;
    let notice = next_frame(&mut ws).await.expect("notice");
    assert!(notice[1].as_str().unwrap().starts_with("auth-required:"));
    assert!(
        next_frame(&mut ws).await.is_none(),
        "connection should close"
    );

    // The same frame is fine once a member is authenticated.
    let mut ws = authed(&relay.url(), &member).await;
    send(
        &mut ws,
        json!(["REQ", "s", {"kinds": [KIND], "#t": ["x".repeat(2048)]}]),
    )
    .await;
    assert_eq!(next_frame(&mut ws).await.unwrap()[0], "EOSE");
}

#[tokio::test]
async fn connections_are_capped_per_client_ip_header() {
    let member = FullKeypair::generate();
    let (relay, _dir) = relay(&member, |p| {
        p.max_connections_per_ip = 1;
        p.client_ip_header = Some("CF-Connecting-IP".to_owned());
    });
    let url = relay.url();

    let first = connect_as(&url, Some("203.0.113.7")).await.expect("first");
    let refused = connect_as(&url, Some("203.0.113.7")).await;
    assert!(
        refused.as_ref().is_err_and(|e| e.contains("429")),
        "second from the same client should be refused: {:?}",
        refused.map(|_| ())
    );
    // A different client (as named by the header) is not affected...
    let _other = connect_as(&url, Some("198.51.100.9"))
        .await
        .expect("other ip");
    // ...and the slot comes back when the first hangs up.
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;
    connect_as(&url, Some("203.0.113.7"))
        .await
        .expect("slot returned");
}

#[tokio::test]
async fn events_are_verified_and_stored_before_ok() {
    let member = FullKeypair::generate();
    let (relay, _dir) = relay(&member, |_| {});
    let mut ws = authed(&relay.url(), &member).await;

    // A tampered event is refused and never stored.
    let mut forged = event(&member, "original");
    forged["content"] = "tampered".into();
    send(&mut ws, json!(["EVENT", forged])).await;
    let ok = next_frame(&mut ws).await.unwrap();
    assert_eq!(ok[2], false);
    assert!(ok[3].as_str().unwrap().starts_with("invalid:"), "{ok:?}");

    // A good one is OK'd only once it is stored: a REQ straight after finds it.
    let good = event(&member, "hello");
    send(&mut ws, json!(["EVENT", good])).await;
    let ok = next_frame(&mut ws).await.unwrap();
    assert_eq!(
        (ok[1].clone(), ok[2].clone()),
        (good["id"].clone(), json!(true))
    );
    send(&mut ws, json!(["REQ", "s", {"kinds": [KIND]}])).await;
    let stored = next_frame(&mut ws).await.unwrap();
    assert_eq!(stored[0], "EVENT");
    assert_eq!(stored[2]["id"], good["id"]);
    assert_eq!(next_frame(&mut ws).await.unwrap()[0], "EOSE");

    // Sending it again is a duplicate, OK'd as such.
    send(&mut ws, json!(["EVENT", good])).await;
    let ok = next_frame(&mut ws).await.unwrap();
    assert_eq!(ok[2], true);
    assert!(ok[3].as_str().unwrap().starts_with("duplicate:"), "{ok:?}");
}

#[tokio::test]
async fn subscriptions_and_filters_are_capped() {
    let member = FullKeypair::generate();
    let (relay, _dir) = relay(&member, |p| {
        p.max_subscriptions = 2;
        p.max_filters = 2;
    });
    let mut ws = authed(&relay.url(), &member).await;

    let f = json!({"kinds": [KIND]});
    send(&mut ws, json!(["REQ", "many", f, f, f])).await;
    let closed = next_frame(&mut ws).await.unwrap();
    assert_eq!(closed[0], "CLOSED");
    assert!(closed[2].as_str().unwrap().starts_with("blocked:"));

    for sub in ["a", "b"] {
        send(&mut ws, json!(["REQ", sub, f])).await;
        assert_eq!(next_frame(&mut ws).await.unwrap()[0], "EOSE");
    }
    send(&mut ws, json!(["REQ", "c", f])).await;
    let closed = next_frame(&mut ws).await.unwrap();
    assert_eq!(
        (closed[0].clone(), closed[1].clone()),
        (json!("CLOSED"), json!("c"))
    );
    assert!(closed[2].as_str().unwrap().starts_with("blocked:"));

    // Re-REQ of an open id replaces it rather than counting against the cap.
    send(&mut ws, json!(["REQ", "a", f])).await;
    assert_eq!(next_frame(&mut ws).await.unwrap()[0], "EOSE");
}
