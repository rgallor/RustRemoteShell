//! Service responsible for handling [Host](HostMsg) commands.
//!
//! This module contains the [`tower`] service [`CommandService`] responsible for
//! * handling stdin/stdout operations,
//! * handling CTRL C and CTRL D signals,
//! * closing an openned WebSocket connection with a device.

use std::{sync::Arc, task::Poll};

use futures::{future::LocalBoxFuture, FutureExt, SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, Stdout},
    select,
    sync::Notify,
};
use tower::Service;
use tracing::{debug, error, info, instrument, warn};

use crate::{
    host::HostError,
    protocol::{DeviceMsg, HostMsg},
    stream::MessageHandler,
};

/// Service responsible for handling [Host](HostMsg) commands.
#[derive(Debug)]
pub struct CommandService<S> {
    msg_handler: MessageHandler<S>,
    ctrl_c: Arc<Notify>,
    stdout: Stdout,
}

impl<S> Clone for CommandService<S> {
    fn clone(&self) -> Self {
        Self {
            msg_handler: self.msg_handler.clone(),
            ctrl_c: self.ctrl_c.clone(),
            stdout: tokio::io::stdout(),
        }
    }
}

impl<S> Service<String> for CommandService<S>
where
    S: AsyncRead + AsyncWrite + Unpin + 'static,
{
    type Response = ();
    type Error = HostError;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    /// call the command handler
    fn call(&mut self, req: String) -> Self::Future {
        let handler = self.clone();
        async move { handler.handle_command(req).await }.boxed_local()
    }
}

impl<S> CommandService<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    #[instrument(skip_all)]
    pub(crate) fn new(msg_handler: MessageHandler<S>, ctrl_c: Arc<Notify>) -> Self {
        Self {
            msg_handler,
            ctrl_c,
            stdout: tokio::io::stdout(),
        }
    }

    pub(crate) async fn close(&self) -> Result<(), HostError> {
        debug!("closing stream");
        self.msg_handler.close().await
    }

    /// Command execution handler.
    #[instrument(skip(self))]
    async fn handle_command(self, cmd: String) -> Result<(), HostError> {
        match self.impl_handle_command(cmd).await {
            Ok(()) => Ok(()),
            Err(err) => match err.handle_close() {
                Ok(msg) => {
                    info!("{}", msg);
                    Ok(())
                }
                Err(err @ HostError::Exit) => {
                    info!("Closing connection");
                    Err(err)
                }
                Err(err) => {
                    error!("Fatal error: {}", err);
                    eprintln!("{:#?}", err);
                    Err(err)
                }
            },
        }
    }

    #[instrument(skip_all)]
    async fn impl_handle_command(mut self, cmd: String) -> Result<(), HostError> {
        // Steps:
        // 1. send command to device
        // 2. loop
        //      select
        //          cmd_out -> stdout (no close connection)
        //          ctrl_c -> send CTRL C to device + exit

        // check exit condition
        if cmd.trim() == "exit" {
            // termination condition when calling this method in a loop
            debug!("command \"exit\" received");
            self.close().await?;
            return Err(HostError::Exit);
        }

        let msg = HostMsg::Command { cmd };

        // send command to the device
        self.msg_handler
            .sink()
            .await
            .send(msg.try_into()?)
            .await
            .map_err(HostError::TungsteniteReadData)?;

        loop {
            select! {
                _ = self.ctrl_c.notified() => {
                    self.handle_ctrl_c_notify().await?;
                    return Ok(());
                }

                msg = self.msg_handler.next() => {
                    let stop_loop = self.handle_incoming_msg(msg).await?;
                    if stop_loop {
                        return Ok(());
                    }
                }
            }
        }
    }

    /// Function called when a Notify of a CTRL C event has been received. It tries to terminate the command execution.
    async fn handle_ctrl_c_notify(&mut self) -> Result<(), HostError> {
        info!("CTRL C received. Terminate command execution.");

        // send CTRL C to the device
        let msg = HostMsg::Ctrlc.try_into()?;

        // Take the sink part of the websocket connection and send CTRL C message.
        if let Err(err) = self.msg_handler.sink().await.send(msg).await {
            error!("Error sending CTRL C Message to device: {}", err);
        }

        // print out the already received commands output
        while let Some(msg) = self.msg_handler.next().await {
            // in case an EoF or a ShelError is received, break the loop
            let stop_loop = !self.handle_msg_variants(msg).await?;

            if stop_loop {
                break;
            }
        }

        Ok(())
    }

    /// This function takes a received DeviceMsg and handle it, returning true in case the
    /// host should stop looping because no other messages should arrive.
    async fn handle_incoming_msg(
        &mut self,
        msg: Option<Result<DeviceMsg, HostError>>,
    ) -> Result<bool, HostError> {
        match msg {
            Some(msg) => {
                // in case an EoF or a ShelError is received, break the loop
                let stop_loop = !self.handle_msg_variants(msg).await?;
                if stop_loop {
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            None => {
                error!("Stream teerminated due to unexpected EOF");
                Err(HostError::UnexpectedEof)
            }
        }
    }

    /// Given a DeviceMsg, it checks its variants and return a bool stating if the calling function
    /// should continue looping (true) or break (false)
    async fn handle_msg_variants(
        &mut self,
        msg: Result<DeviceMsg, HostError>,
    ) -> Result<bool, HostError> {
        let msg = msg?;

        match msg {
            DeviceMsg::Eof => {
                debug!("EOF received.");
                Ok(false)
            }
            DeviceMsg::Err(err) => {
                error!("Skipping shell error: {}", err);
                Ok(false)
            }
            DeviceMsg::Output(out) => {
                debug!("Received {} bytes", out.len());
                self.write_stdout(out).await?;
                Ok(true)
            }
        }
    }

    /// Write the command output to the stdout.
    #[instrument(skip_all)]
    pub(crate) async fn write_stdout(&mut self, cmd_out: Vec<u8>) -> Result<(), HostError> {
        // check if there are command outputs stored in the channel. Eventually, print them to the stdout
        self.stdout
            .write(&cmd_out)
            .await
            .map_err(HostError::IOWrite)?;

        self.stdout.flush().await.map_err(HostError::IOWrite)?;

        Ok(())
    }
}
