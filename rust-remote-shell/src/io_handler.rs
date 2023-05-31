use futures::stream::SplitSink;
use futures::SinkExt;
use tokio::io::{AsyncBufReadExt, BufReader, Stdin, Stdout};
use tokio::net::TcpStream;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio::sync::MutexGuard;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, instrument, warn};

use crate::sender_client::ClientError;

#[derive(Debug)]
pub struct IOHandler {
    stdout: Stdout,
    reader: BufReader<Stdin>,
    write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    tx_err: Sender<Result<(), ClientError>>,
    buf_cmd: String,
    exited: bool,
}

impl IOHandler {
    pub fn new(
        write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        tx_err: Sender<Result<(), ClientError>>,
    ) -> Self {
        Self {
            stdout: tokio::io::stdout(),
            reader: BufReader::new(tokio::io::stdin()),
            write,
            tx_err,
            buf_cmd: String::new(),
            exited: false,
        }
    }

    #[instrument(skip_all)]
    pub async fn read_stdin(&mut self) -> Result<(), ClientError> {
        // empty the buffer so to be able to store incoming command
        self.buf_cmd.clear();

        // read a shell command from the stdin and send it to the server
        let byte_read = self
            .reader
            .read_line(&mut self.buf_cmd)
            .await
            .map_err(ClientError::IORead)?;

        debug!(?byte_read);
        if byte_read == 0 {
            info!("EOF received");
            self.exit().await?;
        } else if self.check_exit() {
            info!("exit received");
            self.exit().await?;
        }

        Ok(())
    }

    #[instrument(skip_all)]
    fn check_exit(&self) -> bool {
        matches!(self.buf_cmd.split_ascii_whitespace().next(), Some(cmd) if cmd == "exit")
    }

    #[instrument(skip_all)]
    async fn exit(&mut self) -> Result<(), ClientError> {
        // close the connection
        self.write
            .send(Message::Close(None))
            .await
            .map_err(|err| ClientError::TungsteniteClose { err })?;
        info!("Closed websocket on client side");

        self.tx_err.send(Ok(())).await.expect("channel error");

        self.exited = true;
        Ok(())
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    #[instrument(skip_all)]
    pub async fn send_to_server(&mut self) -> Result<(), ClientError> {
        info!("Send command to the server");
        self.write
            .send(Message::Binary(self.buf_cmd.as_bytes().to_vec()))
            .await
            .map_err(|err| ClientError::TungsteniteReadData { err })?;

        info!("Command sent: {}", self.buf_cmd);

        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn write_stdout(
        &mut self,
        rx: &Mutex<UnboundedReceiver<Message>>,
    ) -> Result<(), ClientError> {
        // check if there are command outputs stored in the channel. Eventually, print them to the stdout
        let mut channel = rx.lock().await;

        // wait to receive the first command output
        let msg = channel.recv().await.unwrap();

        self.impl_write_stdout(msg).await?;

        // if the channel still contains information, empty it before aborting the task
        self.empty_buffer(channel).await?;

        Ok(())
    }

    #[instrument(skip_all)]
    async fn impl_write_stdout(&mut self, msg: Message) -> Result<(), ClientError> {
        let data = msg.into_data();

        self.stdout
            .write(&data)
            .await
            .map_err(|err| ClientError::IOWrite { err })?;

        self.stdout
            .flush()
            .await
            .map_err(|err| ClientError::IOWrite { err })?;

        Ok(())
    }

    async fn empty_buffer(
        &mut self,
        mut channel: MutexGuard<'_, UnboundedReceiver<Message>>,
    ) -> Result<(), ClientError> {
        loop {
            match channel.try_recv() {
                Ok(msg) => {
                    self.impl_write_stdout(msg).await?;
                }
                Err(TryRecvError::Empty) => {
                    // the channel is empty but the connection is still open
                    break Ok(());
                }
                Err(TryRecvError::Disconnected) => {
                    unreachable!("the channel should not be dropped before the task is aborted")
                }
            }
        }
    }
}
