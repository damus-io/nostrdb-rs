use crate::relay::{Relay, RelayStatus};
use crate::{ClientMessage, Result};
use nostrdb::Filter;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use url::Url;

#[cfg(not(feature = "tokio"))]
use ewebsock::WsMessage;

#[cfg(not(target_arch = "wasm32"))]
use tracing::{debug, error};

#[cfg(not(feature = "tokio"))]
pub type WebsockEvent = ewebsock::WsEvent;

#[cfg(feature = "tokio")]
pub type WebsockEvent = tungstenite::protocol::Message;

#[derive(Debug)]
pub struct PoolEvent<'a> {
    pub relay: &'a str,

    pub event: WebsockEvent,
}

impl PoolEvent<'_> {
    pub fn into_owned(self) -> PoolEventBuf {
        PoolEventBuf {
            relay: self.relay.to_owned(),
            event: self.event,
        }
    }
}

pub struct PoolEventBuf {
    pub relay: String,

    #[cfg(feature = "tokio")]
    pub event: tungstenite::protocol::Message,

    #[cfg(not(feature = "tokio"))]
    pub event: ewebsock::WsEvent,
}

pub struct PoolRelay {
    pub relay: Relay,
    pub last_ping: Instant,
    pub last_connect_attempt: Instant,
    pub retry_connect_after: Duration,
}

impl PoolRelay {
    pub fn new(relay: Relay) -> PoolRelay {
        PoolRelay {
            relay,
            last_ping: Instant::now(),
            last_connect_attempt: Instant::now(),
            retry_connect_after: Self::initial_reconnect_duration(),
        }
    }

    /// Determine if we should reconnect. You can call this if the relay
    /// is in a disconnected state and you want to know if you should try
    /// to reconnect
    pub fn should_reconnect(&self) -> bool {
        let now = Instant::now();
        let reconnect_at = self.last_connect_attempt + self.retry_connect_after;
        now > reconnect_at
    }

    #[cfg(not(feature = "tokio"))]
    fn reconnect(&mut self, wakeup: impl Fn() + Send + Sync + Clone + 'static) -> Result<()> {
        if !self.should_reconnect() {
            return Ok(());
        }

        self.mark_reconnect_attempt();
        self.relay.connect(wakeup)
    }

    #[cfg(feature = "tokio")]
    async fn reconnect(&mut self) -> Result<()> {
        if !self.should_reconnect() {
            return Ok(());
        }

        self.mark_reconnect_attempt();
        self.relay.connect().await
    }

    /// Mark the last reconnect attempt, to be called when you are about
    /// to reconnect.
    pub fn mark_reconnect_attempt(&mut self) {
        self.last_connect_attempt = Instant::now();
        let next_duration =
            Duration::from_millis(((self.retry_connect_after.as_millis() as f64) * 1.5) as u64);
        debug!(
            "bumping reconnect duration from {:?} to {:?} and retrying connect",
            self.retry_connect_after, next_duration
        );
        self.retry_connect_after = next_duration;
    }

    pub fn initial_reconnect_duration() -> Duration {
        Duration::from_secs(5)
    }
}

pub struct RelayPool {
    pub relays: Vec<PoolRelay>,
    pub ping_rate: Duration,
}

impl Default for RelayPool {
    fn default() -> Self {
        RelayPool::new()
    }
}

impl RelayPool {
    // Constructs a new, empty RelayPool.
    pub fn new() -> RelayPool {
        RelayPool {
            relays: vec![],
            ping_rate: Duration::from_secs(25),
        }
    }

    pub fn ping_rate(&mut self, duration: Duration) -> &mut Self {
        self.ping_rate = duration;
        self
    }

    pub fn has(&self, url: &str) -> bool {
        for relay in &self.relays {
            if relay.relay.url == url {
                return true;
            }
        }

        false
    }

    pub fn urls(&self) -> BTreeSet<String> {
        self.relays
            .iter()
            .map(|pool_relay| pool_relay.relay.url.clone())
            .collect()
    }

    pub fn send(&mut self, cmd: &ClientMessage) {
        for relay in &mut self.relays {
            relay.relay.send(cmd);
        }
    }

    pub fn unsubscribe(&mut self, subid: String) {
        for relay in &mut self.relays {
            relay.relay.send(&ClientMessage::close(subid.clone()));
        }
    }

    pub fn subscribe(&mut self, subid: String, filter: Vec<Filter>) {
        for relay in &mut self.relays {
            relay.relay.subscribe(subid.clone(), filter.clone());
        }
    }

    fn handle_connected(&self, relay: usize) {
        let now = std::time::Instant::now();
        let relay = self.relays[relay];

        relay.retry_connect_after = PoolRelay::initial_reconnect_duration();

        let should_ping = now - relay.last_ping > self.ping_rate;
        if should_ping {
            debug!("pinging {}", relay.relay.url);
            relay.relay.ping();
            relay.last_ping = Instant::now();
        }
    }

