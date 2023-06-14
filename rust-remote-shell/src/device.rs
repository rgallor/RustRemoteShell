use std::{fmt::Debug, future, string::FromUtf8Error};

use futures::{SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tower::Service;
use tracing::{info, warn};
use url::Url;

use crate::shell::{CommandHandler, ShellError};
#[cfg(feature = "tls")]
use crate::tls;

#[derive(Error, Debug)]
pub enum DeviceError {
    #[error("Failed to bind")]
    Bind(#[from] io::Error),
    #[error("Failed to accept a new connection")]
    AcceptConnection {
        #[source]
        err: io::Error,
    },
    #[error("Failed to read {file} from file")]
    ReadFile {
        #[source]
        err: io::Error,
        file: String,
    },
    #[error("Connected streams should have a peer address")]
    PeerAddr,
    #[error("Error during the websocket handshake occurred")]
    WebSocketHandshake,
    #[error("Error while reading the shell command from websocket")]
    ReadCommand,
    #[error("Error marshaling to UTF8")]
    Utf8Error(#[from] FromUtf8Error),
    #[error("Trasport error from Tungstenite")]
    Transport(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("Error while precessing the shell command")]
    ShellError(#[from] ShellError),
    #[error("Close websocket connection")]
    CloseWebsocket,
    #[error("Error while establishing a TLS connection")]
    #[cfg(feature = "tls")]
    RustTls(#[from] tokio_rustls::rustls::Error),
}

#[derive(Clone, Debug)]
pub struct Device {
    url: Url,
}

impl Device {
    pub fn new(url: Url) -> Self {
        Self { url }
    }

    pub async fn connect<S, E, C>(&mut self, ca_cert: Option<C>, mut service: S)
    where
        S: Service<WebSocketStream<MaybeTlsStream<TcpStream>>, Error = E> + Clone + 'static,
        E: Debug,
        C: Into<Vec<u8>>,
    {
        let ws_stream = match ca_cert {
            #[cfg(feature = "tls")]
            Some(ca_cert) => {
                let connector = tls::client_tls_config(ca_cert.into()).await; // Connector::Rustls

                tls::connect(&self.url, Some(connector)).await.expect("err")
            }
            _ => connect_async(&self.url).await.expect("err").0,
        };

        service.call(ws_stream).await.expect("err");
    }
}

// the device listen for commands, execute them and send the output to the host
pub async fn device_handle<U>(stream: WebSocketStream<U>) -> Result<(), DeviceError>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    // let (write, read) = stream.split();

    // let _ = read
    //     .map_err(ServerError::Transport)
    //     .and_then(|msg| {
    //         let cmd = match msg {
    //             // convert the message from a Vec<u8> into a OsString
    //             Message::Binary(v) => String::from_utf8(v).map_err(ServerError::Utf8Error),
    //             Message::Close(_) => Err(ServerError::CloseWebsocket), // the client closed the connection
    //             _ => Err(ServerError::ReadCommand),
    //         };

    //         future::ready(cmd)
    //     })
    //     .and_then(|cmd| async move {
    //         // define a command handler
    //         let cmd_handler = CommandHandler::new();

    //         // execute the command and eventually return the error
    //         let cmd_out = cmd_handler
    //             .execute(cmd)
    //             .await
    //             .unwrap_or_else(|err| format!("Shell error: {}\n", err));

    //         Ok(Message::Binary(cmd_out.as_bytes().to_vec()))
    //     })
    //     .forward(write.sink_map_err(ServerError::Transport))
    //     .await;

    // separate ownership between receiving and writing part
    let (write, read) = stream.split();

    // Read the received command
    read.map_err(DeviceError::Transport)
        .and_then(|msg| {
            let cmd = match msg {
                // convert the message from a Vec<u8> into a OsString
                Message::Binary(v) => String::from_utf8(v).map_err(DeviceError::Utf8Error),
                Message::Close(_) => Err(DeviceError::CloseWebsocket), // the client closed the connection
                _ => Err(DeviceError::ReadCommand),
            };
            info!("Received command from the client");
            future::ready(cmd)
        })
        .and_then(|cmd| async move {
            // define a command handler
            let cmd_handler = CommandHandler::new();

            // execute the command and eventually return the error
            let cmd_out = cmd_handler.execute(cmd).await.unwrap_or_else(|err| {
                warn!("Shell error: {}", err);
                format!("Shell error: {}\n", err)
            });

            info!("Send command output to the client");
            Ok(Message::Binary(cmd_out.as_bytes().to_vec()))
        })
        .forward(write.sink_map_err(DeviceError::Transport))
        .await?;

    Ok(())
}
