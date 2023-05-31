use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::{Certificate, ClientConfig, PrivateKey, RootCertStore, ServerConfig};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, instrument};
use url::Url;

use crate::device_server::{Connect, ServerError, TcpConnector};
use crate::sender_client::{ClientConnect, ClientError, TcpClientConnector};

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
    async fn bind(&mut self, addr: SocketAddr) -> Result<TcpListener, ServerError> {
        self.listener.bind(addr).await
    }

    async fn connect(
        &mut self,
        listener: &mut TcpListener,
    ) -> Result<TlsStream<TcpStream>, ServerError> {
        let stream = self.listener.connect(listener).await?;
        self.acceptor
            .accept(stream)
            .await
            .map_err(|err| ServerError::AcceptConnection { err })
    }
}

#[instrument(skip_all)]
pub async fn server_tls_config(
    cert: Vec<u8>,
    privkey: Vec<u8>,
) -> Result<tokio_rustls::rustls::ServerConfig, ServerError> {
    let certs = vec![Certificate(cert)];

    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, PrivateKey(privkey))
        .map_err(ServerError::RustTls)?;

    debug!("config created: {:?}", config);

    Ok(config)
}

// configuration options for TLS connection
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
    async fn connect(&mut self) -> Result<(), ClientError> {
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
            ClientError::WebSocketConnect(err)
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
