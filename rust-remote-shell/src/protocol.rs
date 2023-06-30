//! Protocol used to describe the Host and Device actions after having established a connection.

use futures::{stream::BoxStream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};
use tower::{Layer, Service};
use tracing::info;

use crate::{
    device::DeviceError,
    host::{HandleConnection, HostError},
};

/// Enum used to describe the actions a device can make.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMsg {
    /// Close the WebSocket connection
    Exit,
    /// Execute a command
    Command {
        /// Command to execute
        cmd: String,
    },
}

impl TryFrom<Message> for HostMsg {
    type Error = DeviceError;

    fn try_from(value: Message) -> Result<Self, Self::Error> {
        match value {
            Message::Close(_) => Ok(HostMsg::Exit),
            Message::Binary(cmd) => Ok(HostMsg::Command {
                cmd: String::from_utf8_lossy(&cmd).to_string(),
            }),
            msg => Err(DeviceError::WrongWsMessage(msg)),
        }
    }
}

/// Enum used to describe the actions a host can make.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum DeviceMsg {
    /// The output of a command is ready
    Output(Vec<u8>),
    /// EOF received
    Eof,
    /// Error
    Err(String),
}

impl TryFrom<DeviceMsg> for Message {
    type Error = DeviceError;

    fn try_from(value: DeviceMsg) -> Result<Self, Self::Error> {
        Ok(Message::Binary(bson::to_vec(&value)?))
    }
}

/// Service introducing a protocol in the connection.
#[derive(Clone)]
pub struct ProtocolService<S> {
    service: S,
}

/// Stream of [`DeviceMsg`] protocol messsages.
pub type DeviceMsgStream = BoxStream<'static, Result<DeviceMsg, HostError>>;

// receive a websocket stream
impl<S, U> Service<WebSocketStream<U>> for ProtocolService<S>
where
    S: Service<HandleConnection<U>, Error = HostError> + Clone + 'static,
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx) // ready when the wrapping service is ready
    }

    fn call(&mut self, req: WebSocketStream<U>) -> Self::Future {
        let (write, read) = req.split();

        // define a reader stream that maps a tungstenite Message into a DeviceMsg protocol message.
        let stream = read
            .map_err(HostError::from)
            .and_then(handle_message) // handle different message cases
            .boxed();

        let handler = HandleConnection::new(stream, write);

        self.service.call(handler)
    }
}

/// Protocol layer to be added in a [tower stack](tower::layer::util::Stack).
pub struct ProtocolLayer;

impl<S> Layer<S> for ProtocolLayer {
    type Service = ProtocolService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ProtocolService { service: inner }
    }
}

async fn handle_message(msg: Message) -> Result<DeviceMsg, HostError> {
    match msg {
        Message::Close(_) => {
            info!("Closing WebSocket connection.");
            Ok(DeviceMsg::Eof)
        }
        Message::Binary(v) => {
            let msg: DeviceMsg = bson::from_slice(&v)?;
            Ok(msg)
        }
        msg => Err(HostError::WrongWsMessage(msg)),
    }
}
