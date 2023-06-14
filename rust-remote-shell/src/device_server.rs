use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::string::FromUtf8Error;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tracing::{error, info, instrument, warn};

use crate::shell::{CommandHandler, ShellError};

// Avoid importing tls module if TLS is not enabled
#[cfg(feature = "tls")]
use crate::tls::*;

type TxErrorType = tokio::sync::mpsc::Sender<ServerError>;
const MAX_ERRORS_TO_HANDLE: usize = 10;

#[derive(Error, Debug)]
pub enum ServerError {
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

#[async_trait]
pub trait Connect<S, L> {
    async fn bind(&mut self, addr: SocketAddr) -> Result<L, ServerError>;
    async fn connect(&mut self, listener: &mut L) -> Result<S, ServerError>;
}

pub struct TcpConnector;

#[async_trait]
impl Connect<TcpStream, TcpListener> for TcpConnector {
    async fn bind(&mut self, addr: SocketAddr) -> Result<TcpListener, ServerError> {
        TcpListener::bind(addr).await.map_err(ServerError::Bind)
    }

    async fn connect(&mut self, listener: &mut TcpListener) -> Result<TcpStream, ServerError> {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|err| ServerError::AcceptConnection { err })?;
        Ok(stream)
    }
}

#[derive(Debug)]
pub struct Server<C> {
    addr: SocketAddr,
    connector: C,
}

impl Server<TcpConnector> {
    // Avoid compiling the whole function if TLS is not enabled
    #[cfg(feature = "tls")]
    pub async fn with_tls(
        self,
        cert: Vec<u8>,
        privkey: Vec<u8>,
    ) -> Result<Server<TlsConnector>, ServerError> {
        let tls_config = server_tls_config(cert, privkey).await?;
        let tls_config = Arc::new(tls_config);
        let connector = TlsConnector::new(self.connector, tls_config);

        Ok(Server {
            addr: self.addr,
            connector,
        })
    }
}

impl<C> Server<C>
where
    C: Send + 'static,
{
    pub fn new(addr: SocketAddr) -> Server<TcpConnector> {
        Server {
            addr,
            connector: TcpConnector,
        }
    }

    #[instrument(skip(self))]
    pub async fn listen<S>(mut self) -> Result<(), ServerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        C: Connect<S, TcpListener>,
    {
        let mut listener = self.connector.bind(self.addr).await?;

        info!("Listening at {}", self.addr);

        // channel tx/rx to handle error
        let (tx_err, mut rx_err) = tokio::sync::mpsc::channel::<ServerError>(MAX_ERRORS_TO_HANDLE);

        let handles = Arc::new(Mutex::new(Vec::new()));
        let handles_clone = Arc::clone(&handles);

        // accept a new connection
        let handle_connections = tokio::spawn(async move {
            loop {
                let stream = match self.connector.connect(&mut listener).await {
                    Ok(s) => s,
                    Err(err) => {
                        error!("Connection error: {:?}", err);
                        tx_err
                            .send(err)
                            .await
                            .expect("error while sending on channel");
                        break;
                    }
                };

                let handle_single_connection =
                    tokio::spawn(Self::handle_connection(stream, tx_err.clone()));

                handles.lock().await.push(handle_single_connection);
            }
        });

        // join connections and handle errors
        if let Some(err) = rx_err.recv().await {
            Self::terminate(handle_connections, handles_clone).await?;
            error!("Received error {:?}. Terminate all connections.", err);
            return Err(err);
        }

        Ok(())
    }

    // terminate all connections
    #[instrument(skip_all)]
    async fn terminate(
        handle_connections: JoinHandle<()>,
        handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    ) -> Result<(), ServerError> {
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
    async fn handle_connection<S>(stream: S, tx_err: TxErrorType)
    where
        S: AsyncWrite + AsyncRead + Unpin,
    {
        match Self::impl_handle_connection(stream).await {
            Ok(_) => {}
            Err(ServerError::CloseWebsocket)
            | Err(ServerError::Transport(TungsteniteError::Protocol(
                ProtocolError::ResetWithoutClosingHandshake,
            ))) => {
                warn!("Websocket connection closed");
            }
            Err(ServerError::Transport(TungsteniteError::Io(err)))
                if err.kind() == ErrorKind::UnexpectedEof =>
            {
                warn!("Websocket connection closed");
            }
            Err(err) => {
                error!("Fatal error occurred: {}", err);
                tx_err.send(err).await.expect("Error handler failure");
            }
        }
    }

    #[instrument(skip_all)]
    async fn impl_handle_connection<S>(stream: S) -> Result<(), ServerError>
    where
        S: AsyncWrite + AsyncRead + Unpin,
    {
        //create a WebSocket connection
        let ws_stream = accept_async(stream).await.map_err(|err| {
            error!("Websocket error: {:?}", err);
            ServerError::WebSocketHandshake
        })?;

        info!("New WebSocket connection created");

        // separate ownership between receiving and writing part
        let (write, read) = ws_stream.split();

        // Read the received command
        read.map_err(ServerError::Transport)
            .and_then(|msg| async move {
                info!("Received command from the client");
                match msg {
                    // convert the message from a Vec<u8> into a OsString
                    Message::Binary(v) => String::from_utf8(v).map_err(ServerError::Utf8Error),
                    Message::Close(_) => Err(ServerError::CloseWebsocket), // the client closed the connection
                    _ => Err(ServerError::ReadCommand),
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
            .forward(write.sink_map_err(ServerError::Transport))
            .await?;

        Ok(())
    }
}
