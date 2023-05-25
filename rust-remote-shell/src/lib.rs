use std::ffi::OsStr;
use std::io;
use std::net::SocketAddr;
use std::string::FromUtf8Error;
use std::sync::Arc;

use futures::stream::SplitSink;
use futures::{future, SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader, Stdin, Stdout};
use tokio::net::{TcpListener, TcpStream};
use tokio::process;
use tokio::sync::mpsc::error::{SendError, TryRecvError};
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio::sync::MutexGuard;
use tokio::task::JoinHandle;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{error, info, instrument, warn};
use url::Url;

#[derive(Error, Debug)]
pub enum ShellError {
    #[error("Empty command")]
    EmptyCommand,
    #[error("Malformed input")]
    MalformedInput,
    #[error("Command {cmd} does not exists")]
    WrongCommand {
        cmd: String,
        #[source]
        error: io::Error,
    },
    #[error("The execution of the command caused an error while formatting the output into UTF8")]
    WrongOutConversion(#[from] FromUtf8Error),
}

pub struct CommandHandler;

impl CommandHandler {
    fn new() -> Self {
        Self
        // TODO: open a remote shell (to the input IP addr)
    }

    pub async fn execute(&self, cmd: String) -> Result<String, ShellError> {
        // convert the command into the correct format
        let cmd = shellwords::split(&cmd).map_err(|_| ShellError::MalformedInput)?;

        // try executing the command.
        let cmd_out = self.inner_execute(&cmd).await?;

        std::string::String::from_utf8(cmd_out.stdout)
            // if the conversion from UTF8 to String goes wrong, return an error
            .map_err(ShellError::WrongOutConversion)
    }

    async fn inner_execute<S>(&self, cmd: &[S]) -> Result<std::process::Output, ShellError>
    where
        S: AsRef<OsStr>,
    {
        let mut cmd_iter = cmd.iter();
        let cmd_to_exec = cmd_iter.next().ok_or(ShellError::EmptyCommand)?;

        process::Command::new(cmd_to_exec)
            .args(cmd_iter)
            .output()
            .await
            .map_err(|e| ShellError::WrongCommand {
                cmd: cmd_to_exec.as_ref().to_string_lossy().to_string(),
                error: e,
            })
    }
}

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
        let (tx_err, mut rx_err) = tokio::sync::mpsc::channel::<DeviceServerError>(10);

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
            // terminate all connections
            handle_connections.abort();

            match handle_connections.await {
                Err(err) if !err.is_cancelled() => error!("Join failed: {}", err),
                _ => {}
            }

            for h in handles.lock().await.iter() {
                h.abort();
            }