    #[cfg(feature = "tokio")]
    async fn keepalive_ping(&mut self) {
        let num_relays = self.relays.len();
        for i in 0..num_relays {
            let status = self.relays[i].relay.status;
            match status {
                RelayStatus::Disconnected => {
                    let relay = &mut self.relays[i];
                    relay.reconnect().await;
                }

                RelayStatus::Connected => {
                    self.handle_connected(i);
                }

                RelayStatus::Connecting => {}
            }
        }
    }

    /// Keep relay connectiongs alive by pinging relays that haven't been
    /// pinged in awhile. Adjust ping rate with [`ping_rate`].
    #[cfg(not(feature = "tokio"))]
    pub fn keepalive_ping(&mut self, wakeup: impl Fn() + Send + Sync + Clone + 'static) {
        for relay in &mut self.relays {
            match relay.relay.status {
                RelayStatus::Disconnected => {
                    relay.reconnect();
                }

                RelayStatus::Connected => {
                    self.handle_connected(relay);
                }

                RelayStatus::Connecting => {}
            }
        }
    }

    pub fn send_to(&mut self, cmd: &ClientMessage, relay_url: &str) {
        for relay in &mut self.relays {
            let relay = &mut relay.relay;
            if relay.url == relay_url {
                relay.send(cmd);
                return;
            }
        }
    }

    // Adds a websocket url to the RelayPool.
    pub fn add_url(
        &mut self,
        url: String,
        wakeup: impl Fn() + Send + Sync + Clone + 'static,
    ) -> Result<()> {
        let url = Self::canonicalize_url(url);
        // Check if the URL already exists in the pool.
        if self.has(&url) {
            return Ok(());
        }

        #[cfg(not(feature = "tokio"))]
        let relay = Relay::new(url, wakeup)?;

        let pool_relay = PoolRelay::new(relay);

        self.relays.push(pool_relay);

        Ok(())
    }

    pub fn add_urls(
        &mut self,
        urls: BTreeSet<String>,
        wakeup: impl Fn() + Send + Sync + Clone + 'static,
    ) -> Result<()> {
        for url in urls {
            self.add_url(url, wakeup.clone())?;
        }
        Ok(())
    }

    pub fn remove_urls(&mut self, urls: &BTreeSet<String>) {
        self.relays
            .retain(|pool_relay| !urls.contains(&pool_relay.relay.url));
    }

    // standardize the format (ie, trailing slashes)
    fn canonicalize_url(url: String) -> String {
        match Url::parse(&url) {
            Ok(parsed_url) => parsed_url.to_string(),
            Err(_) => url, // If parsing fails, return the original URL.
        }
    }

    #[cfg(feature = "tokio")]
    pub async fn recv(&mut self) -> Option<PoolEvent<'_>> {
        use futures_util::StreamExt;

        for relay in &mut self.relays {
            let relay = &mut relay.relay;
            let msg = relay.stream.next().await;
            // stream ended
            if msg.is_none() {
                relay.status = RelayStatus::Disconnected;
                return None;
            }

            let msg = msg.unwrap();

            let event = match msg {
                Err(err) => {
                    error!("recv err: {err}");
                    relay.status = RelayStatus::Disconnected;
                    return None;
                }

                Ok(msg) => {
                    match &msg {
                        WebsockEvent::Text(_bytes) => {}
                        WebsockEvent::Binary(_bytes) => {}
                        WebsockEvent::Ping(_bytes) => {}
                        WebsockEvent::Pong(_bytes) => {}
                        WebsockEvent::Frame(_frame) => {}
                        WebsockEvent::Close(_frame) => {
                            relay.status = RelayStatus::Disconnected;
                        }
                    }

                    msg
                }
            };

            return Some(PoolEvent {
                event,
                relay: &relay.url,
            });
        }

        None
    }

    /// Attempts to receive a pool event from a list of relays. The
    /// function searches each relay in the list in order, attempting to
    /// receive a message from each. If a message is received, return it.
    /// If no message is received from any relays, None is returned.
    #[cfg(not(feature = "tokio"))]
    pub fn try_recv(&mut self) -> Option<PoolEvent<'_>> {
        use ewebsock::WsEvent;

        for relay in &mut self.relays {
            let relay = &mut relay.relay;
            if let Some(event) = relay.receiver.try_recv() {
                match &event {
                    WsEvent::Opened => {
                        relay.status = RelayStatus::Connected;
                    }
                    WsEvent::Closed => {
                        relay.status = RelayStatus::Disconnected;
                    }
                    WsEvent::Error(err) => {
                        error!("{:?}", err);
                        relay.status = RelayStatus::Disconnected;
                    }
                    WsEvent::Message(ev) => {
                        // let's just handle pongs here.
                        // We only need to do this natively.
                        #[cfg(not(target_arch = "wasm32"))]
                        if let WsMessage::Ping(ref bs) = ev {
                            debug!("pong {}", &relay.url);
                            relay.sender.send(WsMessage::Pong(bs.to_owned()));
                        }
                    }
                }
                return Some(PoolEvent {
                    event,
                    relay: &relay.url,
                });
            }
        }

        None
    }
}
