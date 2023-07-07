//! Helper to handle I/O functionality.
//!
//! This module contains the helper struct [`IoHandler`] responsible for
//! * handling stdin/stdout operations,
//! * sending command output to the [Host](crate::host) waiting for a response,
//! * sending errors through a separate channel to the tokio task responsible for error handling in the [Host](crate::host) module,
//! * closing the WebSocket connection between the device and the host.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdin, Stdout};
use tracing::{instrument, warn};

use crate::host::HostError;

/// I/O helper struct.
#[derive(Debug)]
pub struct IoHandler {
    stdout: Stdout,
    reader: BufReader<Stdin>,
    buf: String,
}

impl Default for IoHandler {
    fn default() -> Self {
        Self {
            stdout: tokio::io::stdout(),
            reader: BufReader::new(tokio::io::stdin()),
            buf: String::new(),
        }
    }
}

impl IoHandler {
    /// Stdin content.
    pub fn buf(&self) -> &str {
        &self.buf
    }

    /// Read the command from the stdin and send it to the device.
    #[instrument(skip_all)]
    pub async fn read_stdin(&mut self) -> Result<usize, HostError> {
        // empty the buffer so to be able to store incoming command
        self.buf.clear();

        let byte_read = self
            .reader
            .read_line(&mut self.buf)
            .await
            .map_err(HostError::IORead)?;

        Ok(byte_read)
    }

    /// Termination condition in case the host sent "exit".
    #[instrument(skip_all)]
    pub fn exited(&self) -> bool {
        matches!(self.buf.split_ascii_whitespace().next(), Some(cmd) if cmd == "exit")
    }

    /// Write the command output to the stdout.
    #[instrument(skip_all)]
    pub async fn write_stdout(&mut self, cmd_out: Vec<u8>) -> Result<(), HostError> {
        // check if there are command outputs stored in the channel. Eventually, print them to the stdout
        self.stdout
            .write(&cmd_out)
            .await
            .map_err(HostError::IOWrite)?;

        self.stdout.flush().await.map_err(HostError::IOWrite)?;

        Ok(())
    }
}
