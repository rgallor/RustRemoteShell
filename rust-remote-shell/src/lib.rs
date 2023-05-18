use futures_util::StreamExt; // future, TryStreamExt
use std::io;
use std::net::SocketAddr;
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

fn execute_cmd<S>(cmd: &[S]) -> Result<process::Output, ShellError>
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

pub fn cmd_from_input<S>(cmd: &[S]) -> Result<String, ShellError>
where
    S: AsRef<OsStr>,
{
    // before calling this function the binary should ensure that the input in in the correct sintactic format

    // try executing the command.
    // If the error states that the command does not exists, throw WrongCommand(cmd.split(' ').first().unwrap())
    let cmd_out = execute_cmd(cmd)?;

    std::string::String::from_utf8(cmd_out.stdout)
        // if the conversion from UTF8 to String goes wrong, return an error
        .map_err(ShellError::WrongOutConversion)
}

// type Command = OsString;

#[derive(Error, Debug)]
pub enum DeviceServerError {
    #[error("Failed to bind on port {0}")]
    BindError(u16),
    #[error("Connected streams should have a peer address")]
    PeerAddrError,
    #[error("Error during the websocket handshake occurred")]
    WebSocketHandshakeError,
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

        // accept a new connection
        while let Ok((stream, _)) = socket.accept().await {
            tokio::spawn(handle_connection(stream));
        }

        Ok(())
    }
}

// create a websocket connection and
async fn handle_connection(stream: TcpStream) -> Result<(), DeviceServerError> {
    let addr = stream
        .peer_addr()
        .map_err(|_| DeviceServerError::PeerAddrError)?;

    // create a WebSocket connection
    let try_web_socket_stream = tokio_tungstenite::accept_async(stream).await;
    let web_socket_stream =
        try_web_socket_stream.map_err(|_| DeviceServerError::WebSocketHandshakeError)?;

    println!("New WebSocket connection created: {}", addr);

    // separate ownership between receiving and writing part
    let (_write, _read) = web_socket_stream.split();

    // 0. decomment imports & type Command & remove _
    // 1. read the received command
    // 2. convert it from a Vec<u8> into a OsString --> OsStr::from_bytes(&Vec<u8>).to_owned()
    // 3. send the output to the client (either Ok or ShellError)
    // 4. IMPLEMENT CLIENT

    /*
    read.try_filter(|msg| future::ready(msg.is_text() || msg.is_binary()))
        .forward(write)
        .await
        .expect("Failed to forward messages")
     */

    Ok(())
}
