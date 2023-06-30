//! Protocol used to describe the Host and Device actions after having established a connection.

use futures::{future, stream::BoxStream, StreamExt, TryStreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tower::{Layer, Service};

use crate::host::{HandleConnection, HostError};

/// Enum used to describe the actions a device can make.
pub enum HostMsg {
    /// Close the WebSocket connection
    Exit,
    /// Execute a command
    Command {
        /// Command to execute
        cmd: String,
    },
}

/// Enum used to describe the actions a host can make.
pub enum DeviceMsg {
    /// The output of a command is ready
    Output(Vec<u8>),
    /// EOF received
    Eof,
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

        let stream = read
            .and_then(|_msg| future::ok(DeviceMsg::Eof)) // TODO: handle different message cases
            .map_err(HostError::from)
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
