//! [NIP-42] relay authentication: the kind-22242 event both sides of an AUTH
//! handshake agree on.
//!
//! A relay that wants to know who it is talking to sends `["AUTH", <challenge>]`.
//! The client answers `["AUTH", <event>]`, where the event is a kind-22242 signed
//! by the account, carrying the relay's URL in a `relay` tag and the challenge in
//! a `challenge` tag. The relay replies with `OK`.
//!
//! Both halves live here so a client and a relay built on this crate cannot drift
//! apart on what a valid AUTH looks like:
//!
//! - [`build_auth_event`] — the client half: sign the answer to a challenge.
//! - [`verify_auth_event`] — the relay half: check an answer against the
//!   challenge this connection was sent and the URL the relay serves as.
//!
//! [NIP-42]: https://github.com/nostr-protocol/nips/blob/master/42.md

use nostr::JsonUtil;
use nostrdb::{Note, NoteBuilder};
use serde_json::json;

use crate::{FilledKeypair, Pubkey};

/// The NIP-42 client-authentication event kind.
pub const AUTH_KIND: u32 = 22242;

/// How far a 22242's `created_at` may sit from the relay's clock, either way,
/// before it is refused as stale. NIP-42 suggests "about 10 minutes".
pub const DEFAULT_AUTH_WINDOW_SECS: u64 = 600;

/// Sign the answer to a relay's `["AUTH", <challenge>]`: a kind-22242 with a
/// `relay` tag naming `relay_url` and a `challenge` tag echoing `challenge`,
/// stamped with the current time.
///
/// `relay_url` is the URL the account means to authenticate *to* — the name the
/// relay serves under (`wss://sns.damus.io`), not whatever address the socket
/// happened to resolve to. A relay compares it by host to its own service URL.
///
/// Returns `None` only if the note fails to build or sign.
pub fn build_auth_event(
    keypair: FilledKeypair<'_>,
    relay_url: &str,
    challenge: &str,
) -> Option<Note<'static>> {
    NoteBuilder::new()
        .kind(AUTH_KIND)
        .content("")
        .start_tag()
        .tag_str("relay")
        .tag_str(relay_url)
        .start_tag()
        .tag_str("challenge")
        .tag_str(challenge)
        .sign(&keypair.secret_key.secret_bytes())
        .build()
}

/// The client frame carrying a signed 22242: `["AUTH", <event>]`.
pub fn auth_message(note: &Note<'_>) -> Result<String, nostrdb::Error> {
    let note_json = note.json()?;
    Ok(format!(r#"["AUTH",{note_json}]"#))
}

/// What a relay expects of the 22242 a connection answers with.
#[derive(Debug, Clone, Copy)]
pub struct AuthCheck<'a> {
    /// The URL this relay serves as, e.g. `wss://sns.damus.io`. The event's
    /// `relay` tag must name the same host.
    pub service_url: &'a str,
    /// The challenge this connection was sent.
    pub challenge: &'a str,
    /// The relay's clock, in unix seconds.
    pub now: u64,
    /// How far `created_at` may sit from `now`, either way.
    pub window_secs: u64,
}

/// Why a 22242 was refused. The [`Display`](std::fmt::Display) text is meant to
/// follow an `invalid: ` prefix in the relay's `OK`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("auth event is not a valid nostr event")]
    Malformed,
    #[error("auth event id or signature does not verify")]
    BadSignature,
    #[error("auth event is kind {0}, not 22242")]
    WrongKind(u32),
    #[error("auth event does not answer this connection's challenge")]
    ChallengeMismatch,
    #[error("auth event's relay tag does not name this relay")]
    RelayMismatch,
    #[error("auth event created_at is outside the allowed window")]
    Stale,
}

/// Check a client's 22242 (as JSON) against what this connection expects, and
/// return the authenticated pubkey.
///
/// In order: the event parses, its id and signature verify, it is kind 22242,
/// its `challenge` tag equals [`AuthCheck::challenge`], its `relay` tag names the
/// same host as [`AuthCheck::service_url`], and its `created_at` is within
/// [`AuthCheck::window_secs`] of [`AuthCheck::now`].
///
/// The relay tag is compared by host alone — not scheme, port or path — because
/// one service name can be reached several ways (through a TLS proxy, from the
/// LAN by split-horizon DNS), and the socket address says nothing about which
/// name the client meant. A per-connection challenge is what stops a 22242
/// being replayed elsewhere; the host check stops one minted for another relay
/// being replayed here.
pub fn verify_auth_event(event_json: &str, check: &AuthCheck<'_>) -> Result<Pubkey, AuthError> {
    let event = nostr::Event::from_json(event_json).map_err(|_| AuthError::Malformed)?;
    event.verify().map_err(|_| AuthError::BadSignature)?;

    let kind = u32::from(event.kind.as_u16());
    if kind != AUTH_KIND {
        return Err(AuthError::WrongKind(kind));
    }

    if tag_value(&event, "challenge") != Some(check.challenge) {
        return Err(AuthError::ChallengeMismatch);
    }

    let relay_host = tag_value(&event, "relay").and_then(url_host);
    if relay_host.is_none() || relay_host != url_host(check.service_url) {
        return Err(AuthError::RelayMismatch);
    }

    if event.created_at.as_u64().abs_diff(check.now) > check.window_secs {
        return Err(AuthError::Stale);
    }

    Ok(Pubkey::new(event.pubkey.to_bytes()))
}

