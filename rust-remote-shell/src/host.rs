//! Host implementation.
//!
//! This module provides a [`Host`] and [`HostBuilder`] structs necessary for the creation of a new host and
//! for the establishment of a new WebSocket connection with a device.
//! It also provides [`HostService`] struct, representing a [`tower`] service which handles the communication with a device.

use std::fmt::Debug;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;

use tokio::io::{AsyncBufReadExt, BufReader};

use futures::stream::SplitSink;
use futures::{Future, SinkExt, StreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::SendError;

use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tower::layer::util::Stack;
use tower::{layer::util::Identity, Service, ServiceBuilder};
use tracing::{error, info, instrument};

use crate::protocol::{DeviceMsg, DeviceMsgStream, ProtocolLayer};
use crate::websocket::WebSocketLayer;

#[cfg(feature = "tls")]
use crate::tls::{self, Error as TlsError, TlsLayer};

/// Host error.
#[derive(Error, Debug)]
pub enum HostError {
    /// Error while trying to connect with a device.
    #[error("Error while trying to connect with a device.")]
    WebSocketConnect(#[from] tokio_tungstenite::tungstenite::Error),

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
    TungsteniteReadData(#[source] tokio_tungstenite::tungstenite::Error),

    /// Error from Tungstenite while closing websocket connection
    #[error("Error from Tungstenite while closing websocket connection")]
    TungsteniteClose(#[source] tokio_tungstenite::tungstenite::Error),

    /// Sender channel was dropped before sending message.
    #[error("Sender channel was dropped before sending message.")]
    ChannelDropped,

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
        // TODO: find a way to handle multiple device connections
        loop {
            let (stream, _) = self.listener.accept().await.map_err(HostError::Listen)?;
            info!("Connection accepted.");
            // handle the connection by sending messages and printing out responses from the device
            service.call(stream).await?;
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
            .layer(ProtocolLayer) // TODO: PROTOCOL (add also in the TLS serve() method)
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
    ws_stream: SplitSink<WebSocketStream<U>, Message>, // From<Protocol> for Message
    msg_stream: DeviceMsgStream,
}

impl<U> HandleConnection<U>
where
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Create a new handler for a connection.
    pub fn new(
        msg_stream: DeviceMsgStream,
        ws_stream: SplitSink<WebSocketStream<U>, Message>,
    ) -> Self {
        Self {
            ws_stream,
            msg_stream,
        }
    }

    /// Connection handler
    #[instrument(skip_all)]
    pub async fn handle_connection(mut self) -> Result<(), HostError> {
        // TODO: read from stdin
        let mut buf = String::new();
        let mut reader = BufReader::new(tokio::io::stdin());

        let _byte_read = reader
            .read_line(&mut buf)
            .await
            .map_err(HostError::IORead)?;

        self.ws_stream
            .send(Message::Binary(buf.as_bytes().to_vec()))
            .await
            .map_err(HostError::TungsteniteReadData)?;

        while let Some(msg) = self.msg_stream.next().await {
            let msg = msg?;
            match msg {
                DeviceMsg::Eof => {
                    info!("EOF received");
                    return Ok(());
                }
                _ => unimplemented!(), // TODO: implement receipt of a message output
            }
        }

        Ok(())

        // let (write, read) = self.msg_stream.split();

        // // Define a channel used to communicate command output from a task responsible for receiving commands output from a device to a task responsible
        // // of printing them to the stdout.
        // let (tx_cmd_out, rx_cmd_out) = tokio::sync::mpsc::unbounded_channel::<Message>();
        // let rx_cmd_out = Arc::new(Mutex::new(rx_cmd_out));
        // let rx_cmd_out_clone = Arc::clone(&rx_cmd_out);

        // // Define a channel used to communicate errors between tasks.
        // let (tx_err, mut rx_err) = tokio::sync::mpsc::channel::<Result<(), HostError>>(1);

        // // Define a task to handle stdin and stdout.
        // let handle_std_in_out =
        //     tokio::spawn(Self::read_write(write, rx_cmd_out_clone, tx_err.clone()));

        // // Define a task to handle the reception of commands output.
        // let handle_read = tokio::spawn(async move {
        //     let res = read
        //         .map_err(HostError::TungsteniteReadData)
        //         .try_for_each(|cmd_out| async {
        //             tx_cmd_out.send(cmd_out).map_err(HostError::ChannelMsg)
        //         })
        //         .await;

        //     if let Err(err) = res {
        //         tx_err.send(Err(err)).await.expect("channel error");
        //     }

        //     Ok(())
        // });

        // let mut handles = [handle_std_in_out, handle_read];

        // // handle possible errors
        // let res = rx_err.recv().await.ok_or(HostError::ChannelDropped)?;
        // match res {
        //     // Close the websocket connection when the device get disconnected.
        //     Ok(())
        //     | Err(HostError::TungsteniteReadData(
        //         tokio_tungstenite::tungstenite::Error::Protocol(
        //             ProtocolError::ResetWithoutClosingHandshake,
        //         ),
        //     )) => {
        //         info!("Closing websocket connection due to device interruption");
        //         Self::close(&mut handles, rx_cmd_out).await
        //     }
        //     // Report an error after closing the connection if something else happens.
        //     Err(err) => {
        //         error!("Fatal error: {:?}", err);
        //         Self::close(&mut handles, rx_cmd_out).await?;
        //         Err(err)
        //     }
        // }
    }

    // Function responsible for reading from stdin and printing eventual messages output into the stdout
    // async fn read_write(
    //     write: SplitSink<WebSocketStream<U>, Message>,
    //     rx: Arc<Mutex<UnboundedReceiver<Message>>>,
    //     tx_err: Sender<Result<(), HostError>>,
    // ) -> Result<(), HostError> {
    //     let mut io_handler = IoHandler::new(write, tx_err);

    //     loop {
    //         io_handler.read_stdin().await?;
    //         if io_handler.is_exited() {
    //             break Ok(());
    //         }
    //         io_handler.send_to_device().await?;
    //         io_handler.write_stdout(&rx).await?;
    //     }
    // }

    // // Abort the current active tasks and, if available, print to the stout the buffered commands output.
    // #[instrument(skip_all)]
    // async fn close(
    //     handles: &mut [JoinHandle<Result<(), HostError>>],
    //     rx_cmd_out: Arc<Mutex<UnboundedReceiver<Message>>>,
    // ) -> Result<(), HostError> {
    //     for h in handles.iter() {
    //         h.abort();
    //     }

    //     for h in handles {
    //         match h.await {
    //             Err(err) if !err.is_cancelled() => {
    //                 error!("Join failed: {}", err);
    //             }
    //             Err(_) => {
    //                 trace!("Task cancelled");
    //             }
    //             Ok(res) => {
    //                 debug!("Task joined with: {:?}", res);
    //             }
    //         }
    //     }

    //     let mut channel = rx_cmd_out.lock().await;
    //     let mut stdout = tokio::io::stdout();
    //     while let Ok(cmd_out) = channel.try_recv() {
    //         let data = cmd_out.into_data();
    //         stdout.write(&data).await.map_err(HostError::IOWrite)?;
    //         stdout.flush().await.map_err(HostError::IOWrite)?;
    //     }

    //     info!("Client terminated");

    //     Ok(())
    // }
}
