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
use std::{fmt::Debug, string::FromUtf8Error};

use futures::{future, SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream};
use tokio_util::io::ReaderStream;
use tracing::{debug, error, info, instrument};
use url::Url;

use crate::astarte::{Error as AstarteError, HandleAstarteConnection};
use crate::protocol::{DeviceMsg, HostMsg};
use crate::shell::{CommandHandler, ShellError};
#[cfg(feature = "tls")]
use crate::tls::{self, Error as TlsError};

/// Device errors.
#[derive(Error, Debug)]
pub enum DeviceError {
    /// Error while reading the shell command from websocket.
    #[error("Error while reading the shell command from websocket.")]
    ReadCommand,

    /// Error marshaling to UTF8.
    #[error("Error marshaling to UTF8.")]
    Utf8Error(#[from] FromUtf8Error),

    ///Trasport error from Tungstenite.
    #[error("Trasport error from Tungstenite.")]
    Transport(#[from] tokio_tungstenite::tungstenite::Error),

    /// Error while trying to connect with the host.
    #[error("Error while trying to connect with the host.")]
    WebSocketConnect(#[source] tokio_tungstenite::tungstenite::Error),

    /// Close websocket connection.
    ///
    /// This is not an error. Instead, when a device wants to close a connection
    /// it sends a CloseWebsocket error.
    #[error("Close websocket connection.")]
    CloseWebsocket,

    /// Wrong scheme received.
    #[error("Wrong scheme, {0}.")]
    WrongScheme(String),

    /// Error while reading from file.
    #[error("Error while reading from file.")]
    ReadFile(#[source] std::io::Error),

    /// Error while reading stdout from child process executing the shell command.
    #[error("Error while reading from child process's stdout.")]
    ChildStdout(#[source] std::io::Error),

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
    url: Url,
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

        let mut device = handle_astarte
            .create_astarte_device(&cfg)
            .await
            .map_err(|err| DeviceError::Astarte {
                err,
                msg: "failed to create the astarte device".to_string(),
            })?;

        info!("Connection to Astarte established.");

        // TODO: creare funzione handle_events e chiamarla in loop nel main

        // Wait for an aggregate datastream containing IP and port to connect to
        // TODO: define 1 task to loop over handle_events. spawn a new task for each host connection. Define a channel to handle errors (handled by the main task)
        match device.handle_events().await {
            Ok(data) => {
                if let astarte_device_sdk::Aggregation::Object(map) = data.data {
                    let url =
                        handle_astarte
                            .retrieve_url(&map)
                            .map_err(|err| DeviceError::Astarte {
                                err,
                                msg: "failed to retrieve the url".to_string(),
                            })?;
                    info!("Connecting to {}", url);
                    Ok(Self { url })
                } else {
                    Err(DeviceError::Astarte {
                        err: AstarteError::AstarteWrongAggregation,
                        msg: "received wrong astarte type".to_string(),
                    })
                }
            }
            Err(err) => {
                error!("Astarte error: {:?}", err);
                Err(DeviceError::Astarte {
                    err: AstarteError::AstarteHandleEvent(err),
                    msg: "failed to handle astarte event".to_string(),
                })
            }
        }
    }

    /// Connect to a host by using a TLS connection.
    #[cfg(feature = "tls")]
    pub async fn connect_tls(&mut self, ca_cert_file: Option<PathBuf>) -> Result<(), DeviceError> {
        let ws_stream = match self.url.scheme() {
            "wss" => {
                let connector = tls::device_tls_config(ca_cert_file)?; // Connector::Rustls
                tls::connect(&self.url, Some(connector)).await?
            }
            scheme => {
                return Err(DeviceError::WrongScheme(scheme.to_string()));
            }
        };

        device_handle(ws_stream).await
    }

    /// Connect to a host by using a plain TCP connection
    #[instrument(skip_all)]
    pub async fn connect(&mut self) -> Result<(), DeviceError> {
        let ws_stream = match self.url.scheme() {
            "ws" => {
                let (ws_stream, _) = connect_async(&self.url).await.map_err(|err| {
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
    let mut stream = read.map_err(DeviceError::Transport).and_then(|msg| {
        debug!("Received command from the host, {:?}", msg);
        future::ready(HostMsg::try_from(msg))
    });

    while let Some(res) = stream.next().await {
        let cmd = res?;

        let cmd = match cmd {
            HostMsg::Command { cmd } => cmd,
            HostMsg::Exit => {
                write.close().await?;
                return Ok(());
            } // device exit -> // TODO: esci dal task relativo alla connessione device-host
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
            debug!(?msg);
            write.send(msg?).await?;
        }

        // send EoF
        debug!("Processing EOF.");
        write
            .send(Message::Binary(bson::to_vec(&DeviceMsg::Eof)?))
            .await?;
    }

    Ok(())

    // match res {
    //     Ok(()) => Ok(()),
    //     Err(
    //         DeviceError::CloseWebsocket
    //         | DeviceError::Transport(tokio_tungstenite::tungstenite::Error::Protocol(
    //             ProtocolError::ResetWithoutClosingHandshake,
    //         )),
    //     ) => {
    //         warn!("Websocket connection closed");
    //         Ok(())
    //     }
    //     Err(DeviceError::Transport(tokio_tungstenite::tungstenite::Error::Io(err)))
    //         if err.kind() == ErrorKind::UnexpectedEof =>
    //     {
    //         warn!("Websocket connection closed");
    //         Ok(())
    //     }
    //     Err(err) => {
    //         error!("Fatal error occurred: {}", err);
    //         Err(err)
    //     }
    // }
}