/// The relay's `["AUTH", <challenge>]` frame.
pub fn challenge_message(challenge: &str) -> String {
    json!(["AUTH", challenge]).to_string()
}

/// The first value of the first tag named `name`.
fn tag_value<'e>(event: &'e nostr::Event, name: &str) -> Option<&'e str> {
    event.tags.iter().find_map(|tag| match tag.as_slice() {
        [key, value, ..] if key == name => Some(value.as_str()),
        _ => None,
    })
}

/// A URL's host, lowercased. `None` if it does not parse or has no host.
fn url_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    parsed.host_str().map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FullKeypair;

    const SERVICE: &str = "wss://sns.damus.io";

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn check(challenge: &str) -> AuthCheck<'_> {
        AuthCheck {
            service_url: SERVICE,
            challenge,
            now: now(),
            window_secs: DEFAULT_AUTH_WINDOW_SECS,
        }
    }

    fn signed(kp: &FullKeypair, kind: u32, relay: &str, challenge: &str, at: u64) -> String {
        NoteBuilder::new()
            .kind(kind)
            .content("")
            .created_at(at)
            .start_tag()
            .tag_str("relay")
            .tag_str(relay)
            .start_tag()
            .tag_str("challenge")
            .tag_str(challenge)
            .sign(&kp.secret_key.secret_bytes())
            .build()
            .expect("note")
            .json()
            .expect("json")
    }

    #[test]
    fn built_event_verifies_as_its_signer() {
        let kp = FullKeypair::generate();
        let note = build_auth_event(kp.to_filled(), SERVICE, "abc").expect("build");
        assert_eq!(note.kind(), AUTH_KIND);
        let who = verify_auth_event(&note.json().unwrap(), &check("abc")).expect("verify");
        assert_eq!(who, kp.pubkey);
    }

    #[test]
    fn auth_message_wraps_the_event() {
        let kp = FullKeypair::generate();
        let note = build_auth_event(kp.to_filled(), SERVICE, "abc").expect("build");
        let frame: serde_json::Value = serde_json::from_str(&auth_message(&note).unwrap()).unwrap();
        assert_eq!(frame[0], "AUTH");
        assert_eq!(frame[1]["kind"], AUTH_KIND);
    }

    #[test]
    fn relay_tag_is_compared_by_host() {
        let kp = FullKeypair::generate();
        for relay in [
            "wss://sns.damus.io",
            "wss://sns.damus.io/",
            "wss://SNS.damus.io:443",
            "ws://sns.damus.io:7777/x",
        ] {
            let ev = signed(&kp, AUTH_KIND, relay, "c", now());
            assert!(verify_auth_event(&ev, &check("c")).is_ok(), "{relay}");
        }
        for relay in ["wss://damus.io", "wss://sns.damus.io.evil", "not a url", ""] {
            let ev = signed(&kp, AUTH_KIND, relay, "c", now());
            assert_eq!(
                verify_auth_event(&ev, &check("c")),
                Err(AuthError::RelayMismatch),
                "{relay}"
            );
        }
    }

    #[test]
    fn refuses_wrong_challenge_kind_and_time() {
        let kp = FullKeypair::generate();
        let ev = signed(&kp, AUTH_KIND, SERVICE, "other", now());
        assert_eq!(
            verify_auth_event(&ev, &check("c")),
            Err(AuthError::ChallengeMismatch)
        );

        let ev = signed(&kp, 1, SERVICE, "c", now());
        assert_eq!(
            verify_auth_event(&ev, &check("c")),
            Err(AuthError::WrongKind(1))
        );

        let ev = signed(&kp, AUTH_KIND, SERVICE, "c", now() - 3600);
        assert_eq!(verify_auth_event(&ev, &check("c")), Err(AuthError::Stale));
        let ev = signed(&kp, AUTH_KIND, SERVICE, "c", now() + 3600);
        assert_eq!(verify_auth_event(&ev, &check("c")), Err(AuthError::Stale));
    }

    #[test]
    fn refuses_tampered_and_malformed_events() {
        let kp = FullKeypair::generate();
        let ev = signed(&kp, AUTH_KIND, SERVICE, "c", now());
        let mut v: serde_json::Value = serde_json::from_str(&ev).unwrap();
        v["content"] = "tampered".into();
        assert_eq!(
            verify_auth_event(&v.to_string(), &check("c")),
            Err(AuthError::BadSignature)
        );
        assert_eq!(
            verify_auth_event("{}", &check("c")),
            Err(AuthError::Malformed)
        );
    }
}
