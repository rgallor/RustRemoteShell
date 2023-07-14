//! Host implementation.
//!
//! This module provides a [`Host`] struct necessary for the creation of a new host and
//! for the establishment of a new WebSocket connection with a device.
//! It also provides [`HostService`] and [`HostLayer`] structs, necessary to define the [`tower`] service
//!  which handles the communication with a device.

use std::fmt::Debug;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use futures::Future;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;

use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{channel, Sender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::{select, signal};
use tokio_tungstenite::tungstenite::error::{Error as TungError, ProtocolError};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use tower::layer::util::{Identity, Stack};
use tower::make::MakeConnection;
use tower::make::MakeService;
use tower::Layer;
use tower::{Service, ServiceBuilder};
use tracing::{debug, error, info, instrument, warn};

use crate::command::CommandService;
use crate::stream::MessageHandler;
use crate::websocket::{TcpService, WebSocketLayer, WebSocketService};

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
    IORead(#[source] io::Error),

    /// Error while writing to stdout,.
    #[error("IO error occurred while writing to stdout.")]
    IOWrite(#[source] io::Error),

    /// Error while binding.
    #[error("Failed to bind.")]
    Bind(#[source] io::Error),

    /// Failed to accept a new connection.
    #[error("Failed to accept a new connection.")]
    Listen(#[source] io::Error),

    /// Error while reading from a file.
    #[error("Error while reading from file {file}.")]
    ReadFile {
        /// IO error source.
        #[source]
        err: io::Error,
        /// File path.
        file: PathBuf,
    },

    /// Error while trying to send the output of a command to the task responsible for handling it.
    #[error("Error while trying to send the output of a command to the task responsible for handling it.")]
    ChannelMsg(#[from] SendError<Message>),

    /// Error while trying to send the command to the task responsible for handling it.
    #[error("Error while trying to send the command to the task responsible for handling it.")]
    ChannelCmd(#[from] SendError<String>),

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

    /// Unexpected EoF.
    #[error("Unexpected EoF.")]
    UnexpectedEof,

    /// Exit command received.
    ///
    /// This error variant doesn't represent a real error.
    /// Instead it is used to close a connection without dropping stdin.
    #[error("Exit command")]
    Exit,

    /// TLS error
    #[cfg(feature = "tls")]
    #[error("Wrong item.")]
    Tls(#[from] TlsError),
}

impl HostError {
    /// Method used to handle errors causing the closure of the underlying WebSocket connection
    /// between the host and and Astarte device. It returns a [`Exit`](HostError::Exit) error in case
    /// the connection should be gracefully closed, otherwise it returns the original error.
    pub(crate) fn handle_close(self) -> Result<String, Self> {
        match self {
            HostError::WebSocketConnect(TungError::Protocol(
                ProtocolError::ResetWithoutClosingHandshake,
            )) => {
                info!("Closing websocket connection due to device interruption");
                Err(HostError::Exit)
            }
            HostError::WebSocketConnect(TungError::Io(err))
                if err.kind() == io::ErrorKind::UnexpectedEof =>
            {
                info!("Connection reset by peer");
                Err(HostError::Exit)
            }
            HostError::TungsteniteReadData(_) => {
                info!("Connection closed normally");
                Err(HostError::Exit)
            }
            #[cfg(feature = "tls")]
            HostError::Tls(tls_err @ TlsError::AcceptTls(_)) => {
                error!("Connection discarded due to tls error: {}", tls_err);
                Ok(format!(
                    "Connection discarded due to tls error: {}",
                    tls_err
                ))
            }
            err => Err(err),
        }
    }
}

/// Struct handling connections with devices.
pub struct Host {
    /// Tcp listener to a given address
    listener: TcpListener,
    #[cfg(feature = "tls")]
    acceptor: Option<tokio_rustls::TlsAcceptor>,
}

impl Host {
    /// Define a TCP listener given a [`SocketAddr`].
    pub async fn bind(addr: SocketAddr) -> Result<Self, HostError> {
        let listener = TcpListener::bind(addr).await.map_err(HostError::Bind)?;

        Ok(Host {
            listener,
            #[cfg(feature = "tls")]
            acceptor: None,
        })
    }

    /// Add TLS information.
    #[cfg(feature = "tls")]
    pub async fn with_tls(
        mut self,
        host_cert_file: PathBuf,
        privkey_file: PathBuf,
    ) -> Result<Self, HostError> {
        let acceptor = tls::acceptor(host_cert_file, privkey_file).await?;
        self.acceptor = Some(acceptor);
        Ok(self)
    }

    /// This function define a [`tower`] service (built using different tower Layers)
    /// and start listenning in loop for new connections from devices.
    #[instrument(skip_all)]
    pub async fn listen(mut self) -> Result<(), HostError> {
        // define a Notify used to notify tasks when a CTRL C is sent
        let ctrlc_notify = Arc::new(Notify::new());

        // spawn the task rsponsible for handling the reception of CTRL C
        let ctrl_c_handle = Self::ctrl_c_handler(Arc::clone(&ctrlc_notify));

        // build the a HostService
        let builder = ServiceBuilder::new()
            .layer(HostLayer {
                ctrl_c: Arc::clone(&ctrlc_notify),
            })
            .layer(WebSocketLayer);

        // return the result of the listen loop in case of a TCP or TLS connection
        let res = self.listen_result(builder, ctrlc_notify).await;

        // after the listen loop is closed, meaning that an error occurred, abort the active tasks
        ctrl_c_handle.abort();
        let _ = ctrl_c_handle.await; // discarding join error

        res
    }

    #[cfg(feature = "tls")]
    async fn listen_result(
        &mut self,
        builder: ServiceBuilder<Stack<WebSocketLayer, Stack<HostLayer, Identity>>>,
        ctrlc_notify: Arc<Notify>,
    ) -> Result<(), HostError> {
        match &mut self.acceptor {
            Some(acceptor) => {
                let service = builder
                    .layer(TlsLayer::new(acceptor.clone()))
                    .service(TcpService);
                self.listen_loop(service, Arc::clone(&ctrlc_notify)).await
            }
            None => {
                unreachable!("acceptor must be initialized when using TLS")
            }
        }
    }

    #[cfg(not(feature = "tls"))]
    async fn listen_result(
        &mut self,
        builder: ServiceBuilder<Stack<WebSocketLayer, Stack<HostLayer, Identity>>>,
        ctrlc_notify: Arc<Notify>,
    ) -> Result<(), HostError> {
        let service = builder.service(TcpService);
        self.listen_loop(service, Arc::clone(&ctrlc_notify)).await
    }

    /// In case a CTRL C signal arrives, it notify the event to the tasks capable of handling the notification.
    #[instrument(skip_all)]
    fn ctrl_c_handler(ctrlc_tx: Arc<Notify>) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if let Err(err) = signal::ctrl_c().await {
                    error!("CTRL C error: {}", err);
                }

                ctrlc_tx.notify_one();

                debug!("CTRL C sent");
            }
        })
    }

    /// Function responsible for handling the connection between a host and a device.
    /// It first wait for a new connection to be established and then uses the [CommandService](crate::command::CommandService)
    /// to send shell command to the device and handle its responses.
    #[instrument(skip_all)]
    async fn listen_loop<S, U>(
        &mut self,
        mut service: HostService<WebSocketService<S>>,
        ctrlc_notify: Arc<Notify>,
    ) -> Result<(), HostError>
    where
        S: for<'a> MakeConnection<&'a mut TcpListener, Connection = U, Error = HostError>
            + Clone
            + 'static,
        U: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        let (tx_stdin, mut rx_stdin) = channel(1);

        // task responsible for handling stdin
        let handle_stdio = tokio::task::spawn_blocking(move || Self::stdin_handler(tx_stdin));

        loop {
            select! {
                _ = ctrlc_notify.notified() => {
                    info!("CTRL C received while waiting for a connection, terminating the host.");
                    handle_stdio.abort();
                    // this is necessary to stop reading from stdin if the task is blocking
                    std::process::exit(0);
                }
                // create a new connection and use cmd_handler to handle shell commands
                cmd_handler = service.make_service(&mut self.listener) => {
                    let mut cmd_handler = cmd_handler?;

                    loop {
                        select! {
                            // CTRL C received
                            _ = ctrlc_notify.notified() => {
                                info!("CTRL C received while waiting for a command to be executed, closing the connection and waiting for a new one.");
                                Self::ctrl_c_close(cmd_handler, &handle_stdio).await?;
                                break;
                            }
                            // receive a string (command) from the stdin task
                            res = rx_stdin.recv() => {
                                if let Some(cmd) = res {
                                    // CTRL D (0 bytes read) -> close connection
                                    if cmd.is_empty() {
                                        info!("CTRL D received");
                                        Self::ctrl_c_close(cmd_handler, &handle_stdio).await?;
                                        break;
                                    }

                                    // handle command using call() function of CommandService
                                    match cmd_handler.call(cmd).await {
                                        Ok(()) => {},
                                        Err(HostError::Exit) => {
                                            info!("waiting to receive a new connection");
                                            break;
                                        },

                                        Err(err) => return Err(err),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Endless stdin read.
    fn stdin_handler(tx_stdin: Sender<String>) -> Result<(), HostError> {
        let stdin = std::io::stdin();

        loop {
            let mut buf = String::new();
            stdin.read_line(&mut buf).map_err(HostError::IORead)?;
            tx_stdin.blocking_send(buf)?; // HostError::ChannelCmd
        }
    }

    /// Close the connection and abort the task responsible for handling stdin read.
    async fn ctrl_c_close<U>(
        cmd_handler: CommandService<U>,
        handle_stdio: &JoinHandle<Result<(), HostError>>,
    ) -> Result<(), HostError>
    where
        U: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        cmd_handler.close().await?;
        handle_stdio.abort();
        Ok(())
    }
}

/// Service responsible for handling a connection.
///
/// It creates a [`WebSocketStream`] and uses it to define a `MessageHandler`, a stream
/// used to handle possible messages the Host can send to a device, necessary to create a
/// [`CommandService`].
#[derive(Clone, Debug)]
pub struct HostService<S> {
    service: S, // WebSocketService
    ctrl_c: Arc<Notify>,
}

impl<'a, S, U> Service<&'a mut TcpListener> for HostService<S>
where
    S: Service<&'a mut TcpListener, Response = WebSocketStream<U>> + Clone + 'static,
    U: AsyncRead + AsyncWrite + Unpin,
    HostError: From<S::Error>,
{
    type Response = CommandService<U>;
    type Error = HostError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + 'a>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(HostError::from)
    }

    fn call(&mut self, req: &'a mut TcpListener) -> Self::Future {
        let mut service = self.service.clone();
        let ctrlc_notify = Arc::clone(&self.ctrl_c);

        Box::pin(async move {
            let ws_stream = service.call(req).await?;
            let msg_handler = MessageHandler::new(ws_stream);

            Ok(CommandService::new(msg_handler, ctrlc_notify))
        })
    }
}

/// Host layer to be added in a [tower stack](tower::layer::util::Stack).
pub struct HostLayer {
    ctrl_c: Arc<Notify>,
}

impl<S> Layer<S> for HostLayer {
    type Service = HostService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HostService {
            service: inner,
            ctrl_c: Arc::clone(&self.ctrl_c),
        }
    }
}
