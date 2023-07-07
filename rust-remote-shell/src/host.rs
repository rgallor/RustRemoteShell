//! Host implementation.
//!
//! This module provides a [`Host`] and [`HostBuilder`] structs necessary for the creation of a new host and
//! for the establishment of a new WebSocket connection with a device.
//! It also provides [`HostService`] struct, representing a [`tower`] service which handles the communication with a device.

use std::fmt::Debug;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;

use futures::{Future, SinkExt, StreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::signal;
use tokio::sync::mpsc::error::SendError;

use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::error::{Error as TungError, ProtocolError};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use tower::layer::util::Stack;
use tower::{layer::util::Identity, Service, ServiceBuilder};
use tracing::{debug, error, info, instrument, warn};

use crate::io_handler::IoHandler;
use crate::protocol::{DeviceMsg, HostMsg, ProtocolLayer};
use crate::stream::MessageHandler;
use crate::websocket::WebSocketLayer;

#[cfg(feature = "tls")]
use crate::tls::{self, Error as TlsError, TlsLayer};

/// Host error.
#[derive(Error, Debug)]
pub enum HostError {
    /// Error while trying to connect with a device.
    #[error("Error while trying to connect with a device.")]
    WebSocketConnect(#[from] TungError),

    /// Error while reading from stdin.
    #[error("IO error occurred while reading from stdin.")]
    IORead(#[source] std::io::Error),

    /// Error while writing to stdout,.
    #[error("IO error occurred while writing to stdout.")]
    IOWrite(#[source] std::io::Error),

    /// Error while binding.
    #[error("Failed to bind.")]
    Bind(#[source] std::io::Error),

    /// Failed to accept a new connection.
    #[error("Failed to accept a new connection.")]
    Listen(#[source] std::io::Error),

    /// Error while reading from a file.
    #[error("Error while reading from file {file}.")]
    ReadFile {
        /// IO error source.
        #[source]
        err: std::io::Error,
        /// File path.
        file: PathBuf,
    },

    /// Error while trying to send the output of a command to the main task.
    #[error("Error while trying to send the output of a command to the main task.")]
    ChannelMsg(#[from] SendError<Message>),

    /// Error from Tungstenite while reading command.
    #[error("Error from Tungstenite while reading command.")]
    TungsteniteReadData(#[source] TungError),

    /// Error from Tungstenite while closing websocket connection
    #[error("Error from Tungstenite while closing websocket connection")]
    TungsteniteClose(#[source] TungError),

    /// Wrong WebSocket message type.
    #[error("Wrong WebSocket message type.")]
    WrongWsMessage(tokio_tungstenite::tungstenite::Message),

    /// Couldn't deserialize from BSON.
    #[error("Couldn't deserialize from BSON.")]
    Deserialize(#[from] bson::de::Error),

    /// Couldn't serialize to BSON.
    #[error("Couldn't serialize to BSON.")]
    Serialize(#[from] bson::ser::Error),

    /// TLS error
    #[cfg(feature = "tls")]
    #[error("Wrong item.")]
    Tls(#[from] TlsError),
}

/// Host struct.
///
/// Struct handling new connections with devices.
pub struct Host {
    /// Tcp listener to a given address
    listener: TcpListener,
}

impl Host {
    /// Define a TCP listener and return a [`HostBuilder`].
    pub async fn bind(addr: SocketAddr) -> Result<HostBuilder<Identity>, HostError> {
        let listener = TcpListener::bind(addr).await.map_err(HostError::Bind)?;
        let host = Self { listener };

        Ok(HostBuilder {
            host,
            builder: ServiceBuilder::new(),
        })
    }

    /// Wait for a new connection from a device and handle it.
    ///
    /// service is an handler for the single connection.
    #[instrument(skip_all)]
    pub async fn listen<S>(&mut self, mut service: S) -> Result<(), HostError>
    where
        S: Service<TcpStream, Error = HostError> + Clone + 'static,
    {
        loop {
            let (stream, _) = self.listener.accept().await.map_err(HostError::Listen)?;
            info!("Connection accepted.");
            // handle the connection by sending messages and printing out responses from the device
            match service.call(stream).await {
                Ok(_) => {
                    info!("connection closed");
                }
                Err(err) => {
                    Self::handle_error(err)?;
                }
            }
        }
    }

    #[instrument]
    fn handle_error(err: HostError) -> Result<(), HostError> {
        match err {
            #[cfg(feature = "tls")]
            HostError::Tls(tls_err @ TlsError::AcceptTls(_)) => {
                error!("Connection discarded due to tls error: {}", tls_err);
                Ok(())
            }
            err => Err(err),
        }
    }
}

/// Builder for a Host.
///
/// Builder responsible for creating a stack of services on top of which the host will
/// be listening for new (device) connections.The [`HostBuilder`] struct interacts with [`tower`] crate
pub struct HostBuilder<L> {
    host: Host,
    /// Service builder.
    ///
    /// It may contain an Identity layer or a stack of layers
    builder: ServiceBuilder<L>,
}

impl<L> HostBuilder<L> {
    /// Returns the [`HostBuilder`] private fields.
    ///
    /// This function is only called if the TLS feature is enabled.
    #[cfg(feature = "tls")]
    pub fn fields(self) -> (Host, ServiceBuilder<L>) {
        (self.host, self.builder)
    }
}

impl HostBuilder<Identity> {
    /// Return a new builder containing a TLS layer.
    #[cfg(feature = "tls")]
    pub async fn with_tls(
        self,
        host_cert_file: PathBuf,
        privkey_file: PathBuf,
    ) -> Result<HostBuilder<Stack<TlsLayer, Identity>>, HostError> {
        let acceptor = tls::acceptor(host_cert_file, privkey_file).await?;

        Ok(HostBuilder {
            host: self.host,
            builder: self.builder.layer(TlsLayer::new(acceptor)),
        })
    }

    /// Call the host listen function after having added the necessary layers
    pub async fn serve(mut self) -> Result<(), HostError> {
        let service = self
            .builder
            .layer(WebSocketLayer)
            .layer(ProtocolLayer)
            .service(HostService);

        self.host.listen(service).await
    }
}

/// Service responsible for handling a connection.
#[derive(Clone, Debug)]
pub struct HostService;

impl<U> Service<HandleConnection<U>> for HostService
where
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Response = ();
    type Error = HostError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: HandleConnection<U>) -> Self::Future {
        Box::pin(req.handle_connection())
    }
}

/// Handle connection struct
///
/// Implement the necessary methods to handle a single connection with a device.
/// The host sends commands, wait for device's answer and display it.
pub struct HandleConnection<U> {
    ws_stream: MessageHandler<U>,
    ctrl_c: JoinHandle<()>,
}

impl<U> Drop for HandleConnection<U> {
    fn drop(&mut self) {
        self.ctrl_c.abort();
    }
}

impl<U> HandleConnection<U>
where
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Create a new handler for a connection.
    pub(crate) fn new(ws_stream: WebSocketStream<U>) -> Self {
        let ws_stream = MessageHandler::new(ws_stream);

        // spawn the task to handle the reception of CTRL C (SIGINT) signal
        let ctrl_c = HandleConnection::ctrl_c_handler(ws_stream.clone());

        Self { ws_stream, ctrl_c }
    }

    fn ctrl_c_handler(ws_stream: MessageHandler<U>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if let Err(err) = signal::ctrl_c().await {
                    error!("CTRL C error: {}", err);
                }

                let msg = match HostMsg::Ctrlc.try_into() {
                    Err(err) => {
                        error!("Error converting HostMsg::Ctrlc into Message, {:?}", err);
                        return;
                    }
                    Ok(msg) => msg,
                };

                // send CTRL C to the device
                // Take the sink part of the websocket connection and send CTRL C message.
                if let Err(err) = ws_stream.sink().await.send(msg).await {
                    error!("Error sending CTRL C Message to device: {}", err);
                }

                debug!("CTRL C sent");
            }
        })
    }

    /// Connection handler.
    #[instrument(skip_all)]
    async fn handle_connection(self) -> Result<(), HostError> {
        match self.impl_handle_connection().await {
            Ok(()) => {
                info!("Closing websocket connection");
                Ok(())
            }
            Err(HostError::WebSocketConnect(TungError::Protocol(
                ProtocolError::ResetWithoutClosingHandshake,
            ))) => {
                info!("Closing websocket connection due to device interruption");
                Ok(())
            }
            Err(HostError::WebSocketConnect(TungError::Io(err)))
                if err.kind() == ErrorKind::UnexpectedEof =>
            {
                info!("Connection reset by peer");
                Ok(())
            }
            Err(HostError::TungsteniteReadData(_)) => {
                info!("Connection closed normally");
                Ok(())
            }
            Err(err) => {
                error!("Fatal error: {}", err);
                eprintln!("{:#?}", err);
                Err(err)
            }
        }
    }

    /// Implementation of a connection handler.
    #[instrument(skip_all)]
    async fn impl_handle_connection(mut self) -> Result<(), HostError> {
        let mut io_handler = IoHandler::default();

        // if exit, read_stdin() returns Ok(None)
        while let Some(cmd) = io_handler.read_stdin().await? {
            let msg = HostMsg::Command {
                cmd: cmd.to_string(),
            };

            self.ws_stream
                .sink()
                .await
                .send(msg.try_into()?)
                .await
                .map_err(HostError::TungsteniteReadData)?;

            // handle stream of messages coming from device
            while let Some(msg) = self.ws_stream.next().await {
                let msg = msg?;

                match msg {
                    DeviceMsg::Eof => {
                        debug!("EOF received.");
                        break;
                    }
                    DeviceMsg::Err(err) => {
                        warn!("{}", err);
                        break;
                    }
                    DeviceMsg::Output(out) => {
                        debug!("Received {} bytes", out.len());
                        io_handler.write_stdout(out).await?
                    }
                }
            }
        }

        info!("Exit command or EOF received.");

        self.ws_stream.close().await?;

        Ok(())
    }
}
