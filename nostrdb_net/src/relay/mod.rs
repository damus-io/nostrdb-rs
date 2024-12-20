#[cfg(not(feature = "tokio"))]
use ewebsock::{Options, WsMessage, WsReceiver, WsSender};

use crate::{ClientMessage, Result};
use nostrdb::Filter;
use std::fmt;
use std::hash::{Hash, Hasher};

#[cfg(feature = "tokio")]
use tokio::net::TcpStream;
#[cfg(feature = "tokio")]
use tokio_tungstenite::MaybeTlsStream;
#[cfg(feature = "tokio")]
use tungstenite::protocol::Message;

use tracing::{debug, error, info};

pub mod message;
pub mod pool;

#[derive(Debug, Copy, Clone)]
pub enum RelayStatus {
    Connected,
    Connecting,
    Disconnected,
}

#[cfg(feature = "tokio")]
pub type RelayStream = tokio_tungstenite::WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct Relay {
    pub url: String,
    pub status: RelayStatus,

    #[cfg(feature = "tokio")]
    pub stream: RelayStream,

    #[cfg(not(feature = "tokio"))]
    pub sender: WsSender,
    #[cfg(not(feature = "tokio"))]
    pub receiver: WsReceiver,
}

impl fmt::Debug for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Relay")
            .field("url", &self.url)
            .field("status", &self.status)
            .finish()
    }
}

impl Hash for Relay {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hashes the Relay by hashing the URL
        self.url.hash(state);
    }
}

impl PartialEq for Relay {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

impl Eq for Relay {}

impl Relay {
    #[cfg(feature = "tokio")]
    pub async fn new(url: String) -> Result<Self> {
        let status = RelayStatus::Connecting;
        let stream = Self::new_connection(&url).await?;

        Ok(Self {
            url,
            status,
            stream,
        })
    }

    #[cfg(not(feature = "tokio"))]
    pub fn new(url: String) -> Result<Self> {
        let status = RelayStatus::Connecting;
        let (sender, receiver) = ewebsock::connect_with_wakeup(&url, Options::default(), wakeup)?;

        Ok(Self {
            url,
            sender,
            receiver,
            status,
        })
    }

    pub fn send(&mut self, msg: &ClientMessage) {
        let json = match msg.to_json() {
            Ok(json) => {
                debug!("sending {} to {}", json, self.url);
                json
            }
            Err(e) => {
                error!("error serializing json for filter: {e}");
                return;
            }
        };

        #[cfg(feature = "tokio")]
        {
            let txt = Message::Text(json);
            self.stream.send(txt);
        }

        #[cfg(not(feature = "tokio"))]
        {
            let txt = WsMessage::Text(json);
            self.sender.send(txt);
        }
    }

    #[cfg(feature = "tokio")]
    pub async fn new_connection(url: &str) -> Result<RelayStream> {
        use tokio_tungstenite::connect_async;
        use tungstenite::client::IntoClientRequest;

        let request = url.into_client_request()?;
        let (stream, _response) = connect_async(request).await?;

        Ok(stream)
    }

    #[cfg(feature = "tokio")]
    pub async fn connect(&mut self) -> Result<()> {
        self.status = RelayStatus::Connecting;
        self.stream = Self::new_connection(&self.url).await?;

        Ok(())
    }

    #[cfg(not(feature = "tokio"))]
    pub fn connect(&mut self, wakeup: impl Fn() + Send + Sync + 'static) -> Result<()> {
        let (sender, receiver) =
            ewebsock::connect_with_wakeup(&self.url, Options::default(), wakeup)?;
        self.status = RelayStatus::Connecting;
        self.sender = sender;
        self.receiver = receiver;
        Ok(())
    }

    pub fn ping(&mut self) {
        #[cfg(not(feature = "tokio"))]
        {
            let msg = WsMessage::Ping(vec![]);
            self.sender.send(msg);
        }

        #[cfg(feature = "tokio")]
        {
            let msg = Message::Ping(vec![]);
            self.stream.send(msg);
        }
    }

    pub fn subscribe(&mut self, subid: String, filters: Vec<Filter>) {
        info!(
            "sending '{}' subscription to relay pool: {:?}",
            subid, filters
        );
        self.send(&ClientMessage::req(subid, filters));
    }
}
