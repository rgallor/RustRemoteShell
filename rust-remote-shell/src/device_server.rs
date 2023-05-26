use std::io::{self};
use std::net::SocketAddr;
use std::string::FromUtf8Error;
use std::sync::Arc;

use futures::{future, SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, instrument, warn};

use crate::shell::{CommandHandler, ShellError};

#[derive(Error, Debug)]
pub enum DeviceServerError {
    #[error("Failed to bind")]
    Bind(#[from] io::Error),
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
}

type TxErrorType = tokio::sync::mpsc::Sender<DeviceServerError>;
const MAX_ERRORS_TO_HANDLE: usize = 10;

#[derive(Debug)]
pub struct DeviceServer {
    addr: SocketAddr,
}

impl DeviceServer {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }

    #[instrument(skip(self))]
    pub async fn listen(&self) -> Result<(), DeviceServerError> {
        let socket = TcpListener::bind(self.addr)
            .await
            .map_err(DeviceServerError::Bind)?;

        info!("Listening at {}", self.addr);

        // channel tx/rx to handle error
        let (tx_err, mut rx_err) =
            tokio::sync::mpsc::channel::<DeviceServerError>(MAX_ERRORS_TO_HANDLE);

        let handles = Arc::new(Mutex::new(Vec::new()));
        let handles_clone = Arc::clone(&handles);

        // accept a new connection
        let handle_connections = tokio::spawn(async move {
            while let Ok((stream, _)) = socket.accept().await {
                let handle_single_connection =
                    tokio::spawn(Self::handle_connection(stream, tx_err.clone()));

                handles_clone.lock().await.push(handle_single_connection);
            }
        });

        // join connections and handle errors
        if let Some(err) = rx_err.recv().await {
            self.terminate(handle_connections, &handles).await?;
            error!("Received error {:?}. Terminate all connections.", err);
            return Err(err);
        }

        Ok(())
    }

    // terminate all connections
    #[instrument(skip_all)]
    async fn terminate(
        &self,
        handle_connections: JoinHandle<()>,
        handles: &Mutex<Vec<JoinHandle<()>>>,
    ) -> Result<(), DeviceServerError> {
        handle_connections.abort();

        match handle_connections.await {
            Err(err) if !err.is_cancelled() => error!("Join failed: {}", err),
            _ => {}
        }

        for h in handles.lock().await.iter() {
            h.abort();
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn handle_connection(stream: TcpStream, tx_err: TxErrorType) {
        match Self::impl_handle_connection(stream).await {
            Ok(_) => {}
            Err(DeviceServerError::CloseWebsocket)
            | Err(DeviceServerError::Transport(TungsteniteError::Protocol(
                ProtocolError::ResetWithoutClosingHandshake,
            ))) => {
                warn!("Websocket connection closed");
                // TODO: check that the connection is effectively closed on the server-side (not only on the client-side)
            }
            Err(err) => {
                error!("Fatal error occurred: {}", err);
                tx_err.send(err).await.expect("Error handler failure");
            }
        }
    }

    #[instrument(skip_all)]
    async fn impl_handle_connection(stream: TcpStream) -> Result<(), DeviceServerError> {
        let addr = stream
            .peer_addr()
            .map_err(|_| DeviceServerError::PeerAddr)?;

        // create a WebSocket connection
        let web_socket_stream = accept_async(stream)
            .await
            .map_err(|_| DeviceServerError::WebSocketHandshake)?;

        info!("New WebSocket connection created: {}", addr);

        // separate ownership between receiving and writing part
        let (write, read) = web_socket_stream.split();

        // Read the received command
        read.map_err(DeviceServerError::Transport)
            .and_then(|msg| {
                let cmd = match msg {
                    // convert the message from a Vec<u8> into a OsString
                    Message::Binary(v) => {
                        String::from_utf8(v).map_err(DeviceServerError::Utf8Error)
                    }
                    Message::Close(_) => Err(DeviceServerError::CloseWebsocket), // the client closed the connection
                    _ => Err(DeviceServerError::ReadCommand),
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
            .forward(write.sink_map_err(DeviceServerError::Transport))
            .await?;

        Ok(())
    }
}
