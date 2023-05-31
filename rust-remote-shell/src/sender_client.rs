use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::SplitSink;
use futures::{StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio::task::JoinHandle;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, instrument, trace};
use url::Url;

use crate::io_handler::IOHandler;

#[cfg(feature = "tls")]
use crate::tls::*;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("Error while trying to connect with server")]
    WebSocketConnect(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("IO error occurred while reading from stdin")]
    IORead(#[from] std::io::Error),
    #[error("IO error occurred while writing to stdout")]
    IOWrite {
        #[source]
        err: std::io::Error,
    },
    #[error("Failed to read CA certificate from file")]
    ReadCAFile {
        #[source]
        err: std::io::Error,
    },
    #[error("Error while trying to send the output of a command to the main task")]
    Channel(#[from] SendError<Message>),
    #[error("Error from Tungstenite while reading command")]
    TungsteniteReadData {
        #[source]
        err: tokio_tungstenite::tungstenite::Error,
    },
    #[error("Error from Tungstenite while closing websocket connection")]
    TungsteniteClose {
        #[source]
        err: tokio_tungstenite::tungstenite::Error,
    },
    #[error("Server disconnected")]
    Disconnected,
}

#[async_trait]
pub trait ClientConnect {
    async fn connect(&mut self) -> Result<(), ClientError>;
    fn get_ws_stream(self) -> Option<WebSocketStream<MaybeTlsStream<TcpStream>>>;
}

pub struct Client<C> {
    pub connector: C,
}

impl Client<TcpClientConnector> {
    #[cfg(feature = "tls")]
    pub async fn with_tls(
        self,
        ca_cert: Vec<u8>,
    ) -> Result<Client<TlsClientConnector>, ClientError> {
        let tls_connector = client_tls_config(ca_cert).await;
        let connector = TlsClientConnector::new(self.connector, tls_connector);
        Ok(Client { connector })
    }
}

impl<C> Client<C>
where
    C: ClientConnect + 'static,
{
    pub async fn new(listener_url: Url) -> Result<Client<TcpClientConnector>, ClientError> {
        let connector = TcpClientConnector::new(listener_url).await?;
        Ok(Client { connector })
    }

    pub async fn connect(&mut self) -> Result<(), ClientError> {
        self.connector.connect().await
    }

    #[instrument(skip_all)]
    pub async fn handle_connection(self) -> Result<(), ClientError> {
        let (write, read) = self
            .connector
            .get_ws_stream()
            .expect("websocket connection must have been previously created")
            .split();

        let (tx_cmd_out, rx_cmd_out) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let rx_cmd_out = Arc::new(Mutex::new(rx_cmd_out));
        let rx_cmd_out_clone = Arc::clone(&rx_cmd_out);

        let (tx_err, mut rx_err) = tokio::sync::mpsc::channel::<Result<(), ClientError>>(1);

        // handle stdin and stdout
        let handle_std_in_out =
            tokio::spawn(Self::read_write(write, rx_cmd_out_clone, tx_err.clone()));

        let handle_read = tokio::spawn(async move {
            let res = read
                .map_err(|err| ClientError::TungsteniteReadData { err })
                .try_for_each(|cmd_out| async {
                    tx_cmd_out.send(cmd_out).map_err(ClientError::Channel)
                })
                .await;

            if let Err(err) = res {
                tx_err.send(Err(err)).await.expect("channel error");
            }

            Ok(())
        });

        let mut handles = [handle_std_in_out, handle_read];

        match rx_err.recv().await.expect("channel error") {
            Ok(()) => {
                info!("Closing websocket connection");
                Self::close(&mut handles, rx_cmd_out).await
            }
            Err(err) => {
                error!("Fatal error: {}", err);
                Self::close(&mut handles, rx_cmd_out).await?;
                Err(err)
            }
        }
    }

    async fn read_write(
        write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        rx: Arc<Mutex<UnboundedReceiver<Message>>>,
        tx_err: Sender<Result<(), ClientError>>,
    ) -> Result<(), ClientError> {
        let mut iohandler = IOHandler::new(write, tx_err);

        // read from stdin and, if messages are present on the channel (rx) print them to the stdout
        loop {
            iohandler.read_stdin().await?;
            if iohandler.is_exited() {
                break Ok(());
            }
            iohandler.send_to_server().await?;
            iohandler.write_stdout(&rx).await?;
        }
    }

    #[instrument(skip_all)]
    async fn close(
        handles: &mut [JoinHandle<Result<(), ClientError>>],
        rx_cmd_out: Arc<Mutex<UnboundedReceiver<Message>>>,
    ) -> Result<(), ClientError> {
        // abort the current active tasks
        for h in handles.iter() {
            h.abort();
        }

        for h in handles {
            match h.await {
                Err(err) if !err.is_cancelled() => {
                    error!("Join failed: {}", err)
                }
                Err(_) => {
                    trace!("Task cancelled")
                }
                Ok(res) => {
                    debug!("Task joined with: {:?}", res)
                }
            }
        }

        // write the remaining elements from cmd out buffer to stdout
        let mut channel = rx_cmd_out.lock().await;
        let mut stdout = tokio::io::stdout();
        while let Ok(cmd_out) = channel.try_recv() {
            let data = cmd_out.into_data();
            stdout
                .write(&data)
                .await
                .map_err(|err| ClientError::IOWrite { err })?;
            stdout
                .flush()
                .await
                .map_err(|err| ClientError::IOWrite { err })?;
        }

        info!("Client terminated");

        Ok(())
    }
}

#[derive(Debug)]
pub struct TcpClientConnector {
    listener_url: Url,
    ws_stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

#[async_trait]
impl ClientConnect for TcpClientConnector {
    #[instrument(skip(self))]
    async fn connect(&mut self) -> Result<(), ClientError> {
        use tokio_tungstenite::connect_async;
        // Websocket connection to an existing server
        let (ws_stream, _) = connect_async(self.listener_url.clone())
            .await
            .map_err(|err| {
                error!("Websocket error: {:?}", err);
                ClientError::WebSocketConnect(err)
            })?;

        info!("WebSocket handshake has been successfully completed on a NON-TLS protected stream");

        self.ws_stream = Some(ws_stream);

        Ok(())
    }

    #[instrument(skip(self))]
    fn get_ws_stream(self) -> Option<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        self.ws_stream
    }
}

// TODO: CAPIRE SE SPOSTARE METODI DENTRO LA STRUCT CLIENT
impl TcpClientConnector {
    pub async fn new(listener_url: Url) -> Result<Self, ClientError> {
        Ok(Self {
            listener_url,
            ws_stream: None,
        })
    }

    pub fn get_listener_url(&self) -> Url {
        self.listener_url.clone()
    }

    pub fn set_ws_stream(&mut self, ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>) {
        self.ws_stream = Some(ws_stream);
    }
}
