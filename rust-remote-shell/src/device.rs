use std::{fmt::Debug, io::ErrorKind, string::FromUtf8Error};

use futures::{SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{error::ProtocolError, Message},
    WebSocketStream,
};
use tracing::{error, info, warn};
use url::Url;

use crate::astarte::{Error as AstarteError, HandleAstarteConnection};
use crate::shell::CommandHandler;
#[cfg(feature = "tls")]
use crate::tls;

#[derive(Error, Debug)]
pub enum DeviceError {
    #[error("Error while reading the shell command from websocket")]
    ReadCommand,
    #[error("Error marshaling to UTF8")]
    Utf8Error(#[from] FromUtf8Error),
    #[error("Trasport error from Tungstenite")]
    Transport(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("Error while trying to connect with server")]
    WebSocketConnect(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("Close websocket connection")]
    CloseWebsocket,
    #[error("Wrong scheme, {0}")]
    WrongScheme(String),
    #[error("Astarte error, {}", msg)]
    Astarte {
        #[source]
        err: AstarteError,
        msg: String,
    },
}

#[derive(Clone, Debug)]
pub struct Device {
    url: Url,
}

impl Device {
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

        // wait for an aggregate datastream containing IP and port to connect to
        // TODO: loop over handle_events. If a 2nd event arrives while the device is still handling the 1st, queue it so that it can be managed later or do something else
        match device.handle_events().await {
            Ok(data) => {
                if let astarte_device_sdk::Aggregation::Object(map) = data.data {
                    let url =
                        handle_astarte
                            .retrieve_url(map)
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

    #[cfg(feature = "tls")]
    pub async fn connect_tls<C>(&mut self, ca_cert: C) -> Result<(), DeviceError>
    where
        C: Into<Vec<u8>>,
    {
        let ws_stream = match self.url.scheme() {
            "wss" => {
                let connector = tls::client_tls_config(ca_cert.into()).await; // Connector::Rustls
                tls::connect(&self.url, Some(connector)).await?
            }
            scheme => {
                return Err(DeviceError::WrongScheme(scheme.to_string()));
            }
        };

        device_handle(ws_stream).await
    }

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

// the device listen for commands, execute them and send the output to the host
pub async fn device_handle<U>(stream: WebSocketStream<U>) -> Result<(), DeviceError>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    // separate ownership between receiving and writing part
    let (write, read) = stream.split();

    // Read the received command
    let res = read
        .map_err(DeviceError::Transport)
        .and_then(|msg| async move {
            info!("Received command from the client");
            match msg {
                // convert the message from a Vec<u8> into a OsString
                Message::Binary(v) => String::from_utf8(v).map_err(DeviceError::Utf8Error),
                Message::Close(_) => Err(DeviceError::CloseWebsocket), // the client closed the connection
                _ => Err(DeviceError::ReadCommand),
            }
        })
        .and_then(|cmd| async move {
            // define a command handler
            let cmd_handler = CommandHandler::default();

            // execute the command and eventually return the error
            let cmd_out = cmd_handler.execute(cmd).await.unwrap_or_else(|err| {
                warn!("Shell error: {}", err);
                format!("Shell error: {}\n", err)
            });

            info!("Send command output to the client");
            Ok(Message::Binary(cmd_out.as_bytes().to_vec())) // TODO: BUFFERIZE
        })
        .forward(write.sink_map_err(DeviceError::Transport))
        .await;

    match res {
        Ok(()) => Ok(()),
        Err(DeviceError::CloseWebsocket)
        | Err(DeviceError::Transport(tokio_tungstenite::tungstenite::Error::Protocol(
            ProtocolError::ResetWithoutClosingHandshake,
        ))) => {
            warn!("Websocket connection closed");
            Ok(())
        }
        Err(DeviceError::Transport(tokio_tungstenite::tungstenite::Error::Io(err)))
            if err.kind() == ErrorKind::UnexpectedEof =>
        {
            warn!("Websocket connection closed");
            Ok(())
        }
        Err(err) => {
            error!("Fatal error occurred: {}", err);
            Err(err)
        }
    }
}
