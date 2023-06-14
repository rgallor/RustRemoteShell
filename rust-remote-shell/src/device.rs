use std::{fmt::Debug, future};

use futures::{SinkExt, StreamExt, TryStreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tower::Service;
use url::Url;

#[cfg(feature = "tls")]
use crate::tls;
use crate::{device_server::ServerError, shell::CommandHandler};

#[derive(Clone, Debug)]
pub struct Device {
    url: Url,
}

impl Device {
    pub fn new(url: Url) -> Self {
        Self { url }
    }

    pub async fn connect<S, E, C>(&mut self, ca_cert: Option<C>, mut service: S)
    where
        S: Service<WebSocketStream<MaybeTlsStream<TcpStream>>, Error = E> + Clone + 'static,
        E: Debug,
        C: Into<Vec<u8>>,
    {
        let ws_stream = match ca_cert {
            #[cfg(feature = "tls")]
            Some(ca_cert) => {
                let connector = tls::client_tls_config(ca_cert.into()).await; // Connector::Rustls

                tls::connect(&self.url, Some(connector)).await.expect("err")
            }
            _ => connect_async(&self.url).await.expect("err").0,
        };

        service.call(ws_stream).await.expect("err");
    }
}

// the device listen for commands, execute them and send the output to the host
pub async fn device_handle<U>(stream: WebSocketStream<U>) -> Result<(), ()>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (write, read) = stream.split();

    let _ = read
        .map_err(ServerError::Transport)
        .and_then(|msg| {
            let cmd = match msg {
                // convert the message from a Vec<u8> into a OsString
                Message::Binary(v) => String::from_utf8(v).map_err(ServerError::Utf8Error),
                Message::Close(_) => Err(ServerError::CloseWebsocket), // the client closed the connection
                _ => Err(ServerError::ReadCommand),
            };

            future::ready(cmd)
        })
        .and_then(|cmd| async move {
            // define a command handler
            let cmd_handler = CommandHandler::new();

            // execute the command and eventually return the error
            let cmd_out = cmd_handler
                .execute(cmd)
                .await
                .unwrap_or_else(|err| format!("Shell error: {}\n", err));

            Ok(Message::Binary(cmd_out.as_bytes().to_vec()))
        })
        .forward(write.sink_map_err(ServerError::Transport))
        .await;

    Ok(())
}
