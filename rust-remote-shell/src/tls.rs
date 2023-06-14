use futures::Future;

use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{Certificate, ClientConfig, PrivateKey, RootCertStore};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_tungstenite::{
    connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};
use tower::layer::util::{Identity, Stack};
use tower::{Layer, Service};
use tracing::{debug, instrument};
use url::Url;

use crate::device::DeviceError;
use crate::host::{HostBuilder, HostError};

/*
use async_trait::async_trait;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use crate::sender_client::{ClientConnect, HostError, TcpClientConnector};

pub struct TlsConnector {
    listener: TcpConnector,
    acceptor: TlsAcceptor,
}

impl TlsConnector {
    pub fn new(connector: TcpConnector, tls_config: Arc<ServerConfig>) -> Self {
        let acceptor = TlsAcceptor::from(tls_config);
        Self {
            listener: connector,
            acceptor,
        }
    }
}

#[async_trait]
impl Connect<TlsStream<TcpStream>, TcpListener> for TlsConnector {
    async fn bind(&mut self, addr: SocketAddr) -> Result<TcpListener, DeviceError> {
        self.listener.bind(addr).await
    }

    async fn connect(
        &mut self,
        listener: &mut TcpListener,
    ) -> Result<TlsStream<TcpStream>, DeviceError> {
        let stream = self.listener.connect(listener).await?;
        self.acceptor
            .accept(stream)
            .await
            .map_err(|err| DeviceError::AcceptConnection { err })
    }
}

pub struct TlsClientConnector {
    client: TcpClientConnector,
    tls_connector: Connector,
}

impl TlsClientConnector {
    pub fn new(client: TcpClientConnector, tls_connector: Connector) -> Self {
        Self {
            client,
            tls_connector,
        }
    }

    fn get_listener_url(&self) -> Url {
        self.client.get_listener_url()
    }

    fn get_tls_connector(&self) -> Connector {
        self.tls_connector.clone()
    }

    fn set_ws_stream(&mut self, ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>) {
        self.client.set_ws_stream(ws_stream);
    }
}

// TODO: RETURN THE WEBSOCKET CONNECTION FROM THE connect()
#[async_trait]
impl ClientConnect for TlsClientConnector {
    #[instrument(skip(self))]
    async fn connect(&mut self) -> Result<(), HostError> {
        use tokio_tungstenite::connect_async_tls_with_config;

        // Websocket connection to an existing server
        let (ws_stream, _) = connect_async_tls_with_config(
            self.get_listener_url(),
            None,
            Some(self.get_tls_connector()),
        )
        .await
        .map_err(|err| {
            error!("Websocket error: {:?}", err);
            HostError::WebSocketConnect(err)
        })?;

        info!("WebSocket handshake has been successfully completed on a TLS protected stream");

        self.set_ws_stream(ws_stream);

        Ok(())
    }

    #[instrument(skip(self))]
    fn get_ws_stream(self) -> Option<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        self.client.get_ws_stream()
    }
}

*/

// ----------------------------------------------------------------------------------------------------------------------------------------------------------------

#[instrument(skip_all)]
pub async fn server_tls_config<C, P>(
    cert: C,
    privkey: P,
) -> Result<tokio_rustls::rustls::ServerConfig, DeviceError>
where
    C: Into<Vec<u8>>,
    P: Into<Vec<u8>>,
{
    let certs = vec![Certificate(cert.into())];

    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, PrivateKey(privkey.into()))
        .map_err(DeviceError::RustTls)?;

    debug!("config created: {:?}", config);

    Ok(config)
}

pub async fn acceptor<C, P>(cert: C, privkey: P) -> Result<TlsAcceptor, DeviceError>
where
    C: Into<Vec<u8>>,
    P: Into<Vec<u8>>,
{
    let acceptor = TlsAcceptor::from(Arc::new(server_tls_config(cert, privkey).await?));

    Ok(acceptor)
}

pub async fn client_tls_config(ca_cert: Vec<u8>) -> Connector {
    let mut root_certs = RootCertStore::empty();
    let cert = Certificate(ca_cert);
    root_certs.add(&cert).unwrap();

    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_certs)
        .with_no_client_auth();

    Connector::Rustls(Arc::new(config))
}

pub async fn connect(
    url: &Url,
    connector: Option<Connector>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, HostError> {
    let (ws_stream, _) = connect_async_tls_with_config(url, None, connector)
        .await
        .map_err(HostError::WebSocketConnect)?;

    Ok(ws_stream)
}

#[derive(Clone)]
pub struct TlsService<S> {
    service: S,
    acceptor: TlsAcceptor,
}

impl<S, Request> Service<Request> for TlsService<S>
where
    S: Service<TlsStream<Request>> + Clone + 'static,
    Request: AsyncWrite + AsyncRead + Unpin + 'static,
{
    type Error = S::Error;
    type Response = S::Response;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let clone = self.service.clone();
        // Acceptor is an Arc
        let acceptor = self.acceptor.clone();
        // take the service that was ready
        let mut inner = std::mem::replace(&mut self.service, clone);
        Box::pin(async move {
            let stream = acceptor.accept(req).await.expect("err");
            inner.call(stream).await
        })
    }
}

pub struct TlsLayer {
    acceptor: TlsAcceptor,
}

impl TlsLayer {
    pub fn new(acceptor: TlsAcceptor) -> Self {
        Self { acceptor }
    }
}

impl<S> Layer<S> for TlsLayer {
    type Service = TlsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TlsService {
            service: inner,
            acceptor: self.acceptor.clone(),
        }
    }
}

impl HostBuilder<Stack<TlsLayer, Identity>> {
    #[cfg(feature = "tls")]
    pub async fn serve<S, E>(self, service: S)
    where
        S: Service<WebSocketStream<TlsStream<TcpStream>>, Error = E> + Clone + 'static,
        E: std::fmt::Debug,
    {
        use crate::service::WebSocketLayer;

        let (mut server, builder) = self.fields();
        let service = builder.layer(WebSocketLayer).service(service);

        server.listen(service).await;
    }
}
