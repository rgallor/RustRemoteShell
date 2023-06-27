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

/*
STEP
1. register the device with rust sdk
    a. import trait
    b. add device id
    c. configure it
    d. connect
2. add server-owned interface to astarte
3. send data with  "astartectl appengine --appengine-url http://localhost:4002/ --realm-management-url http://localhost:4000/ --realm-key test_private.pem --realm-name test devices send-data 2TBn-jNESuuHamE2Zo1anA <INTERFACE_NAME> <ENDPOINT> <VALUE>"
4. device must receive the data (IP + PORT) and use them
 */

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
}

#[derive(Clone, Debug)]
pub struct Device {
    url: Url,
}

impl Device {
    pub fn new(url: Url) -> Self {
        Self { url }
    }

    pub async fn connect<C>(&mut self, ca_cert: Option<C>) -> Result<(), DeviceError>
    where
        C: Into<Vec<u8>>,
    {
        let ws_stream = match ca_cert {
            #[cfg(feature = "tls")]
            Some(ca_cert) => {
                let connector = tls::client_tls_config(ca_cert.into()).await; // Connector::Rustls

                tls::connect(&self.url, Some(connector)).await?
            }
            _ => {
                let (ws_stream, _) = connect_async(&self.url).await.map_err(|err| {
                    error!("Websocket error: {:?}", err);
                    DeviceError::WebSocketConnect(err)
                })?;
                ws_stream
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
            Ok(Message::Binary(cmd_out.as_bytes().to_vec()))
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
