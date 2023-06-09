use std::fmt::Debug;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

use futures::{future, SinkExt, StreamExt, TryStreamExt};

use tokio_tungstenite::{connect_async, tungstenite::Message};
pub use tower;

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::{accept_async, MaybeTlsStream, WebSocketStream};
use tower::layer::util::Identity;
use tower::layer::util::Stack;
use tower::{Layer, Service, ServiceBuilder};
use url::Url;

use crate::device_server::ServerError;
use crate::shell::CommandHandler;

// Avoid importing tls module if TLS is not enabled
#[cfg(feature = "tls")]
use crate::tls::{self, TlsLayer};

#[derive(Clone)]
pub struct WebSocketService<S> {
    service: S,
}

impl<S, Request> Service<Request> for WebSocketService<S>
where
    S: Service<WebSocketStream<Request>> + Clone + 'static,
    Request: AsyncWrite + AsyncRead + Unpin + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>>>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx) // ready when the wrapping service is ready
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let clone = self.service.clone();
        // take the service that was ready
        let mut inner = std::mem::replace(&mut self.service, clone);

        Box::pin(async move {
            let ws_stream = accept_async(req).await.expect("err");
            inner.call(ws_stream).await
        })
    }
}

pub struct WebSocketLayer;

impl<S> Layer<S> for WebSocketLayer {
    type Service = WebSocketService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        WebSocketService { service: inner }
    }
}

pub struct HostBuilder<L> {
    server: Host,
    builder: ServiceBuilder<L>,
}

impl<L> HostBuilder<L> {
    pub fn fields(self) -> (Host, ServiceBuilder<L>) {
        (self.server, self.builder)
    }
}

impl HostBuilder<Identity> {
    #[cfg(feature = "tls")]
    pub async fn with_tls<C, P>(self, cert: C, privkey: P) -> HostBuilder<Stack<TlsLayer, Identity>>
    where
        C: Into<Vec<u8>>,
        P: Into<Vec<u8>>,
    {
        let acceptor = tls::acceptor(cert, privkey).await.expect("err");

        HostBuilder {
            server: self.server,
            builder: self.builder.layer(TlsLayer::new(acceptor)),
        }
    }

    pub async fn serve<S, E>(mut self, service: S)
    where
        S: Service<WebSocketStream<TcpStream>, Error = E> + Clone + 'static,
        E: Debug,
    {
        let service = self.builder.layer(WebSocketLayer).service(service);

        self.server.listen(service).await;
    }
}

pub struct Host {
    listener: TcpListener,
}

impl Host {
    pub async fn bind(addr: SocketAddr) -> HostBuilder<Identity> {
        let listener = TcpListener::bind(addr).await.expect("err");
        let server = Self { listener };

        HostBuilder {
            server,
            builder: ServiceBuilder::new(),
        }
    }

    pub async fn listen<S, E>(&mut self, mut service: S)
    where
        S: Service<TcpStream, Error = E> + Clone + 'static,
        E: Debug,
    {
        loop {
            // wait for a connection from a device
            let (stream, _) = self.listener.accept().await.expect("err");
            // handle the connection by sending messages and printing out responses from the device
            service.call(stream).await.expect("err");
        }
    }
}

pub async fn host_handle<U>(stream: WebSocketStream<U>) -> Result<(), ()>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    // TODO: the host should send commands, wait for device's answer and display the reply
    let (mut write, read) = stream.split();

    write
        .send(Message::Binary("ls".as_bytes().to_vec()))
        .await
        .expect("err");

    let _ = read
        .try_for_each(|cmd_out| async move {
            println!("OUTPUT: {}", cmd_out);
            Ok(())
        })
        .await;

    Ok(())
}

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
        let ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>> = match ca_cert {
            #[cfg(feature = "tls")]
            Some(ca_cert) => {
                let connector = tls::client_tls_config(ca_cert.into()).await; // Connector::Rustls

                tls::connect(self.url.clone(), Some(connector))
                    .await
                    .expect("err")
            }
            _ => connect_async(self.url.clone()).await.expect("err").0,
        };

        service.call(ws_stream).await.expect("err");
    }
}

pub async fn device_handle<U>(stream: WebSocketStream<U>) -> Result<(), ()>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
    // TODO: the device should listen for commands, execute them and send the output to the host
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
