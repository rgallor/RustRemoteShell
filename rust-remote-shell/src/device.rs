//! Device implementation.
//!
//! This module provides a [`Device`] struct necessary for the creation of a new device and
//! to connect through WebSocket connection with a host.
//! Thanks to the interaction with the [astarte](crate::astarte) module,
//! the device will first try to connect with the Astarte server and then with a host
//! (reachable from the URL received) through the astarte server - astarte device communication.
//!
//! The module also provides the [`start`](Device::start) function, responsible for handling
//! the communication between the device and a host.

use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use astarte_device_sdk::AstarteDeviceSdk;
use futures::stream::SplitSink;
use futures::{future, SinkExt, StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use tokio::net::TcpStream;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tokio::task::{JoinError, JoinSet};
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::{tungstenite::Error as TungError, tungstenite::Message, WebSocketStream};
use tokio_util::io::ReaderStream;
use tracing::{debug, error, info, instrument, warn};
use url::Url;

use crate::astarte::{Error as AstarteError, HandleAstarteConnection};
use crate::protocol::{DeviceMsg, HostMsg};
use crate::shell::{CommandHandler, ShellError};

#[cfg(feature = "tls")]
use crate::tls::{self, Error as TlsError};

/// Device errors.
#[derive(Error, Debug)]
pub enum DeviceError {
    ///Trasport error from Tungstenite.
    #[error("Trasport error from Tungstenite.")]
    Transport(#[from] TungError),

    /// Error while trying to connect with the host.
    #[error("Error while trying to connect with the host.")]
    WebSocketConnect(#[source] TungError),

    /// Wrong scheme received.
    #[error("Wrong scheme, {0}.")]
    WrongScheme(String),

    /// Error while reading stdout from child process executing the shell command.
    #[error("Error while reading from child process's stdout.")]
    ChildStdout(#[source] std::io::Error),

    /// Error while sending an URL across tasks.
    #[error("Error while sending an URL across tasks.")]
    ChannelUrl(#[from] SendError<Url>),

    /// Error while sending HostMsg over channel.
    #[error("Error while sending HostMsg over channel.")]
    ChannelMsg(#[from] SendError<HostMsg>),

    /// Error while joining a tokio task.
    #[error("Error while joining a tokio task.")]
    Join(#[from] JoinError),

    /// [`AstarteError`] error.
    #[error("Astarte error, {}.", msg)]
    Astarte {
        /// Sourece error.
        #[source]
        err: AstarteError,
        /// Error message
        msg: String,
    },

    /// [`ShellError`] error.
    #[error("Shell error.")]
    Shell(#[from] ShellError),

    /// Wrong WebSocket message type.
    #[error("Wrong WebSocket message type.")]
    WrongWsMessage(tokio_tungstenite::tungstenite::Message),

    /// Couldn't deserialize from BSON.
    #[error("Couldn't deserialize from BSON.")]
    Deserialize(#[from] bson::de::Error),

    /// Couldn't serialize to BSON.
    #[error("Couldn't serialize to BSON.")]
    Serialize(#[from] bson::ser::Error),

    /// TLS error
    #[cfg(feature = "tls")]
    #[error("Wrong item.")]
    Tls(#[from] TlsError),
}

/// Device struct.
///
/// The device will attempt to connect with Astarte. When an Astarte datastream aggregate Object
/// is received, the device retrieves the information and build an URL, which will subsequently use to
/// establish a connection with a host.
/// It can also add TLS configuration information to establish a secure WebSocket connection over TLS.
#[derive(Clone, Debug)]
pub struct Device {
    astarte_device: AstarteDeviceSdk,
    ca_cert_file: Option<PathBuf>,
}

impl Device {
    /// Configure an Astarte device and wait for an Astarte event. Then retrieve an URL from the event
    /// so to open a WebSocket connection with a host.
    #[instrument(skip_all)]
    pub async fn new(device_cfg_path: &str) -> Result<Self, DeviceError> {
        let handle_astarte = HandleAstarteConnection;

        let cfg = handle_astarte
            .read_device_config(device_cfg_path)
            .await
            .map_err(|err| DeviceError::Astarte {
                err,
                msg: "wrong device config".to_string(),
            })?;

        let astarte_device = handle_astarte
            .create_astarte_device(&cfg)
            .await
            .map_err(|err| DeviceError::Astarte {
                err,
                msg: "failed to create the astarte device".to_string(),
            })?;

        info!("Connection to Astarte established.");

        Ok(Self {
            astarte_device,
            ca_cert_file: None,
        })
    }

    /// Provide CA cert file to the device in order to open a TLS connection with a host.
    #[cfg(feature = "tls")]
    pub fn with_tls(&mut self, ca_cert_file: Option<PathBuf>) {
        self.ca_cert_file = ca_cert_file.or_else(|| panic!("No CA cert file has been provided"));
    }

    /// Start the device.
    ///
    /// A device that is started will try to connect with hosts after having received from Astarte the host URL.
    #[instrument(skip_all)]
    pub async fn start(self) -> Result<(), DeviceError> {
        debug!("spawning a task to handle new connections");
        let (tx_url, rx_url) = unbounded_channel::<Url>();

        // define a task to handle astarte events and send the received URL to the task responsible for the creation of new connections
        let handle_astarte =
            tokio::spawn(Device::handle_astarte_events(self.astarte_device, tx_url));

        // define a task to handle new connections after having received an URL from the task respponsible for handling astarte events
        Device::handle_connections(rx_url, self.ca_cert_file).await;

        handle_astarte.await?
    }

    /// Handle a single Astarte event.
    async fn handle_astarte_events(
        mut astarte_device: AstarteDeviceSdk,
        tx_url: UnboundedSender<Url>,
    ) -> Result<(), DeviceError> {
        loop {
            // Wait for an aggregate datastream containing a scheme, an IP and a port number to connect to
            match astarte_device.handle_events().await {
                Ok(data) => {
                    match data.data {
                        astarte_device_sdk::Aggregation::Object(map) if data.path == "/rshell" => {
                            let url =
                                HandleAstarteConnection::retrieve_url(&map).map_err(|err| {
                                    DeviceError::Astarte {
                                        err,
                                        msg: "failed to retrieve the url".to_string(),
                                    }
                                })?;
                            info!("received url: {}", url);

                            // send the retrieved URL to the task responsible for the creation of a new connection
                            tx_url.send(url)?;
                        }
                        _ => {
                            // when an error is returned, tx_url is dropped. This is fundamental because the task
                            // responsible for handling the connections will stop spawning other connections
                            // after having received None from the channel
                            return Err(DeviceError::Astarte {
                                err: AstarteError::AstarteWrongAggregation,
                                msg: "received wrong astarte type".to_string(),
                            });
                        }
                    }
                }
                // If the device get disconnected from Astarte, it will loop untill it is reconnected
                Err(err @ astarte_device_sdk::AstarteError::ConnectionError(_)) => {
                    warn!("Connection err  {:#?}", err);
                }
                Err(err) => {
                    error!("Astarte error: {:?}", err);
                    return Err(DeviceError::Astarte {
                        err: AstarteError::AstarteHandleEvent(err),
                        msg: "failed to handle astarte event".to_string(),
                    });
                }
            }
        }
    }

    /// Connect to a host by using a TCP/TLS connection.
    async fn handle_connections(mut rx_url: UnboundedReceiver<Url>, ca_cert_file: Option<PathBuf>) {
        // define a set of tasks, each responsible for handling a connection with a host
        let mut handles = JoinSet::new();

        // when the channel is dropped, that is when the astarte task fails, the rx_url.recv() statement
        // returns None and the while loop terminates
        debug!("waiting to receive an URL");
        while let Some(url) = rx_url.recv().await {
            handles.spawn(Device::handle_connection(url, ca_cert_file.clone()));
        }

        // await the other connections to terminate before returning
        // Note: errors are not propagated to avoid aborting the exisitng connections
        while let Some(res) = handles.join_next().await {
            match res {
                Ok(Ok(())) => {
                    info!("connection with host closed");
                }
                Err(err) => {
                    error!("Join handle error: {}", err);
                }
                Ok(Err(err)) => {
                    error!("device error, {:#?}", err);
                }
            }
        }
    }

    /// Function responsible for handling the communication between the device and a host.
    ///
    /// The device listens for commands, executes them and sends the output to the host
    async fn handle_connection(url: Url, ca_cert_file: Option<PathBuf>) -> Result<(), DeviceError> {
        let ws_stream = Self::create_ws_stream(url, ca_cert_file).await?;

        // separate ownership between receiving and writing part
        let (write, read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));

        // map the Websocket stream of Messages into a stream of HostMsg
        let mut stream = read
            .map_err(DeviceError::Transport)
            .and_then(|msg| future::ready(HostMsg::try_from(msg)));

        // define the command handler
        let cmd_handler = Arc::new(Mutex::new(CommandHandler::default()));

        // channel used to pass HostMsg between tasks
        let (tx_host_msg, rx_host_msg) = unbounded_channel::<HostMsg>();

        // define atomicbool to stop sending data when a CTRL C is received
        let ctrl_c_atomic = Arc::new(AtomicBool::new(false));

        let ctrl_c_atomic_clone = Arc::clone(&ctrl_c_atomic);
        let write_clone = Arc::clone(&write);
        let cmd_handler_clone = Arc::clone(&cmd_handler);

        // task responsible for handling HostMsg
        let handle = tokio::spawn(Self::handle_host_msgs(
            rx_host_msg,
            write_clone,
            cmd_handler_clone,
            ctrl_c_atomic_clone,
        ));

        // the main task is responsible for retrieving incoming Messages, converting them into HostMsg,
        // handling eventual CTRL C events and sending HostMsg to the task responsible for their handling
        while let Some(res) = stream.next().await {
            let host_msg = match res {
                Ok(msg) => msg,
                Err(err) => return Err(err),
            };

            // if CTRL C is received, stop the currently executed command
            if let HostMsg::Ctrlc = host_msg {
                match cmd_handler.lock().await.ctrl_c().await {
                    Ok(()) => {
                        info!("Command execution interrupted");
                        ctrl_c_atomic.store(true, Ordering::Relaxed);
                        continue; // wait for the next HostMsg
                    }
                    Err(ShellError::NoCommand) => {
                        warn!("Received CTRL C without executing any command, closing connection with host");
                        Self::close_connection(&write).await?;
                        handle.abort();
                        break;
                    }
                    Err(err) => {
                        error!("error while processing CTRL C, {:#?}", err);
                        Self::close_connection(&write).await?;
                        handle.abort();
                        break;
                    }
                }
            }

            // set atomic_bool to false if it is true, so that another CTRL C can be handled for successive commands
            if ctrl_c_atomic.load(Ordering::Relaxed) {
                ctrl_c_atomic.store(false, Ordering::Relaxed);
            }

            // queue the HostMsg so that the task responsible for HostMsg handling can proceed
            tx_host_msg.send(host_msg)?;
        }

        match handle.await {
            Ok(Ok(())) => debug!("task terminated"),
            Err(err) if err.is_cancelled() => {
                debug!("task terminated with CTRL C")
            }
            Err(err) => {
                error!("Join handle error: {}", err);
            }
            Ok(Err(err)) => {
                error!("device error, {:#?}", err);
                return Err(err);
            }
        }

        Ok(())
    }

    async fn create_ws_stream(
        url: Url,
        ca_cert_file: Option<PathBuf>,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, DeviceError> {
        // in case of a TLS connection, it is possible to receive either "ws" or "wss" as acceptable url scheme
        let ws_stream = match url.scheme() {
            #[cfg(feature = "tls")]
            "wss" => {
                let connector = tls::device_tls_config(ca_cert_file)?; // Connector::Rustls
                tls::connect(&url, Some(connector)).await?
            }
            "ws" => {
                let (ws_stream, _) =
                    tokio_tungstenite::connect_async(&url)
                        .await
                        .map_err(|err| {
                            error!("Websocket error: {:?}", err);
                            DeviceError::WebSocketConnect(err)
                        })?;
                ws_stream
            }
            scheme => {
                return Err(DeviceError::WrongScheme(scheme.to_string()));
            }
        };

        Ok(ws_stream)
    }

    /// This function iterates over a channel of [`HostMsg`] and executes each of them,
    /// interrupting the execution in case a CTRL C signal is received. Then it sends
    /// the command output through the WebSocket connection to the host.
    #[instrument(skip_all)]
    async fn handle_host_msgs<U>(
        mut rx_host_msg: UnboundedReceiver<HostMsg>,
        write: Arc<Mutex<SplitSink<WebSocketStream<U>, Message>>>,
        cmd_handler: Arc<Mutex<CommandHandler>>,
        ctrl_c_atomic: Arc<AtomicBool>,
    ) -> Result<(), DeviceError>
    where
        U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        while let Some(cmd) = rx_host_msg.recv().await {
            let cmd = match cmd {
                HostMsg::Command { cmd } => cmd,
                HostMsg::Exit => {
                    info!("closing connection with host");
                    write.lock().await.close().await?;
                    return Ok::<(), DeviceError>(());
                }
                HostMsg::Ctrlc => {
                    unreachable!("should have been already handled in handle_stream task")
                }
            };

            let res = cmd_handler
                .lock()
                .await
                .execute(cmd)
                .map_err(DeviceError::Shell);

            let child_stdout = match res {
                Ok(child_stdout) => child_stdout,
                Err(DeviceError::Shell(err)) => {
                    debug!("shell error");
                    write
                        .lock()
                        .await
                        .send(DeviceMsg::Err(err.to_string()).try_into()?)
                        .await?;

                    continue;
                }
                _ => {
                    todo!("TODO: check if there are crytical error that must interrupt the application")
                }
            };

            // future used to terminate the stream in case a CTRL C is received
            let stop_fut = future::poll_fn(|_cx| {
                if ctrl_c_atomic.load(Ordering::Relaxed) {
                    // set the atomic bool to false again (because the stream is being terminated)
                    ctrl_c_atomic.store(false, Ordering::Relaxed);
                    return Poll::Ready(());
                }
                Poll::Pending
            });

            // command output stream
            let mut stream = ReaderStream::with_capacity(child_stdout, 1024)
                .map_err(DeviceError::ChildStdout)
                .and_then(|buf| {
                    let msg = DeviceMsg::Output(buf.to_vec());
                    let res: Result<Message, DeviceError> = msg.try_into();
                    match res {
                        Ok(msg) => future::ok(msg),
                        Err(err) => future::err(err),
                    }
                })
                .take_until(stop_fut);

            while let Some(msg) = stream.next().await {
                let msg = msg?;
                write.lock().await.send(msg).await?;
            }

            // the command has finished being executed, therefore the child process stored insidet che command_handler can be set to None
            cmd_handler.lock().await.empty();

            // send EoF
            debug!("Sending EOF.");
            write
                .lock()
                .await
                .send(Message::Binary(bson::to_vec(&DeviceMsg::Eof)?))
                .await?;
        }

        Ok(())
    }

    /// Function used to close the WebSocket connection in case of errors.
    #[instrument(skip_all)]
    async fn close_connection<U>(
        write: &Mutex<SplitSink<WebSocketStream<U>, Message>>,
    ) -> Result<(), DeviceError>
    where
        U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut write = write.lock().await;
        write.send(Message::Close(None)).await?;

        debug!("sent close message");

        write.close().await?;

        debug!("channel closed");

        Ok(())
    }
}
