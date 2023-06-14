use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;

use futures::stream::SplitSink;
use futures::{StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tower::layer::util::Stack;
use tower::{layer::util::Identity, Service, ServiceBuilder};
use tracing::{debug, error, info, instrument, trace};

use crate::io_handler::IOHandler;
use crate::service::WebSocketLayer;

#[cfg(feature = "tls")]
use crate::tls::{self, TlsLayer};

#[derive(Error, Debug)]
pub enum HostError {
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

pub struct Host {
    listener: TcpListener,
}

impl Host {
    pub async fn bind(addr: SocketAddr) -> HostBuilder<Identity> {
        let listener = TcpListener::bind(addr).await.expect("err");
        let server = Self { listener };

        HostBuilder {
            server,
            builder: ServiceBuilder::new(),
        }
    }

    pub async fn listen<S, E>(&mut self, mut service: S)
    where
        S: Service<TcpStream, Error = E> + Clone + 'static,
        E: Debug,
    {
        loop {
            // wait for a connection from a device
            let (stream, _) = self.listener.accept().await.expect("err");
            // handle the connection by sending messages and printing out responses from the device
            service.call(stream).await.expect("err");
        }
    }
}

pub struct HostBuilder<L> {
    server: Host,
    builder: ServiceBuilder<L>,
}

impl<L> HostBuilder<L> {
    pub fn fields(self) -> (Host, ServiceBuilder<L>) {
        (self.server, self.builder)
    }
}

impl HostBuilder<Identity> {
    #[cfg(feature = "tls")]
    pub async fn with_tls<C, P>(self, cert: C, privkey: P) -> HostBuilder<Stack<TlsLayer, Identity>>
    where
        C: Into<Vec<u8>>,
        P: Into<Vec<u8>>,
    {
        let acceptor = tls::acceptor(cert, privkey).await.expect("err");

        HostBuilder {
            server: self.server,
            builder: self.builder.layer(TlsLayer::new(acceptor)),
        }
    }

    pub async fn serve<S, E>(mut self, service: S)
    where
        S: Service<WebSocketStream<TcpStream>, Error = E> + Clone + 'static,
        E: Debug,
    {
        let service = self.builder.layer(WebSocketLayer).service(service);

        self.server.listen(service).await;
    }
}

// the host sends commands, wait for device's answer and display it
pub async fn host_handle<U>(stream: WebSocketStream<U>) -> Result<(), ()>
where
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // let (mut write, read) = stream.split();

    // write
    //     .send(Message::Binary("ls".as_bytes().to_vec()))
    //     .await
    //     .expect("err");

    // let _ = read
    //     .try_for_each(|cmd_out| async move {
    //         println!("OUTPUT: {}", cmd_out);
    //         Ok(())
    //     })
    //     .await;

    let handler = HandleConnection::new(stream);
    handler.handle_connection().await.expect("err");

    Ok(())
}

struct HandleConnection<U> {
    ws_stream: WebSocketStream<U>,
}

impl<U> HandleConnection<U>
where
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    fn new(ws_stream: WebSocketStream<U>) -> Self {
        Self { ws_stream }
    }

    #[instrument(skip_all)]
    pub async fn handle_connection(self) -> Result<(), HostError> {
        let (write, read) = self.ws_stream.split();

        let (tx_cmd_out, rx_cmd_out) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let rx_cmd_out = Arc::new(Mutex::new(rx_cmd_out));
        let rx_cmd_out_clone = Arc::clone(&rx_cmd_out);

        let (tx_err, mut rx_err) = tokio::sync::mpsc::channel::<Result<(), HostError>>(1);

        // handle stdin and stdout
        let handle_std_in_out =
            tokio::spawn(Self::read_write(write, rx_cmd_out_clone, tx_err.clone()));

        let handle_read = tokio::spawn(async move {
            let res = read
                .map_err(|err| HostError::TungsteniteReadData { err })
                .try_for_each(|cmd_out| async {
                    tx_cmd_out.send(cmd_out).map_err(HostError::Channel)
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
                error!("Fatal error: {:?}", err);
                Self::close(&mut handles, rx_cmd_out).await?;
                Err(err)
            }
        }
    }

    async fn read_write(
        write: SplitSink<WebSocketStream<U>, Message>,
        rx: Arc<Mutex<UnboundedReceiver<Message>>>,
        tx_err: Sender<Result<(), HostError>>,
    ) -> Result<(), HostError> {
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
        handles: &mut [JoinHandle<Result<(), HostError>>],
        rx_cmd_out: Arc<Mutex<UnboundedReceiver<Message>>>,
    ) -> Result<(), HostError> {
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
                .map_err(|err| HostError::IOWrite { err })?;
            stdout
                .flush()
                .await
                .map_err(|err| HostError::IOWrite { err })?;
        }

        info!("Client terminated");

        Ok(())
    }
}