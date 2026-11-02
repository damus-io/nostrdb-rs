//! The opt-in access policy that locks the embedded relay down for the open
//! internet: [NIP-42] AUTH on connect, a members allowlist on the authenticated
//! pubkey, a kinds allowlist, and limits on what an unauthenticated or abusive
//! connection can make the relay do. See [`AccessPolicy`].
//!
//! [NIP-42]: https://github.com/nostr-protocol/nips/blob/master/42.md

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr::secp256k1::rand::{RngCore, rngs::OsRng};

use crate::Pubkey;
use crate::relay::auth::{self, AuthCheck};

/// Who may use the relay, for what, and how much. Pass one to
/// [`spawn_with_policy`](super::spawn_with_policy); without one the relay
/// behaves as the open localhost dogfooding port it always was.
///
/// With a policy:
///
/// - every connection is sent `["AUTH", <challenge>]` on connect, and is hung up
///   if no member has authenticated on it within [`auth_timeout`](Self::auth_timeout);
/// - until then every `EVENT`, `REQ`, `COUNT` and `NEG-OPEN` is refused with an
///   `auth-required:` reason, and a frame over
///   [`pre_auth_max_message_len`](Self::pre_auth_max_message_len) hangs up;
/// - a connection authenticated as a pubkey outside [`members`](Self::members)
///   is refused the same way with `restricted:`. Membership is decided on the
///   *authenticated* pubkey, never an event's author: a gift wrap is signed by
///   a throwaway key, an SNS envelope by a team key, a PNS note by a PNS key;
/// - an `EVENT` whose kind is outside [`kinds`](Self::kinds) is refused with
///   `blocked:`, and nothing outside it is ever served;
/// - an `EVENT`'s id and signature are verified, and a duplicate recognised,
///   before `OK` is sent, and `OK true` waits until the event is stored.
///
/// The remaining fields bound what one client can cost the relay.
#[derive(Debug, Clone)]
pub struct AccessPolicy {
    /// The URL clients authenticate to, e.g. `wss://sns.damus.io`. A 22242's
    /// `relay` tag must name its host.
    pub service_url: String,
    /// The pubkeys allowed to read and write once authenticated.
    pub members: HashSet<Pubkey>,
    /// The event kinds the relay stores and serves.
    pub kinds: HashSet<u32>,
    /// How long a connection may stay open before a member authenticates on it.
    /// Also bounds the websocket handshake.
    pub auth_timeout: Duration,
    /// How far a 22242's `created_at` may sit from the relay's clock.
    pub auth_window_secs: u64,
    /// The largest frame a connection may send before a member authenticates on
    /// it. A 22242 is well under 1 KiB.
    pub pre_auth_max_message_len: usize,
    /// The largest websocket message (and frame) any connection may send.
    pub max_message_len: usize,
    /// Open connections allowed from one client address.
    pub max_connections_per_ip: usize,
    /// Open connections allowed in all.
    pub max_connections: usize,
    /// A request header naming the client's address, e.g. `CF-Connecting-IP`,
    /// honoured when present; otherwise the socket's peer address is used. Only
    /// set this when the relay is reachable solely through a proxy that sets
    /// the header itself, or clients can claim any address they like.
    pub client_ip_header: Option<String>,
    /// Frames queued for one connection before it is hung up as too slow.
    pub outbound_queue: usize,
    /// Stored events a single `REQ` replays before `EOSE`.
    pub stored_query_limit: i32,
    /// Events a single negentropy reconciliation may cover. A `NEG-OPEN` over it
    /// is refused with `blocked: too many query results`, as strfry does.
    pub max_sync_events: usize,
    /// Negentropy frame size limit, in bytes before hex encoding.
    pub neg_frame_limit: u64,
    /// Live `REQ` subscriptions plus open negentropy sessions per connection.
    pub max_subscriptions: usize,
    /// Filters allowed in one `REQ`.
    pub max_filters: usize,
    /// How long an `EVENT` may take to be stored before `OK false`.
    pub store_timeout: Duration,
}

