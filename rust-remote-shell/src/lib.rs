use futures_util::{future, StreamExt, TryStreamExt};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message; // future, TryStreamExt
                                             //use std::os::unix::prelude::OsStrExt;
use std::ffi::OsStr; // , ffi::OsString, collections::VecDeque,
use std::process;
use std::string::FromUtf8Error;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};

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

async fn execute_cmd<S>(cmd: &[S]) -> Result<process::Output, ShellError>
where
    S: AsRef<OsStr>,
{
    let mut cmd_iter = cmd.iter();
    let cmd_to_exec = cmd_iter.next().ok_or(ShellError::EmptyCommand)?;

    process::Command::new(cmd_to_exec)
        .args(cmd_iter)
        .output()
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
    #[error("Failed to bind on port {0}")]
    BindError(u16),
    #[error("Connected streams should have a peer address")]
    PeerAddrError,
    #[error("Error during the websocket handshake occurred")]
    WebSocketHandshakeError,
    #[error("Error while reading the shell command from websocket")]
    ReadCommandError,
    #[error("Shell error: {0}")]
    DeviceShellError(#[from] ShellError),
    #[error("Error marshaling to UTF8")]
    Utf8Error(#[from] FromUtf8Error),
    #[error("Trasport error from Tungstenite")]
    Transport(#[from] tokio_tungstenite::tungstenite::Error),
}

impl DeviceServerError {
    fn is_fatal(&self) -> bool {
        // distinguish between fatal (cause server failure) and non-fatal error
        // maatch between different kind of errors.
        todo!()
    }
}

pub struct DeviceServer {
    addr: SocketAddr,
    // queue used to store incoming commands in case multiple commands are sent to the device while one is processed.
    // received_commands: VecDeque<Command>,
}

impl DeviceServer {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            //received_commands: VecDeque::new(),
        }
    }

    pub async fn listen(&self) -> Result<(), DeviceServerError> {
        let try_socket = TcpListener::bind(self.addr).await;
        let socket = try_socket.map_err(|_| DeviceServerError::BindError(self.addr.port()))?;

        println!("Listening on: {}", self.addr);

        // TODO
        // Destrutturare self in modo tale da prendere un mutable reference alla lista di comandi
        // Usare un mutex per passare la coda di comandi ad ogni handle_connection
        // pass the queue of commands to each spawned task.

        let (tx_err, mut rx_err) = tokio::sync::mpsc::channel::<DeviceServerError>(10);

        let handles = Arc::new(Mutex::new(Vec::new()));
        let handles_clone = Arc::clone(&handles);

        // accept a new connection
        let handle_connections = tokio::spawn(async move {
            while let Ok((stream, _)) = socket.accept().await {
                let handle_single_connection = tokio::spawn(
                    Self::handle_connection(stream, tx_err.clone()), // TODO: GESTIRE ERRORE
                );

                handles_clone.lock().await.push(handle_single_connection);
            }
        });

        // join connections
        if let Some(err) = rx_err.recv().await {
            handle_connections.abort();
            let _ = handle_connections.await; // TODO print error

            for h in handles.lock().await.iter() {
                h.abort();
            }

            return Err(err);
        }

        Ok(())
    }

    // create a websocket connection and
    async fn handle_connection(
        stream: TcpStream,
        tx_err: tokio::sync::mpsc::Sender<DeviceServerError>,
    ) {
        match Self::impl_handle_connection(stream).await {
            Ok(_) => {}
            Err(err) if err.is_fatal() => tx_err.send(err).await.expect("Error handler failure"),
            Err(_) => todo!(),
        }
    }

    async fn impl_handle_connection(stream: TcpStream) -> Result<(), DeviceServerError> {
        let addr = stream
            .peer_addr()
            .map_err(|_| DeviceServerError::PeerAddrError)?;

        // create a WebSocket connection
        let try_web_socket_stream = tokio_tungstenite::accept_async(stream).await;
        let web_socket_stream =
            try_web_socket_stream.map_err(|_| DeviceServerError::WebSocketHandshakeError)?;

        println!("New WebSocket connection created: {}", addr);

        // separate ownership between receiving and writing part
        let (_write, read) = web_socket_stream.split();

        // Read the received command
        read.map_err(DeviceServerError::Transport)
            .and_then(|msg| {
                let cmd = match msg {
                    // convert the message from a Vec<u8> into a OsString --> OsStr::from_bytes(&Vec<u8>).to_owned()
                    Message::Text(t) => Ok(t),
                    Message::Binary(v) => {
                        String::from_utf8(v).map_err(DeviceServerError::Utf8Error)
                    }
                    _ => Err(DeviceServerError::ReadCommandError),
                };
                future::ready(cmd)
            })
            .try_for_each(|cmd| async move {
                // convert the command into the correct format
                let cmd = shellwords::split(&cmd)
                    .map_err(|_| DeviceServerError::DeviceShellError(ShellError::MalformedInput))?;

                // compute the command
                let cmd_out = cmd_from_input(&cmd)
                    .await
                    .map_err(DeviceServerError::DeviceShellError)?; // TODO: MAKE THIS AN ASYNC FUNCTION
                println!("Command output: {}", cmd_out);

                // WE SHOULD COMPUTE THE OUTPUT AND SEND IT TO THE CLIENT, NOT PRINTING IT

                /*
                read.try_filter(|msg| future::ready(msg.is_text() || msg.is_binary()))
                    .forward(write)
                    .await
                    .expect("Failed to forward messages")
                 */

                Ok(())
            })
            .await?;

        Ok(())
    }
}
