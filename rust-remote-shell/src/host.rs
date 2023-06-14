use std::fmt::Debug;
use std::net::SocketAddr;

use futures::{SinkExt, StreamExt, TryStreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tower::layer::util::Stack;
use tower::{layer::util::Identity, Service, ServiceBuilder};

use crate::service::WebSocketLayer;

#[cfg(feature = "tls")]
use crate::tls::{self, TlsLayer};

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

// the host sends commands, wait for device's answer and display it
pub async fn host_handle<U>(stream: WebSocketStream<U>) -> Result<(), ()>
where
    U: AsyncRead + AsyncWrite + Unpin,
{
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
