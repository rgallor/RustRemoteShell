use futures::stream::SplitSink;
use futures::SinkExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader, Stdin, Stdout};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio::sync::MutexGuard;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, info, instrument, warn};

use crate::host::HostError;

#[derive(Debug)]
pub struct IOHandler<U> {
    stdout: Stdout,
    reader: BufReader<Stdin>,
    write: SplitSink<WebSocketStream<U>, Message>,
    tx_err: Sender<Result<(), HostError>>,
    buf_cmd: String,
    exited: bool,
}

impl<U> IOHandler<U>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(
        write: SplitSink<WebSocketStream<U>, Message>,
        tx_err: Sender<Result<(), HostError>>,
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
    pub async fn read_stdin(&mut self) -> Result<(), HostError> {
        // empty the buffer so to be able to store incoming command
        self.buf_cmd.clear();

        // read a shell command from the stdin and send it to the server
        let byte_read = self
            .reader
            .read_line(&mut self.buf_cmd)
            .await
            .map_err(HostError::IORead)?;

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
    async fn exit(&mut self) -> Result<(), HostError> {
        // close the connection
        self.write
            .send(Message::Close(None))
            .await
            .map_err(HostError::TungsteniteClose)?;
        info!("Closed websocket on client side");

        self.tx_err.send(Ok(())).await.expect("channel error");

        self.exited = true;
        Ok(())
    }

    pub fn is_exited(&self) -> bool {
        self.exited
    }

    #[instrument(skip_all)]
    pub async fn send_to_server(&mut self) -> Result<(), HostError> {
        info!("Send command to the server");
        self.write
            .send(Message::Binary(self.buf_cmd.as_bytes().to_vec()))
            .await
            .map_err(HostError::TungsteniteReadData)?;

        info!("Command sent: {}", self.buf_cmd);

        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn write_stdout(
        &mut self,
        rx: &Mutex<UnboundedReceiver<Message>>,
    ) -> Result<(), HostError> {
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
    async fn impl_write_stdout(&mut self, msg: Message) -> Result<(), HostError> {
        let data = msg.into_data();

        self.stdout.write(&data).await.map_err(HostError::IOWrite)?;

        self.stdout.flush().await.map_err(HostError::IOWrite)?;

        Ok(())
    }

    async fn empty_buffer(
        &mut self,
        mut channel: MutexGuard<'_, UnboundedReceiver<Message>>,
    ) -> Result<(), HostError> {
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
