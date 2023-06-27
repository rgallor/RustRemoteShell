//! TLS functionality.
//!
//! This module provides the necessary functionalities to add a TLS layer on top of the communication between a device and a host.
//! The [`host_tls_config`] and [`acceptor`] functions allow to set the TLS configuration for a
//! host and to provide the necessary TLS connector, necessary to establish a TLS connection with a device.
//! The [`device_tls_config`] and [`connect`] functions allow to set the TLS configuration for a
//! device and to establish a TLS connection with a host.
//!
//! The module also contains the [`TlsService`] and [`TlsLayer`] struct, necessary to implement a Tls layer in the [`tower`] stack.

use futures::Future;

use rustls_pemfile::{read_all, read_one, Item};
use std::io::BufReader;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{
    Certificate, ClientConfig, OwnedTrustAnchor, PrivateKey, RootCertStore,
};
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
use crate::{host::HostService, websocket::WebSocketLayer};

// TODO: add Error (TlsError) + add errorsa from host.rs and device.rs

/// Given the host certificate and private key, return the TLS configuration for the host.
#[instrument(skip_all)]
pub async fn host_tls_config<C, P>(
    cert: C,
    privkey: P,
) -> Result<tokio_rustls::rustls::ServerConfig, HostError>
where
    C: Into<Vec<u8>>,
    P: Into<Vec<u8>>,
{
    let certs = vec![Certificate(cert.into())];

    let config = tokio_rustls::rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, PrivateKey(privkey.into()))
        .map_err(HostError::RustTls)?;

    debug!("config created: {:?}", config);

    Ok(config)
}

/// Given the host certificate and private key files, compute a TLS connection acceptor.
pub async fn acceptor(
    host_cert_file: PathBuf,
    privkey_file: PathBuf,
) -> Result<TlsAcceptor, HostError> {
    let cert_item = retrieve_item(&host_cert_file).map_err(|err| HostError::ReadFile {
        err,
        file: host_cert_file,
    })?;
    let cert = match cert_item {
        Some(Item::X509Certificate(ca_cert)) => ca_cert,
        _ => return Err(HostError::WrongItem),
    };

    let privkey_item = retrieve_item(&privkey_file).map_err(|err| HostError::ReadFile {
        err,
        file: privkey_file,
    })?;
    let privkey = match privkey_item {
        Some(Item::PKCS8Key(privkey)) => privkey,
        _ => return Err(HostError::WrongItem),
    };

    let acceptor = TlsAcceptor::from(Arc::new(host_tls_config(cert, privkey).await?));
    Ok(acceptor)
}

fn retrieve_item(file: &Path) -> Result<Option<Item>, std::io::Error> {
    std::fs::File::open(file)
        .map(BufReader::new)
        .and_then(|mut reader| read_one(&mut reader))
}

/// Given the CA certificate, compute the device TLS configuration and return a Device connector.
pub async fn device_tls_config(ca_cert_file: Option<PathBuf>) -> Result<Connector, DeviceError> {
    let mut root_certs = RootCertStore::empty();

    if let Some(ca_cert_file) = ca_cert_file {
        let file = std::fs::File::open(ca_cert_file).map_err(DeviceError::ReadFile)?;
        let mut reader = BufReader::new(file);

        for item in read_all(&mut reader).map_err(DeviceError::ReadFile)? {
            match item {
                Item::X509Certificate(ca_cert) => {
                    let cert = Certificate(ca_cert);
                    debug!("{:?}", cert);
                    root_certs
                        .add(&cert)
                        .expect("failed to add CA cert to the root certs");
                }
                _ => return Err(DeviceError::WrongItem),
            }
        }
    };

    root_certs.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
        OwnedTrustAnchor::from_subject_spki_name_constraints(
            ta.subject,
            ta.spki,
            ta.name_constraints,
        )
    }));

    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_certs)
        .with_no_client_auth();

    Ok(Connector::Rustls(Arc::new(config)))
}

/// Function used by a device to connect via TLS to a host. It returns a WebSocket connection.
pub async fn connect(
    url: &Url,
    connector: Option<Connector>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, DeviceError> {
    let (ws_stream, _) = connect_async_tls_with_config(url, None, connector)
        .await
        .map_err(|err| {
            tracing::error!(?err);
            DeviceError::WebSocketConnect(err)
        })?;

    Ok(ws_stream)
}

/// TLS [`tower`] service.
///
/// This service add a [`TlsLayer`] on top of a tower stack.
///  It is responsible for the handling of a TLS connection.
#[derive(Clone)]
pub struct TlsService<S> {
    service: S,
    acceptor: TlsAcceptor,
}

impl<S, Request, E> Service<Request> for TlsService<S>
where
    S: Service<TlsStream<Request>, Error = E> + Clone + 'static,
    Request: AsyncWrite + AsyncRead + Unpin + 'static,
    HostError: From<E>,
{
    type Error = HostError;
    type Response = S::Response;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(HostError::from)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let clone = self.service.clone();
        // Acceptor is an Arc
        let acceptor = self.acceptor.clone();
        // take the service that was ready
        let mut inner = std::mem::replace(&mut self.service, clone);
        Box::pin(async move {
            let stream = acceptor.accept(req).await.map_err(HostError::AcceptTls)?;
            inner.call(stream).await.map_err(HostError::from)
        })
    }
}

/// TLS layer to be added in a [tower stack](tower::layer::util::Stack).
pub struct TlsLayer {
    acceptor: TlsAcceptor,
}

impl TlsLayer {
    /// Constructor for a [`TlsLayer`].
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
    /// Add the TlsLayer on top of the tower stack and listen to a connection from a device.
    pub async fn serve(self) -> Result<(), HostError> {
        let (mut server, builder) = self.fields();
        let service = builder.layer(WebSocketLayer).service(HostService);

        server.listen(service).await
    }
}
