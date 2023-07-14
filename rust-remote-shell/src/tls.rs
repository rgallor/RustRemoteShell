//! Module implementing TLS functionality.
//!
//! This module provides the necessary functionalities to add a TLS layer on top of the communication between a device and a host.
//! The [`host_tls_config`] and [`acceptor`] functions allow to set the TLS configuration for a
//! host and to provide the necessary TLS connector, necessary to establish a TLS connection with a device.
//! The [`device_tls_config`] and [`connect`] functions allow to set the TLS configuration for a
//! device and to establish a TLS connection with a host.
//!
//! The module also contains the [`TlsService`] and [`TlsLayer`] struct, necessary to implement a Tls layer in the [`tower`] stack.

use futures::future::Either;
use futures::{ready, Future, FutureExt};

use rustls_pemfile::{read_all, read_one, Item};
use std::io::BufReader;
use std::task::Poll;
use tokio_rustls::{webpki, Accept};
use tower::make::MakeConnection;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::{
    Certificate, ClientConfig, OwnedTrustAnchor, PrivateKey, RootCertStore,
};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tokio_tungstenite::{
    connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};
use tower::{Layer, Service};
use tracing::{debug, error, instrument};
use url::Url;

use crate::device::DeviceError;
use crate::host::HostError;
use crate::websocket::TcpAccept;

/// TLS errora
#[derive(Error, Debug)]
pub enum Error {
    /// Error while loading digital certificate or private key.
    #[error("Wrong item.")]
    WrongItem,

    /// Error while establishing a TLS connection.
    #[error("Error while establishing a TLS connection.")]
    RustTls(#[from] tokio_rustls::rustls::Error),

    /// Error while accepting a TLS connection.
    #[error("Error while accepting a TLS connection.")]
    AcceptTls(#[source] std::io::Error),

    /// Error while reading from file.
    #[error("Error while reading from file.")]
    ReadFile(#[source] std::io::Error),

    /// Failed to add CA cert to the root certs.
    #[error("Failed to add CA cert to the root certs.")]
    RootCert(#[from] webpki::Error),
}

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
        .map_err(Error::RustTls)?;

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
        _ => return Err(HostError::Tls(Error::WrongItem)),
    };

    let privkey_item = retrieve_item(&privkey_file).map_err(|err| HostError::ReadFile {
        err,
        file: privkey_file,
    })?;
    let privkey = match privkey_item {
        Some(Item::PKCS8Key(privkey)) => privkey,
        _ => return Err(HostError::Tls(Error::WrongItem)),
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
#[instrument(skip_all)]
pub fn device_tls_config(ca_cert_file: Option<PathBuf>) -> Result<Connector, DeviceError> {
    let mut root_certs = RootCertStore::empty();

    if let Some(ca_cert_file) = ca_cert_file {
        let file = std::fs::File::open(ca_cert_file).map_err(Error::ReadFile)?;
        let mut reader = BufReader::new(file);

        for item in read_all(&mut reader).map_err(Error::ReadFile)? {
            match item {
                Item::X509Certificate(ca_cert) => {
                    let cert = Certificate(ca_cert);
                    root_certs.add(&cert).map_err(Error::RootCert)?
                }
                _ => return Err(DeviceError::Tls(Error::WrongItem)),
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
#[instrument(skip_all)]
pub async fn connect(
    url: &Url,
    connector: Option<Connector>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, DeviceError> {
    let (ws_stream, _) = connect_async_tls_with_config(url, None, connector)
        .await
        .map_err(|err| {
            error!("error while connecting: {:?}", err);
            DeviceError::WebSocketConnect(err)
        })?;

    Ok(ws_stream)
}

/// TLS [`tower`] service.
///
/// This service add a [`TlsLayer`] on top of a tower stack.
/// It is responsible for the handling of a TLS connection.
#[derive(Clone)]
pub struct TlsService<S> {
    service: S, // TcpService
    acceptor: TlsAcceptor,
}

impl<'a, S> Service<&'a mut TcpListener> for TlsService<S>
where
    S: MakeConnection<&'a mut TcpListener, Connection = TcpStream, Future = TcpAccept<'a>>,
    HostError: From<S::Error>,
{
    type Error = HostError;
    type Response = TlsStream<S::Connection>;
    type Future = TlsAccept<'a>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(HostError::from)
    }

    fn call(&mut self, req: &'a mut TcpListener) -> Self::Future {
        TlsAccept {
            either: Either::Left(TcpAccept { listener: req }),
            acceptor: self.acceptor.clone(),
        }
    }
}

/// Future used to accept a Tls connection.
pub struct TlsAccept<'a> {
    // first accept a TCP connection and then create a TLS one over it.
    either: Either<TcpAccept<'a>, Accept<TcpStream>>,
    acceptor: TlsAcceptor,
}

impl<'a> Future for TlsAccept<'a> {
    type Output = Result<TlsStream<TcpStream>, HostError>;

    // this poll method is necessary to avoid incurring in a deadlock when locking
    #[instrument(skip_all)]
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match &mut self.either {
            Either::Left(tcp) => {
                let stream = match ready!(tcp.poll_unpin(cx)) {
                    Ok(tcp_stream) => tcp_stream,
                    Err(err) => return Poll::Ready(Err(err)),
                };

                let accept = self.acceptor.accept(stream);

                self.either = Either::Right(accept);

                // update context so that EitherRight is executed
                self.poll(cx)
            }
            Either::Right(tls) => {
                let res =
                    ready!(tls.poll_unpin(cx)).map_err(|err| HostError::Tls(Error::AcceptTls(err)));

                match res {
                    Ok(tls_stream) => Poll::Ready(Ok(tls_stream)),
                    Err(err) => Poll::Ready(Err(err)),
                }
            }
        }
    }
}

/// TLS layer to be added in a [tower stack](tower::layer::util::Stack).
pub struct TlsLayer {
    acceptor: TlsAcceptor,
}

impl TlsLayer {
    /// Constructor for a [`TlsLayer`].
    #[must_use]
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