impl AccessPolicy {
    /// A policy serving `kinds` to `members`, authenticating to `service_url`,
    /// with every limit at its default.
    pub fn new(
        service_url: impl Into<String>,
        members: impl IntoIterator<Item = Pubkey>,
        kinds: impl IntoIterator<Item = u32>,
    ) -> Self {
        Self {
            service_url: service_url.into(),
            members: members.into_iter().collect(),
            kinds: kinds.into_iter().collect(),
            auth_timeout: Duration::from_secs(10),
            auth_window_secs: auth::DEFAULT_AUTH_WINDOW_SECS,
            pre_auth_max_message_len: 8 * 1024,
            max_message_len: 512 * 1024,
            max_connections_per_ip: 16,
            max_connections: 512,
            client_ip_header: None,
            outbound_queue: 2_000,
            stored_query_limit: 500,
            max_sync_events: 50_000,
            neg_frame_limit: 60_000,
            max_subscriptions: 32,
            max_filters: 16,
            store_timeout: Duration::from_secs(5),
        }
    }

    pub(super) fn kind_allowed(&self, kind: u32) -> bool {
        self.kinds.contains(&kind)
    }
}

/// One connection's NIP-42 state: the challenge it was sent and who, if anyone,
/// has authenticated on it.
pub(super) struct ConnAuth {
    challenge: String,
    authed: Option<Pubkey>,
    member: bool,
}

impl ConnAuth {
    pub(super) fn new() -> Self {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        Self {
            challenge: hex::encode(bytes),
            authed: None,
            member: false,
        }
    }

    pub(super) fn challenge(&self) -> &str {
        &self.challenge
    }

    pub(super) fn is_member(&self) -> bool {
        self.member
    }

    /// `Ok` if this connection may read and write; otherwise the reason, with
    /// its machine-readable prefix, to put in the `OK`, `CLOSED` or `NEG-ERR`.
    pub(super) fn gate(&self) -> Result<(), String> {
        match self.authed {
            _ if self.member => Ok(()),
            None => Err("auth-required: this relay serves members only; AUTH first".to_owned()),
            Some(pubkey) => Err(format!(
                "restricted: {} is not a member of this relay",
                pubkey.hex()
            )),
        }
    }

    /// Fold in a client's `["AUTH", <event>]`. `Ok` means `OK true`; `Err` is the
    /// `OK false` reason. A connection authenticates once: a second AUTH, even a
    /// valid one, is refused and changes nothing.
    pub(super) fn authenticate(
        &mut self,
        policy: &AccessPolicy,
        event_json: &str,
        now: u64,
    ) -> Result<(), String> {
        if self.authed.is_some() {
            return Err("invalid: this connection is already authenticated".to_owned());
        }
        let check = AuthCheck {
            service_url: &policy.service_url,
            challenge: &self.challenge,
            now,
            window_secs: policy.auth_window_secs,
        };
        let pubkey =
            auth::verify_auth_event(event_json, &check).map_err(|e| format!("invalid: {e}"))?;
        self.member = policy.members.contains(&pubkey);
        self.authed = Some(pubkey);
        Ok(())
    }
}

/// Open connections, per client address and in all. Shared by every
/// connection task; a [`ConnSlot`] gives its count back when dropped.
#[derive(Default)]
pub(super) struct ConnTracker {
    by_ip: Mutex<HashMap<IpAddr, usize>>,
}

impl ConnTracker {
    /// Take a slot for `ip`, or `None` when it or the relay is at its cap.
    pub(super) fn admit(self: &Arc<Self>, ip: IpAddr, policy: &AccessPolicy) -> Option<ConnSlot> {
        let mut by_ip = self.by_ip.lock().unwrap_or_else(|e| e.into_inner());
        let total: usize = by_ip.values().sum();
        let here = by_ip.get(&ip).copied().unwrap_or(0);
        if total >= policy.max_connections || here >= policy.max_connections_per_ip {
            return None;
        }
        by_ip.insert(ip, here + 1);
        Some(ConnSlot {
            tracker: self.clone(),
            ip,
        })
    }
}

/// One admitted connection's place in the [`ConnTracker`].
pub(super) struct ConnSlot {
    tracker: Arc<ConnTracker>,
    ip: IpAddr,
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut by_ip = self.tracker.by_ip.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = by_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                by_ip.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_caps_per_ip_and_gives_slots_back() {
        let mut policy = AccessPolicy::new("wss://x", [], []);
        policy.max_connections_per_ip = 2;
        policy.max_connections = 3;
        let tracker = Arc::new(ConnTracker::default());
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();

        let a1 = tracker.admit(a, &policy).expect("a1");
        let _a2 = tracker.admit(a, &policy).expect("a2");
        assert!(tracker.admit(a, &policy).is_none(), "per-ip cap");
        let _b1 = tracker.admit(b, &policy).expect("b1");
        assert!(tracker.admit(b, &policy).is_none(), "total cap");

        drop(a1);
        assert!(tracker.admit(a, &policy).is_some(), "slot given back");
    }
}