            error!("Received error {:?}. Terminate all connections.", err);
            return Err(err);
        }

        Ok(())
    }

    #[instrument(skip_all)]
    async fn handle_connection(stream: TcpStream, tx_err: TxErrorType) {
        match Self::impl_handle_connection(stream).await {
            Ok(_) => {}
            Err(DeviceServerError::CloseWebsocket) => {
                info!("Websocket connection closed");
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

#[derive(Error, Debug)]
pub enum SenderClientError {
    #[error("Error while trying to connect with server")]
    WebSocketConnect(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("IO error occurred while reading from stdin")]
    IORead(#[from] std::io::Error),
    #[error("IO error occurred while writing to stdout")]
    IOWrite {
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
    #[error("Server disconnected")]
    Disconnected,
}

#[derive(Debug)]
pub struct IOHandler {
    stdout: Stdout,
    reader: BufReader<Stdin>,
    write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    tx_err: Sender<Result<(), SenderClientError>>,
    buf_cmd: String,
}

impl IOHandler {
    fn new(
        write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        tx_err: Sender<Result<(), SenderClientError>>,
    ) -> Self {
        Self {
            stdout: tokio::io::stdout(),
            reader: BufReader::new(tokio::io::stdin()),
            write,
            tx_err,
            buf_cmd: String::new(),
        }
    }

    #[instrument(skip_all)]
    async fn read_stdin(&mut self) -> Result<(), SenderClientError> {
        self.buf_cmd.clear();

        // read a shell command into the stdin and send it to the server
        self.reader
            .read_line(&mut self.buf_cmd)
            .await
            .map_err(SenderClientError::IORead)?;

        if self.check_exit() {
            self.exit().await?;
        }

        Ok(())
    }

    #[instrument(skip_all)]
    fn check_exit(&self) -> bool {
        self.buf_cmd.starts_with("exit")
    }

    #[instrument(skip_all)]
    async fn exit(&mut self) -> Result<(), SenderClientError> {
        // check if the command is exit. Eventually, close the connection

        self.write
            .send(Message::Close(None))
            .await
            .expect("Error while closing websocket connection");
        info!("Closed websocket on client side");

        self.tx_err.send(Ok(())).await.expect("channel error");

        Ok(()) // send Ok(()) to close the connection on client side
               //break Ok(());
    }

    #[instrument(skip_all)]
    async fn send_to_server(&mut self) -> Result<(), SenderClientError> {
        info!("Send command to the server");
        self.write
            .send(Message::Binary(self.buf_cmd.as_bytes().to_vec()))
            .await
            .map_err(|err| SenderClientError::TungsteniteReadData { err })?;

        info!("Command sent: {}", self.buf_cmd);

        Ok(())
    }

    #[instrument(skip_all)]
    async fn impl_write_stdout(&mut self, msg: Message) -> Result<(), SenderClientError> {
        let data = msg.into_data();

        self.stdout
            .write(&data)
            .await
            .map_err(|err| SenderClientError::IOWrite { err })?;

        self.stdout.flush().await.expect("writing stdout");

        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn write_stdout(
        &mut self,
        rx: Arc<Mutex<UnboundedReceiver<Message>>>,
    ) -> Result<(), SenderClientError> {
        // check if there are command outputs stored in the channel. Eventually, print them to the stdout
        let mut channel = rx.lock().await;

        // wait to receive the first command output
        let msg = channel.recv().await.unwrap();

        self.impl_write_stdout(msg).await?;

        self.empty_buffer(channel).await?;

        Ok(())
    }

    async fn empty_buffer(
        &mut self,
        mut channel: MutexGuard<'_, UnboundedReceiver<Message>>,
    ) -> Result<(), SenderClientError> {
        loop {
            match channel.try_recv() {
                Ok(msg) => {
                    self.impl_write_stdout(msg).await?;
                }
                Err(TryRecvError::Empty) => {
                    // the channel is empty but the connection is still open
                    break Ok(()); // TODO: check that Ok(()) is a good return value
                }
                Err(TryRecvError::Disconnected) => {
                    unreachable!("the channel should not be dropped before the task is aborted")
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct SenderClient {
    listener_url: Url,
}

impl SenderClient {
    pub fn new(listener_url: Url) -> Self {
        Self { listener_url }
    }

    async fn read_write(
        write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        rx: Arc<Mutex<UnboundedReceiver<Message>>>,
        tx_err: Sender<Result<(), SenderClientError>>,
    ) -> Result<(), SenderClientError> {
        let mut iohandler = IOHandler::new(write, tx_err);

        // read from stdin and, if messages are present on the channel (rx) print them to the stdout
        loop {
            iohandler.read_stdin().await?;
            iohandler.send_to_server().await?;
            iohandler.write_stdout(Arc::clone(&rx)).await?;
        }
        /*
        loop {
            cmd.clear();
            // read a shell command from the stdin and send it to the server
            reader
                .read_line(&mut cmd)
                .await
                .map_err(SenderClientError::IORead)?;

            // check if the command is exit. Eventually, close the connection
            if cmd.starts_with("exit") {
                write
                    .send(Message::Close(None))
                    .await
                    .expect("Error while closing websocket connection");
                info!("Closed websocket on client side");
                tx_err.send(Ok(())).await.expect("channel error"); // send Ok(()) to close the connection on client side
                break Ok(());
            }

            info!("Send command to the server");
            write
                .send(Message::Binary(cmd.as_bytes().to_vec()))
                .await
                .expect("error while sending a command through websocket to the server");
            info!("Command sent: {}", cmd);

            // check if there are command outputs stored in the channel. Eventually, print them to the stdout
            let mut channel = rx.lock().await;

            let some = channel.recv().await.unwrap();
            let data = some.into_data();

            stdout
                .write(&data)
                .await
                .map_err(|err| SenderClientError::IOWrite { err })?;
            stdout.flush().await.expect("writing stdout");

            // TODO: define function
            loop {
                match channel.try_recv() {
                    Ok(cmd_out) => {
                        let data = cmd_out.into_data();

                        stdout
                            .write(&data)
                            .await
                            .map_err(|err| SenderClientError::IOWrite { err })?;
                        stdout.flush().await.expect("writing stdout");
                    }
                    Err(TryRecvError::Empty) => {
                        // the channel is empty but the connection is still open
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        unreachable!("the channel should not be dropped before the task is aborted")
                    }
                }
            }
        }
        */
    }

    #[instrument(skip(self))]
    pub async fn connect(&mut self) -> Result<(), SenderClientError> {
        // Websocket connection to an existing server
        let (ws_stream, _) = connect_async(self.listener_url.clone())
            .await
            .map_err(SenderClientError::WebSocketConnect)?;

        info!("WebSocket handshake has been successfully completed");

        let (write, read) = ws_stream.split();

        let (tx_cmd_out, rx_cmd_out) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let rx_cmd_out = Arc::new(Mutex::new(rx_cmd_out));
        let rx_cmd_out_clone = Arc::clone(&rx_cmd_out);

        let (tx_err, mut rx_err) = tokio::sync::mpsc::channel::<Result<(), SenderClientError>>(1);

        // handle stdin and stdout
        let handle_std_in_out =
            tokio::spawn(Self::read_write(write, rx_cmd_out_clone, tx_err.clone()));

        let handle_read = tokio::spawn(async move {
            let res = read
                .map_err(|err| SenderClientError::TungsteniteReadData { err })
                .try_for_each(|cmd_out| async {
                    tx_cmd_out.send(cmd_out).map_err(SenderClientError::Channel)
                })
                .await;

            if let Err(err) = res {
                tx_err.send(Err(err)).await.expect("channel error");
            }

            Ok(())
        });

        let handles = vec![handle_std_in_out, handle_read];

        match rx_err.recv().await.expect("channel error") {
            Ok(()) => {
                info!("Closing websocket connection");
                Self::close(handles, rx_cmd_out).await
            }
            Err(err) => {
                error!("Fatal error: {}", err);
                Self::close(handles, rx_cmd_out).await
            }
        }
    }

    #[instrument(skip_all)]
    async fn close(
        handles: Vec<JoinHandle<Result<(), SenderClientError>>>,
        rx_cmd_out: Arc<Mutex<UnboundedReceiver<Message>>>,
    ) -> Result<(), SenderClientError> {
        // abort the current active tasks
        for h in handles.iter() {
            h.abort();
        }

        for h in handles {
            match h.await {
                Err(err) if !err.is_cancelled() => {
                    error!("Join failed: {}", err)
                }
                _ => {}
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
                .map_err(|err| SenderClientError::IOWrite { err })?;
            stdout.flush().await.expect("writing stdout");
        }

        info!("EXIT");

        Ok(())
    }
}
