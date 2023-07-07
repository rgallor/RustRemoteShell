//! Device implementation.
//!
//! This module provides a [`Device`] struct necessary for the creation of a new device and
//! to connect through WebSocket connection with a host.
//! Thanks to the interaction with the [astarte](crate::astarte) module,
//! the device will first try to connect with the Astarte server.
//!
//! The module also provides the [`device_handle`] function, responsible for handling
//! the communication between the device and a host.

use std::path::PathBuf;

use std::fmt::Debug;

use astarte_device_sdk::AstarteDeviceSdk;
use futures::{future, SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::{JoinError, JoinSet};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};
use tokio_util::io::ReaderStream;
use tracing::{debug, error, info, instrument, warn};
use url::Url;

use crate::astarte::{Error as AstarteError, HandleAstarteConnection};
use crate::protocol::{DeviceMsg, HostMsg};
use crate::shell::{CommandHandler, ShellError};

#[cfg(feature = "tls")]
use crate::tls::{self, Error as TlsError};

/// Device errors.
#[derive(Error, Debug)]
pub enum DeviceError {
    ///Trasport error from Tungstenite.
    #[error("Trasport error from Tungstenite.")]
    Transport(#[from] tokio_tungstenite::tungstenite::Error),

    /// Error while trying to connect with the host.
    #[error("Error while trying to connect with the host.")]
    WebSocketConnect(#[source] tokio_tungstenite::tungstenite::Error),

    /// Wrong scheme received.
    #[error("Wrong scheme, {0}.")]
    WrongScheme(String),

    /// Error while reading stdout from child process executing the shell command.
    #[error("Error while reading from child process's stdout.")]
    ChildStdout(#[source] std::io::Error),

    /// Error while sending an URL across tasks.
    #[error("Error while sending an URL across tasks.")]
    Channel(#[from] SendError<Url>),

    /// Error while joining a tokio task.
    #[error("Error while joining a tokio task.")]
    Join(#[from] JoinError),

    /// [`AstarteError`] error.
    #[error("Astarte error, {}.", msg)]
    Astarte {
        /// Sourece error.
        #[source]
        err: AstarteError,
        /// Error message
        msg: String,
    },

    /// [`ShellError`] error.
    #[error("Shell error.")]
    Shell(#[from] ShellError),

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

/// Device struct.
///
/// The device will attempt to connect with Astarte. When an Astarte datastream aggregate Object
/// arrives, the device retrieves the information and build an URL, which will subsequently use for
/// the connection with a host, eventually adding TLS configuration.
#[derive(Clone, Debug)]
pub struct Device {
    astarte_device: AstarteDeviceSdk,
    ca_cert_file: Option<PathBuf>,
}

impl Device {
    /// Configure an Astarte device and wait for an Astarte event. Then retrieve an URL from the event.
    #[instrument(skip_all)]
    pub async fn new(device_cfg_path: &str) -> Result<Self, DeviceError> {
        let handle_astarte = HandleAstarteConnection;

        let cfg = handle_astarte
            .read_device_config(device_cfg_path)
            .await
            .map_err(|err| DeviceError::Astarte {
                err,
                msg: "wrong device config".to_string(),
            })?;

        let astarte_device = handle_astarte
            .create_astarte_device(&cfg)
            .await
            .map_err(|err| DeviceError::Astarte {
                err,
                msg: "failed to create the astarte device".to_string(),
            })?;

        info!("Connection to Astarte established.");

        Ok(Self {
            astarte_device,
            ca_cert_file: None,
        })
    }

    /// Provide CA cert file to the device in order to open a TLS connection with a host.
    #[cfg(feature = "tls")]
    pub fn with_tls(&mut self, ca_cert_file: Option<PathBuf>) {
        self.ca_cert_file = ca_cert_file
    }

    /// Start the device.
    ///
    /// A device that is started will try to connect with hosts after having received from Astarte the host URL.
    #[instrument(skip_all)]
    pub async fn start(self) -> Result<(), DeviceError> {
        debug!("spawning a task to handle new connections");
        let (tx_url, rx_url) = unbounded_channel::<Url>();

        // define a task to handle astarte events and send the received URL to the task responsible for the creation of new connections
        let handle_astarte =
            tokio::spawn(Device::handle_astarte_events(self.astarte_device, tx_url));

        // define a task to handle new connections after having received an URL from the task respponsible for handling astarte events
        Device::handle_connections(rx_url, self.ca_cert_file).await;

        handle_astarte.await.map_err(DeviceError::Join)?
    }

    /// Handle a single Astarte event.
    async fn handle_astarte_events(
        mut astarte_device: AstarteDeviceSdk,
        tx_url: UnboundedSender<Url>,
    ) -> Result<(), DeviceError> {
        loop {
            // Wait for an aggregate datastream containing a scheme, an IP and a port number to connect to
            match astarte_device.handle_events().await {
                Ok(data) => {
                    match data.data {
                        astarte_device_sdk::Aggregation::Object(map) if data.path == "/rshell" => {
                            let url =
                                HandleAstarteConnection::retrieve_url(&map).map_err(|err| {
                                    DeviceError::Astarte {
                                        err,
                                        msg: "failed to retrieve the url".to_string(),
                                    }
                                })?;
                            info!("received url: {}", url);

                            // send the retrieved URL to the task responsible for the creation of a new connection
                            tx_url.send(url).map_err(DeviceError::Channel)?;
                        }
                        _ => {
                            // when an error is returned, tx_url is dropped. This is fundamental because the task
                            // responsible for handling the connections will stop spawning other connections
                            // after having received None from the channel
                            return Err(DeviceError::Astarte {
                                err: AstarteError::AstarteWrongAggregation,
                                msg: "received wrong astarte type".to_string(),
                            });
                        }
                    }
                }
                // If the device get disconnected from Astarte, it will loop untill it is reconnected
                Err(err @ astarte_device_sdk::AstarteError::ConnectionError(_)) => {
                    warn!("Connection err  {:#?}", err);
                }
                Err(err) => {
                    error!("Astarte error: {:?}", err);
                    return Err(DeviceError::Astarte {
                        err: AstarteError::AstarteHandleEvent(err),
                        msg: "failed to handle astarte event".to_string(),
                    });
                }
            }
        }
    }

    /// Connect to a host by using a TLS connection.
    async fn handle_connections(mut rx_url: UnboundedReceiver<Url>, ca_cert_file: Option<PathBuf>) {
        // define a set of tasks, each responsible for handling a connection with a host
        let mut handles = JoinSet::new();

        // when the channel is dropped, that is when the astarte task fails, the rx_url.recv() statement
        // returns None and the while loop terminates
        debug!("waiting to receive an URL");
        while let Some(url) = rx_url.recv().await {
            debug!("received an URL, {}", url);
            handles.spawn(Device::handle_connection(url, ca_cert_file.clone()));
        }

        // await the other connections to terminate before returning
        // Note: errors are not propagated to avoid aborting the exisitn connections
        while let Some(res) = handles.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Err(err) => {
                    error!("Join handle error: {}", err);
                }
                Ok(Err(err)) => {
                    error!("device error, {:#?}", err);
                }
            }
        }
    }

    #[cfg(feature = "tls")]
    async fn handle_connection(url: Url, ca_cert_file: Option<PathBuf>) -> Result<(), DeviceError> {
        // in case of a TLS connection, it is possible to receive either "ws" or "wss" as acceptable url scheme
        let ws_stream = match url.scheme() {
            "wss" => {
                let connector = tls::device_tls_config(ca_cert_file)?; // Connector::Rustls
                tls::connect(&url, Some(connector)).await?
            }
            "ws" => {
                let (ws_stream, _) =
                    tokio_tungstenite::connect_async(&url)
                        .await
                        .map_err(|err| {
                            error!("Websocket error: {:?}", err);
                            DeviceError::WebSocketConnect(err)
                        })?;
                ws_stream
            }
            scheme => {
                return Err(DeviceError::WrongScheme(scheme.to_string()));
            }
        };

        device_handle(ws_stream).await
    }

    /// Connect to a host by using a plain TCP connection
    #[cfg(not(feature = "tls"))]
    #[instrument(skip_all)]
    async fn handle_connection(
        url: Url,
        _ca_cert_file: Option<PathBuf>,
    ) -> Result<(), DeviceError> {
        let ws_stream = match url.scheme() {
            "ws" => {
                let (ws_stream, _) =
                    tokio_tungstenite::connect_async(&url)
                        .await
                        .map_err(|err| {
                            error!("Websocket error: {:?}", err);
                            DeviceError::WebSocketConnect(err)
                        })?;
                ws_stream
            }
            scheme => {
                return Err(DeviceError::WrongScheme(scheme.to_string()));
            }
        };

        device_handle(ws_stream).await
    }
}

/// Function responsible for handling the communication between the device and a host.
///
/// The device listens for commands, executes them and sends the output to the host.
#[instrument(skip_all)]
pub async fn device_handle<U>(stream: WebSocketStream<U>) -> Result<(), DeviceError>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    // separate ownership between receiving and writing part
    let (mut write, read) = stream.split();

    // Read the received command
    let mut stream = read
        .map_err(DeviceError::Transport)
        .and_then(|msg| future::ready(HostMsg::try_from(msg)));

    while let Some(res) = stream.next().await {
        let cmd = res?;

        let cmd = match cmd {
            HostMsg::Command { cmd } => cmd,
            HostMsg::Exit => {
                info!("closing connection with host");
                write.close().await?;
                return Ok(());
            }
        };

        let res = CommandHandler::execute(cmd).map_err(DeviceError::Shell);

        let child_stdout = match res {
            Ok(child_stdout) => child_stdout,
            Err(DeviceError::Shell(err)) => {
                // send EoF
                debug!("Processing EOF.");
                write
                    .send(DeviceMsg::Err(err.to_string()).try_into()?)
                    .await?;

                continue;
            } // avoid crushing the application because of a non-crytical shell error
            _ => {
                todo!("TODO: check if there are crytical error that must interrupt the application")
            }
        };

        let mut msg_stream = ReaderStream::with_capacity(child_stdout, 1024)
            .map_err(DeviceError::ChildStdout)
            .and_then(|buf| {
                let msg = DeviceMsg::Output(buf.to_vec());

                match msg.try_into() {
                    Ok(msg) => future::ok(msg),
                    Err(err) => future::err(err),
                }
            });

        debug!("Processing msg stream.");
        while let Some(msg) = msg_stream.next().await {
            write.send(msg?).await?;
        }

        // send EoF
        debug!("Processing EOF.");
        write
            .send(Message::Binary(bson::to_vec(&DeviceMsg::Eof)?))
            .await?;
    }

    Ok(())
}
