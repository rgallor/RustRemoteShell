use std::ffi::OsStr;
use std::io;
use std::net::SocketAddr;
use std::string::FromUtf8Error;
use std::sync::Arc;

use futures::{future, SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};
use tracing::{error, info, instrument};
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

async fn execute_cmd<S>(cmd: &[S]) -> Result<std::process::Output, ShellError>
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

pub async fn cmd_from_input<S>(cmd: &[S]) -> Result<String, ShellError>
where
    S: AsRef<OsStr>,
{
    // before calling this function the binary should ensure that the input in in the correct sintactic format

    // try executing the command.
    // If the error states that the command does not exists, throw WrongCommand(cmd.split(' ').first().unwrap())
    let cmd_out = execute_cmd(cmd).await?;

    std::string::String::from_utf8(cmd_out.stdout)
        // if the conversion from UTF8 to String goes wrong, return an error
        .map_err(ShellError::WrongOutConversion)
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

        // join connections
        if let Some(err) = rx_err.recv().await {
            // terminate all connections
            handle_connections.abort();
            let _ = handle_connections.await;

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
            Err(DeviceServerError::CloseWebsocket) => info!("Websocket connection closed"), // TODO: check that the connection is effectively closed on the server-side (not only on the client-side)
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
                    // convert the message from a Vec<u8> into a OsString --> OsStr::from_bytes(&Vec<u8>).to_owned()
                    Message::Text(t) => Ok(t),
                    Message::Binary(v) => {
                        String::from_utf8(v).map_err(DeviceServerError::Utf8Error)
                    }
                    Message::Close(_) => Err(DeviceServerError::CloseWebsocket),
                    _ => Err(DeviceServerError::ReadCommand),
                };
                info!("Received command from the client");
                future::ready(cmd)
            })
            .and_then(|cmd| async move {
                // convert the command into the correct format
                let cmd =
                    shellwords::split(&cmd).unwrap_or(vec!["Malformed command.\n".to_string()]);

                // compute the command
                let cmd_out = cmd_from_input(&cmd)
                    .await
                    .unwrap_or(String::from("Incorrect command.\n"));

                info!("Send command output to the client");

                Ok(Message::Binary(cmd_out.as_bytes().to_vec()))
            })
            .forward(write.sink_map_err(DeviceServerError::Transport))
            .await?;

        Ok(())
    }
}

#[derive(Debug)]
pub struct SenderClient {
    listener_url: Url,
    // id: usize,
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
    SendOutput(#[from] tokio::sync::mpsc::error::SendError<Message>),
    #[error("Error from Tungstenite while reading command")]
    TungsteniteReadData {
        #[source]
        err: tokio_tungstenite::tungstenite::Error,
    },
}

impl SenderClient {
    pub fn new(listener_url: Url) -> Self {
        info!("Create client");
        Self { listener_url }
    }

    #[instrument(skip(self))]
    pub async fn connect(&self) -> Result<(), SenderClientError> {
        // Websocket connection to an existing server
        let (mut ws_stream, _) = connect_async(self.listener_url.clone())
            .await
            .map_err(SenderClientError::WebSocketConnect)?;

        info!("WebSocket handshake has been successfully completed");

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut cmd = String::new();
        let mut stdout = tokio::io::stdout();

        // loop to read a command from stdin, wait for its output and write it to stdout
        loop {
            cmd.clear();
            // read a shell command into the stdin and send it to the server
            reader
                .read_line(&mut cmd)
                .await
                .map_err(SenderClientError::IORead)?;

            // check if the command is exit. Eventually, close the connection
            if cmd.starts_with("exit") {
                ws_stream
                    .close(None)
                    .await
                    .expect("Error while closing websocket connection");
                info!("Closed websocket on client side");
                break Ok(());
            }

            info!("Send command to the server");
            ws_stream
                .send(Message::Binary(cmd.as_bytes().to_vec()))
                .await
                .expect("error while sending a command through websocket to the server");

            // read command shell output from the websocket
            let msg = match ws_stream.next().await {
                None => todo!(), // connection closed / server stops
                Some(res) => res.map_err(|err| SenderClientError::TungsteniteReadData { err })?,
            };

            let data = msg.into_data();
            info!("Returned cmd out to the Client");

            stdout
                .write(&data)
                .await
                .map_err(|err| SenderClientError::IOWrite { err })?;
            stdout.flush().await.expect("writing stdout");
        }
    }
}
